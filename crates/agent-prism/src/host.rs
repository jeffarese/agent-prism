//! The session host: dock model, hook and usage events, and account
//! switching (DESIGN §8, §9).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::SystemTime;

use prism_core::config::{Config, DockShow};
use prism_core::model::{
    AccountView, Countdown, DockMode, DockModel, LimitKind, MessageLevel, OnLimit, Policy,
    Projection, StatusMessage, Tool,
};
use prism_core::profile::Profile;
use prism_core::registry::{self, SessionRecord};
use prism_core::share;
use prism_term::runner::{Control, Host, HostEvent};
use prism_usage::balance::{PickResult, fmt_duration};
use prism_usage::cache::CachedUsage;
use prism_usage::poller::{PollTarget, PollerHandle, UsageUpdate};
use prism_usage::views::{self, AccountInput};
use serde_json::Value;

use crate::actions::UiAction;
use crate::app::AppCtx;
use crate::launch::{self, SpecBuilder, Start, unix_now};
use crate::live::{Live, PendingSwitch, SessionInfo};
use crate::tokens::KeychainTokens;

/// How long status messages stay.
const MESSAGE_SECS: i64 = 8;
/// A child that fails this soon after a switch triggers a switch back.
const SWITCH_GRACE_SECS: i64 = 5;
/// How often the config mtime and (without a poller) cache files are checked.
const CHECK_SECS: i64 = 2;
/// How often identities, tokens and the registry are re-read.
const INPUTS_SECS: i64 = 30;
/// How often a Codex session looks for its thread id in the database.
const CODEX_THREAD_SECS: i64 = 3;

/// Events from agent-prism's own threads into the runner.
#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// A Claude hook arrived on the control socket.
    Hook { event: String, payload: Value },
    /// The poller has new usage for a profile.
    Usage(UsageUpdate),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PendingKind {
    /// Countdown; switch at the deadline.
    Auto,
    /// Wait for the user (menu → switch now / ConfirmSwitch).
    Ask,
}

#[derive(Clone, Debug)]
struct Pending {
    target: String,
    kind: PendingKind,
    deadline: i64,
    reason: String,
    /// The limit stopped a turn (StopFailure), so the resumed session gets
    /// the resume prompt. A limit seen by polling while idle does not.
    interrupted: bool,
}

#[derive(Clone, Debug)]
struct Switched {
    previous: String,
    at: i64,
}

/// What the host needs at start.
pub struct HostInit {
    pub ctx: Arc<AppCtx>,
    pub live: Live,
    pub config: Config,
    pub tokens: Arc<KeychainTokens>,
    pub builder: SpecBuilder,
    pub model: Option<String>,
    pub policy: Option<Policy>,
    pub dock_mode: DockMode,
    /// The tool of this session; only its accounts are used.
    pub tool: Tool,
    pub active: String,
    pub session_id: Option<String>,
    pub poller: Option<PollerHandle>,
}

/// A Codex session finds its thread id in Codex's database: the newest
/// thread in the work dir since the child started (Codex has no
/// `--session-id` and no hooks without a trust prompt).
struct CodexWatch {
    /// Unix ms when the current child started.
    since_ms: i64,
    /// The user resumed an older thread (picker or name): match by the last
    /// update instead of the creation time.
    by_update: bool,
    last: i64,
    inflight: Option<Receiver<Option<String>>>,
}

/// Implements [`Host`] for one agent-prism session.
pub struct SessionHost {
    ctx: Arc<AppCtx>,
    live: Live,
    config: Config,
    config_mtime: Option<SystemTime>,
    profiles: Vec<Profile>,
    tokens: Arc<KeychainTokens>,
    inputs: Vec<AccountInput>,
    usage: HashMap<String, CachedUsage>,
    projections: HashMap<String, Projection>,
    builder: SpecBuilder,
    model: Option<String>,
    policy: Option<Policy>,
    dock_mode: DockMode,
    tool: Tool,
    codex: Option<CodexWatch>,
    active: String,
    since: i64,
    started_at: i64,
    session_id: Option<String>,
    busy: bool,
    last_failure: Option<String>,
    message: Option<(StatusMessage, i64)>,
    pending: Option<Pending>,
    limit_episode: bool,
    switched: Option<Switched>,
    poller: Option<PollerHandle>,
    last_check: i64,
    last_inputs: i64,
    last_draw: i64,
    /// Fixed clock for tests.
    test_now: Option<i64>,
}

fn mtime(p: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn window_name(kind: LimitKind) -> &'static str {
    match kind {
        LimitKind::FiveHour => "5h",
        LimitKind::SevenDay => "weekly",
        LimitKind::Scoped => "model",
    }
}

impl SessionHost {
    /// A host for a session that starts on `init.active`.
    pub fn new(init: HostInit) -> Self {
        let now = unix_now();
        let profiles = init.config.profiles_for(&init.ctx.paths, init.tool);
        let codex = (init.tool == Tool::Codex).then(|| CodexWatch {
            since_ms: now * 1000,
            by_update: crate::codex::subcommand(&init.builder.user_args)
                .is_some_and(|s| s == "resume")
                && init.session_id.is_none(),
            last: 0,
            inflight: None,
        });
        let config_mtime = mtime(&init.ctx.paths.config_file());
        let mut h = Self {
            config_mtime,
            profiles,
            inputs: Vec::new(),
            usage: HashMap::new(),
            projections: HashMap::new(),
            since: now,
            started_at: now,
            busy: false,
            last_failure: None,
            message: None,
            pending: None,
            limit_episode: false,
            switched: None,
            last_check: now,
            last_inputs: now,
            last_draw: 0,
            test_now: None,
            ctx: init.ctx,
            live: init.live,
            config: init.config,
            tokens: init.tokens,
            builder: init.builder,
            model: init.model,
            policy: init.policy,
            dock_mode: init.dock_mode,
            tool: init.tool,
            codex,
            active: init.active,
            session_id: init.session_id,
            poller: init.poller,
        };
        h.refresh_inputs();
        h.reload_usage_from_disk();
        h.update_live();
        h
    }

    fn now(&self) -> i64 {
        self.test_now.unwrap_or_else(unix_now)
    }

    fn label(&self, id: &str) -> String {
        self.profiles
            .iter()
            .find(|p| p.id == id)
            .map_or_else(|| id.to_string(), |p| p.label.clone())
    }

    fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    fn refresh_inputs(&mut self) {
        let now = self.now();
        self.inputs = launch::account_inputs(
            &self.ctx.paths,
            &self.profiles,
            &self.tokens,
            Some(&self.active),
            now,
        );
        self.last_inputs = now;
    }

    fn reload_usage_from_disk(&mut self) {
        self.usage = launch::load_usage(&self.ctx.cache, &self.profiles);
        self.projections = launch::projections(&self.ctx.cache, &self.usage, self.now());
    }

    fn view_ctx(&self, exclude: Option<String>) -> views::ViewCtx {
        launch::view_ctx(
            &self.config,
            self.policy,
            self.model.clone(),
            exclude,
            self.now(),
        )
    }

    fn inputs_now(&self) -> Vec<AccountInput> {
        let mut inputs = self.inputs.clone();
        for i in &mut inputs {
            i.active_here = i.profile_id == self.active;
        }
        inputs
    }

    /// Views of all profiles with the switch target marked.
    fn views(&self) -> Vec<AccountView> {
        let ctx = self.view_ctx(Some(self.active.clone()));
        views::build_views(&self.inputs_now(), &self.usage, &self.projections, &ctx)
    }

    fn pick_other(&self) -> PickResult {
        let ctx = self.view_ctx(Some(self.active.clone()));
        prism_usage::balance::pick(&self.inputs_now(), &self.usage, &ctx)
    }

    fn active_limit(&self) -> Option<LimitKind> {
        self.views()
            .into_iter()
            .find(|v| v.profile_id == self.active)
            .and_then(|v| v.limited.map(|l| l.kind))
    }

    fn set_message(&mut self, level: MessageLevel, text: impl Into<String>) {
        let text = text.into();
        match level {
            MessageLevel::Error => log::error!("{text}"),
            MessageLevel::Warn => log::warn!("{text}"),
            MessageLevel::Info => log::info!("{text}"),
        }
        let until = self.now() + MESSAGE_SECS;
        self.message = Some((StatusMessage { level, text }, until));
    }

    fn can_resume(&self, id: &str) -> bool {
        self.profile(id).is_some_and(|p| {
            share::share_health(&p.dir, &p.common_home, &self.config.share_rules(p.tool))
                .is_ok_and(|h| h.sessions_shared)
        })
    }

    /// Writes this session's registry record.
    pub fn write_registry(&self) {
        let rec = SessionRecord {
            pid: std::process::id(),
            profile: self.active.clone(),
            session_id: self.session_id.clone(),
            cwd: self.builder.cwd.clone(),
            started_at: self.started_at,
            switched_at: (self.since != self.started_at).then_some(self.since),
        };
        if let Err(e) = registry::write_record(&self.ctx.paths, &rec) {
            log::warn!("session registry: {e:#}");
        }
    }

    fn poll_targets(&self) -> Vec<PollTarget> {
        self.profiles
            .iter()
            .map(|p| PollTarget {
                profile_id: p.id.clone(),
                active: p.id == self.active,
            })
            .collect()
    }

    fn refresh_usage(&mut self, id: Option<String>) {
        match &self.poller {
            Some(p) => p.refresh_now(id),
            None => self.reload_usage_from_disk(),
        }
    }

    fn update_live(&self) {
        let session = SessionInfo {
            tool: self.tool,
            profile_id: self.active.clone(),
            session_id: self.session_id.clone(),
            busy: self.busy,
            can_switch: self.session_id.is_some() && self.can_resume(&self.active),
            pending: None,
        };
        let views = self.views();
        let pending = self.pending.as_ref().map(|p| PendingSwitch {
            target_id: p.target.clone(),
            target_label: views
                .iter()
                .find(|v| v.profile_id == p.target)
                .map_or_else(|| p.target.clone(), |v| v.label.clone()),
            reason: p.reason.clone(),
        });
        let session = SessionInfo { pending, ..session };
        let pick = self.pick_other();
        let mut d = self.live.lock();
        d.config = self.config.clone();
        d.views = views;
        d.pick = Some(pick);
        d.session = Some(session);
        d.log = crate::logging::recent();
    }

    fn reload_config(&mut self, ctl: &mut Control<UiAction>) {
        self.config_mtime = mtime(&self.ctx.paths.config_file());
        // A file that disappears mid-session (moved, deleted, half-saved)
        // must not drop the accounts: with no profiles, polling and limit
        // switching silently stop.
        let config = match Config::load(&self.ctx.paths) {
            Ok(Some(c)) => c,
            Ok(None) => {
                self.set_message(
                    MessageLevel::Warn,
                    "config file missing; keeping the loaded accounts",
                );
                return;
            }
            Err(e) => {
                self.set_message(MessageLevel::Error, format!("config: {e:#}"));
                return;
            }
        };
        if config.dock.mode != self.config.dock.mode {
            self.dock_mode = config.dock.mode;
            ctl.set_dock_mode(self.dock_mode);
        }
        self.config = config;
        self.profiles = self.config.profiles_for(&self.ctx.paths, self.tool);
        self.tokens.set_profiles(self.profiles.clone());
        self.refresh_inputs();
        if let Some(p) = &self.poller {
            p.set_targets(self.poll_targets());
        }
        self.reload_usage_from_disk();
        log::info!("config reloaded");
        ctl.redraw();
    }

    /// A limit stopped the active account: start the switch flow (§9).
    fn on_limit(&mut self, kind: Option<LimitKind>, interrupted: bool) {
        if self.pending.is_some() {
            return;
        }
        self.limit_episode = true;
        let label = self.label(&self.active);
        let what = kind.map_or("usage", window_name);
        let pick = self.pick_other();
        let Some(target) = pick.chosen else {
            let now = self.now();
            let text = match pick.earliest_reset {
                Some((l, t)) => {
                    format!(
                        "All accounts limited · {l} resets in {}",
                        fmt_duration(t - now)
                    )
                }
                None => "All accounts limited".to_string(),
            };
            self.set_message(MessageLevel::Error, text);
            return;
        };
        let reason = format!("hit the {what} limit");
        match self.config.balance.on_limit {
            OnLimit::Auto => {
                log::info!("{label} {reason}; countdown to {target}");
                self.pending = Some(Pending {
                    target,
                    kind: PendingKind::Auto,
                    deadline: self.now() + i64::from(self.config.balance.countdown_secs),
                    reason,
                    interrupted,
                });
            }
            OnLimit::Ask => {
                self.pending = Some(Pending {
                    target,
                    kind: PendingKind::Ask,
                    deadline: 0,
                    reason: reason.clone(),
                    interrupted,
                });
                self.set_message(
                    MessageLevel::Warn,
                    format!("{label} {reason} · ⌃\\ to choose"),
                );
            }
            OnLimit::Stay => self.set_message(MessageLevel::Warn, format!("{label} {reason}")),
        }
    }

    fn cancel_pending(&mut self) {
        if self.pending.take().is_some() {
            let label = self.label(&self.active);
            self.set_message(MessageLevel::Info, format!("Staying on {label}"));
        }
    }

    /// Moves the session to `target` (§9 steps 4–5). Returns false when
    /// refused (a message says why).
    /// `interrupted`: a limit stopped the last turn, so send the resume prompt.
    fn perform_switch(
        &mut self,
        target: &str,
        interrupted: bool,
        ctl: &mut Control<UiAction>,
    ) -> bool {
        self.pending = None;
        if target == self.active {
            return false;
        }
        let Some(sid) = self.session_id.clone() else {
            self.set_message(MessageLevel::Error, "No session id yet; cannot switch");
            return false;
        };
        for id in [self.active.clone(), target.to_string()] {
            if !self.can_resume(&id) {
                self.set_message(
                    MessageLevel::Error,
                    format!("run agp adopt {id} to share sessions"),
                );
                return false;
            }
        }
        let Some(profile) = self.profile(target).cloned() else {
            self.set_message(MessageLevel::Error, format!("unknown profile {target}"));
            return false;
        };
        let prompt = (interrupted && !self.config.balance.resume_prompt.is_empty())
            .then(|| self.config.balance.resume_prompt.clone());
        let start = Start::Resume {
            session_id: sid.clone(),
            prompt,
        };
        let spec = match self.builder.build(&self.config, &profile, &start) {
            Ok((spec, _)) => spec,
            Err(e) => {
                self.set_message(MessageLevel::Error, format!("switch failed: {e:#}"));
                return false;
            }
        };
        ctl.restart_child(spec);
        let now = self.now();
        self.restart_codex_watch(now);
        self.switched = Some(Switched {
            previous: std::mem::replace(&mut self.active, target.to_string()),
            at: now,
        });
        self.since = now;
        self.busy = false;
        // The new account is not limited; a later limit is a new episode.
        self.limit_episode = false;
        self.write_registry();
        launch::touch_last_used(&self.ctx.paths, target, now);
        if let Some(p) = &self.poller {
            p.set_targets(self.poll_targets());
        }
        let short: String = sid.chars().take(8).collect();
        self.set_message(
            MessageLevel::Info,
            format!("Moved to {} · resumed {short}", profile.label),
        );
        self.refresh_inputs();
        ctl.redraw();
        true
    }

    fn on_child_exit(&mut self, code: i32, ctl: &mut Control<UiAction>) {
        let now = self.now();
        let recent = self
            .switched
            .take()
            .filter(|s| code != 0 && now - s.at <= SWITCH_GRACE_SECS);
        let (Some(sw), Some(sid)) = (recent, self.session_id.clone()) else {
            ctl.quit(code);
            return;
        };
        let failed = self.label(&self.active);
        let Some(prev) = self.profile(&sw.previous).cloned() else {
            ctl.quit(code);
            return;
        };
        let start = Start::Resume {
            session_id: sid,
            prompt: None,
        };
        match self.builder.build(&self.config, &prev, &start) {
            Ok((spec, _)) => {
                ctl.restart_child(spec);
                self.restart_codex_watch(now);
                self.active = prev.id.clone();
                self.since = now;
                self.write_registry();
                if let Some(p) = &self.poller {
                    p.set_targets(self.poll_targets());
                }
                self.set_message(
                    MessageLevel::Error,
                    format!(
                        "{} on {failed} exited ({code}); back on {}",
                        self.tool.bin(),
                        prev.label
                    ),
                );
                ctl.redraw();
            }
            Err(e) => {
                log::error!("switch back failed: {e:#}");
                ctl.quit(code);
            }
        }
    }

    fn on_hook(&mut self, event: &str, payload: &Value, ctl: &mut Control<UiAction>) {
        match event {
            "SessionStart" => {
                if let Some(id) = payload.get("session_id").and_then(Value::as_str) {
                    self.session_id = Some(id.to_string());
                    self.write_registry();
                }
                let key = self
                    .builder
                    .memory_key
                    .lock()
                    .ok()
                    .and_then(|mut k| k.take());
                if let (Some(key), Some(t)) =
                    (key, payload.get("transcript_path").and_then(Value::as_str))
                {
                    launch::check_memory_key(t, &key);
                }
            }
            "UserPromptSubmit" => self.busy = true,
            "Stop" => {
                self.busy = false;
                self.check_active_limit();
            }
            "SessionEnd" => self.busy = false,
            "StopFailure" => {
                self.busy = false;
                let err = payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                log::info!("StopFailure: {err}");
                match err.as_str() {
                    "rate_limit" => {
                        self.refresh_usage(Some(self.active.clone()));
                        let kind = self.active_limit();
                        self.on_limit(kind, true);
                    }
                    "authentication_failed" => {
                        let label = self.label(&self.active);
                        self.set_message(
                            MessageLevel::Warn,
                            format!("{label}: login expired — agp add / claude /login"),
                        );
                    }
                    _ => {}
                }
                self.last_failure = Some(err);
            }
            _ => {}
        }
        self.update_live();
        ctl.redraw();
    }

    fn on_usage(&mut self, u: UsageUpdate, ctl: &mut Control<UiAction>) {
        let id = u.profile_id.clone();
        self.usage.insert(id.clone(), u.cached);
        self.projections = launch::projections(&self.ctx.cache, &self.usage, self.now());
        if id == self.active {
            self.check_active_limit();
        }
        self.update_live();
        ctl.redraw();
    }

    /// A limit seen by polling. While Claude works on a turn, wait: the turn
    /// either ends (`Stop` checks again) or fails (`StopFailure` switches).
    fn check_active_limit(&mut self) {
        if self.busy {
            return;
        }
        match self.active_limit() {
            Some(kind) if !self.limit_episode && self.pending.is_none() => {
                self.on_limit(Some(kind), false);
            }
            Some(_) => {}
            None => self.limit_episode = false,
        }
    }

    /// A new child runs from `now` on: threads it creates are newer.
    fn restart_codex_watch(&mut self, now: i64) {
        if let Some(w) = &mut self.codex {
            w.since_ms = now * 1000;
            w.by_update = false;
            w.inflight = None;
        }
    }

    /// Looks for the Codex thread id now and then, in a thread, and takes a
    /// new id when Codex starts a new thread (e.g. `/new`).
    fn poll_codex_thread(&mut self, now: i64) {
        let Some(w) = &mut self.codex else { return };
        if let Some(rx) = &w.inflight {
            match rx.try_recv() {
                Ok(found) => {
                    w.inflight = None;
                    if let Some(id) = found
                        && self.session_id.as_deref() != Some(id.as_str())
                    {
                        log::info!("codex thread {id}");
                        self.session_id = Some(id);
                        self.write_registry();
                        self.update_live();
                    }
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => w.inflight = None,
            }
        }
        let Some(w) = &mut self.codex else { return };
        if now - w.last < CODEX_THREAD_SECS {
            return;
        }
        w.last = now;
        let Some(profile) = self.profiles.iter().find(|p| p.id == self.active) else {
            return;
        };
        let home = launch::codex_sqlite_home(profile, |k| std::env::var(k).ok());
        let Some(db) = crate::codex::state_db(&home) else {
            return;
        };
        let account = self
            .tokens
            .credentials(&profile.id)
            .ok()
            .flatten()
            .and_then(|c| c.account_id);
        let cwd = crate::codex::work_dir(&self.builder.user_args, &self.builder.cwd);
        let (since_ms, by_update) = (w.since_ms, w.by_update);
        let (tx, rx) = std::sync::mpsc::channel();
        w.inflight = Some(rx);
        std::thread::spawn(move || {
            let found = if by_update {
                crate::codex::find_updated_thread(&db, &cwd, since_ms)
            } else {
                crate::codex::find_thread(&db, &cwd, since_ms, account.as_deref())
            };
            let _ = tx.send(found);
        });
    }

    fn on_tick(&mut self, ctl: &mut Control<UiAction>) {
        let now = self.now();
        self.poll_codex_thread(now);
        if self
            .message
            .as_ref()
            .is_some_and(|(_, until)| *until <= now)
        {
            self.message = None;
            ctl.redraw();
        }
        if let Some(p) = &self.pending
            && p.kind == PendingKind::Auto
            && now >= p.deadline
        {
            let (target, interrupted) = (p.target.clone(), p.interrupted);
            self.perform_switch(&target, interrupted, ctl);
        }
        if now - self.last_check >= CHECK_SECS {
            self.last_check = now;
            if mtime(&self.ctx.paths.config_file()) != self.config_mtime {
                self.reload_config(ctl);
            }
            if self.poller.is_none() {
                self.reload_usage_from_disk();
                self.check_active_limit();
            }
        }
        if now - self.last_inputs >= INPUTS_SECS {
            self.refresh_inputs();
        }
        if now != self.last_draw {
            self.last_draw = now;
            self.update_live();
            ctl.redraw();
        }
    }

    fn open_menu_or_cancel(&mut self, ctl: &mut Control<UiAction>) {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.kind == PendingKind::Auto)
        {
            self.cancel_pending();
            ctl.redraw();
            return;
        }
        self.update_live();
        ctl.open_panel(crate::ui::menu_panel(self.ctx.clone(), self.live.clone()));
    }

    fn on_action(&mut self, a: UiAction, ctl: &mut Control<UiAction>) {
        match a {
            UiAction::SwitchTo(id) => {
                ctl.close_panel();
                let target = match id {
                    Some(id) => Some(id),
                    None => self.pick_other().chosen,
                };
                match target {
                    Some(t) => {
                        self.perform_switch(&t, false, ctl);
                    }
                    None => self.set_message(MessageLevel::Error, "No other account to switch to"),
                }
            }
            UiAction::ConfirmSwitch => {
                ctl.close_panel();
                if let Some(p) = self.pending.clone() {
                    self.perform_switch(&p.target, p.interrupted, ctl);
                }
            }
            UiAction::CancelSwitch => {
                ctl.close_panel();
                self.cancel_pending();
            }
            UiAction::SetDockMode(m) => {
                self.dock_mode = m;
                ctl.set_dock_mode(m);
            }
            UiAction::RefreshUsage(id) => self.refresh_usage(id),
            UiAction::ConfigChanged => self.reload_config(ctl),
        }
        self.update_live();
        ctl.redraw();
    }
}

impl Host<SessionEvent, UiAction> for SessionHost {
    fn dock_model(&mut self) -> DockModel {
        let now = self.now();
        let views = self.views();
        let show_all = self.config.dock.show == DockShow::All;
        let accounts: Vec<AccountView> = views
            .into_iter()
            .filter(|v| show_all || v.in_pool || v.profile_id == self.active)
            .collect();
        let newest = accounts
            .iter()
            .filter_map(|v| v.usage.as_ref().map(|u| u.fetched_at))
            .max();
        let countdown = self.pending.as_ref().and_then(|p| {
            (p.kind == PendingKind::Auto).then(|| Countdown {
                from_label: self.label(&self.active),
                to_label: self.label(&p.target),
                reason: p.reason.clone(),
                deadline: p.deadline,
            })
        });
        DockModel {
            accounts,
            mode: self.dock_mode,
            policy: self.policy.unwrap_or(self.config.balance.policy),
            session_short: self
                .session_id
                .as_ref()
                .map(|s| s.chars().take(8).collect()),
            on_account: Some((self.label(&self.active), self.since)),
            usage_age_secs: newest.map(|t| (now - t).max(0)),
            message: self.message.as_ref().map(|(m, _)| m.clone()),
            countdown,
            menu_key_label: "⌃\\".into(),
            show_projections: self.config.dock.projections,
            icons: self.config.dock.icons,
            now,
        }
    }

    fn on_event(&mut self, event: HostEvent<SessionEvent, UiAction>, ctl: &mut Control<UiAction>) {
        match event {
            HostEvent::Custom(SessionEvent::Hook { event, payload }) => {
                self.on_hook(&event, &payload, ctl);
            }
            HostEvent::Custom(SessionEvent::Usage(u)) => self.on_usage(u, ctl),
            HostEvent::Tick => self.on_tick(ctl),
            HostEvent::MenuKey | HostEvent::DockClick { .. } => self.open_menu_or_cancel(ctl),
            HostEvent::PanelAction(a) => self.on_action(a, ctl),
            HostEvent::ChildExited { code } => self.on_child_exit(code, ctl),
            HostEvent::Resized { .. } => ctl.redraw(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_core::config::ProfileConfig;
    use prism_core::model::{UsageSnapshot, Window};
    use prism_core::paths::Paths;
    use prism_core::profile::keychain_service_for;
    use prism_usage::cache::UsageCache;

    struct Fixture {
        _dir: tempfile::TempDir,
        host: SessionHost,
        b_dir: std::path::PathBuf,
    }

    fn snap(u5h: f64, now: i64) -> CachedUsage {
        CachedUsage {
            snapshot: Some(UsageSnapshot {
                fetched_at: now,
                five_hour: Some(Window {
                    utilization: u5h,
                    resets_at: Some(now + 3600),
                    inferred: false,
                }),
                seven_day: Some(Window {
                    utilization: 10.0,
                    resets_at: Some(now + 86_400),
                    inferred: false,
                }),
                scoped: vec![],
                extra_usage_enabled: false,
                limit_reached: false,
            }),
            last_attempt: now,
            ..Default::default()
        }
    }

    /// Two profiles `a` (5h at `ua`) and `b` (5h at `ub`); session on `a`.
    fn fixture(ua: f64, ub: f64, share_b: bool, on_limit: OnLimit) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = Paths::under(root);
        let common = root.join("common");
        std::fs::create_dir_all(common.join("projects")).unwrap();
        let mut config = Config {
            common_home: common.to_string_lossy().into(),
            ..Default::default()
        };
        config.balance.on_limit = on_limit;
        let mut store = serde_json::Map::new();
        let exp = (unix_now() + 7200) * 1000;
        for (id, share) in [("a", true), ("b", share_b)] {
            let pd = root.join(format!("p-{id}"));
            std::fs::create_dir_all(&pd).unwrap();
            if share {
                std::os::unix::fs::symlink(common.join("projects"), pd.join("projects")).unwrap();
            }
            config.profiles.push(ProfileConfig {
                id: id.into(),
                label: format!("Acct {}", id.to_uppercase()),
                dir: pd.to_string_lossy().into(),
                ..Default::default()
            });
            store.insert(
                keychain_service_for(&pd, false),
                Value::String(format!(
                    r#"{{"claudeAiOauth":{{"accessToken":"t","expiresAt":{exp},"subscriptionType":"team"}}}}"#
                )),
            );
        }
        let store_file = root.join("store.json");
        std::fs::write(&store_file, Value::Object(store).to_string()).unwrap();
        let cache = UsageCache::new(paths.usage_dir());
        let now = unix_now();
        cache.store("a", &snap(ua, now)).unwrap();
        cache.store("b", &snap(ub, now)).unwrap();
        let ctx = Arc::new(AppCtx {
            paths: paths.clone(),
            store: Arc::new(crate::tokens::FileStore::new(store_file)),
            cache,
        });
        let profiles = config.profiles(&paths);
        let tokens = Arc::new(KeychainTokens::new(ctx.store.clone(), profiles));
        let builder = SpecBuilder {
            paths: paths.clone(),
            tool: Tool::Claude,
            program: "/bin/claude".into(),
            user_args: vec!["--model".into(), "opus".into(), "fix it".into()],
            user_settings: None,
            run_dir: root.join("run"),
            exe: "/bin/agp".into(),
            cwd: root.to_path_buf(),
            memory_key: Default::default(),
        };
        let host = SessionHost::new(HostInit {
            ctx,
            live: Live::default(),
            config,
            tokens,
            builder,
            model: None,
            policy: None,
            dock_mode: DockMode::Full,
            tool: Tool::Claude,
            active: "a".into(),
            session_id: None,
            poller: None,
        });
        Fixture {
            b_dir: root.join("p-b"),
            _dir: dir,
            host,
        }
    }

    fn hook(h: &mut SessionHost, ev: &str, payload: Value) -> Control<UiAction> {
        let mut ctl = Control::default();
        h.on_event(
            HostEvent::Custom(SessionEvent::Hook {
                event: ev.into(),
                payload,
            }),
            &mut ctl,
        );
        ctl
    }

    fn args(spec: &prism_term::pty::ChildSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|a| a.to_string_lossy().into())
            .collect()
    }

    fn env<'a>(spec: &'a prism_term::pty::ChildSpec, k: &str) -> Option<&'a std::ffi::OsStr> {
        spec.env_set
            .iter()
            .find(|(a, _)| a == k)
            .map(|(_, b)| b.as_os_str())
    }

    #[test]
    fn session_start_and_busy() {
        let mut f = fixture(10.0, 10.0, true, OnLimit::Auto);
        hook(
            &mut f.host,
            "SessionStart",
            serde_json::json!({"session_id":"abcdef123456"}),
        );
        assert_eq!(f.host.session_id.as_deref(), Some("abcdef123456"));
        assert_eq!(
            f.host.dock_model().session_short.as_deref(),
            Some("abcdef12")
        );
        hook(&mut f.host, "UserPromptSubmit", Value::Null);
        assert!(f.host.busy);
        hook(&mut f.host, "Stop", Value::Null);
        assert!(!f.host.busy);
        let live = f.host.live.lock().clone();
        assert!(live.session.unwrap().can_switch);
        assert_eq!(live.views.len(), 2);
    }

    #[test]
    fn rate_limit_countdown_then_switch() {
        let mut f = fixture(100.0, 5.0, true, OnLimit::Auto);
        hook(
            &mut f.host,
            "SessionStart",
            serde_json::json!({"session_id":"sess-1234567"}),
        );
        let mut ctl = hook(
            &mut f.host,
            "StopFailure",
            serde_json::json!({"error":"rate_limit"}),
        );
        assert!(ctl.take_restart().is_none());
        let m = f.host.dock_model();
        let cd = m.countdown.expect("countdown");
        assert_eq!(cd.to_label, "Acct B");
        assert_eq!(cd.reason, "hit the 5h limit");
        // Before the deadline nothing happens.
        let mut ctl = Control::default();
        f.host.test_now = Some(cd.deadline - 1);
        f.host.on_event(HostEvent::Tick, &mut ctl);
        assert!(ctl.take_restart().is_none());
        // Past the deadline: restart on b with --resume and the prompt.
        f.host.test_now = Some(cd.deadline + 1);
        f.host.on_event(HostEvent::Tick, &mut ctl);
        let spec = ctl.take_restart().expect("restart");
        let a = args(&spec);
        assert_eq!(a[..2], ["--model", "opus"]);
        assert!(!a.contains(&"fix it".to_string()));
        assert_eq!(a[a.len() - 3..], ["--resume", "sess-1234567", "continue"]);
        assert_eq!(env(&spec, "CLAUDE_CONFIG_DIR"), Some(f.b_dir.as_os_str()));
        assert_eq!(env(&spec, "AGP_PROFILE").unwrap(), "b");
        assert_eq!(env(&spec, "AGP_LABEL").unwrap(), "Acct B");
        assert_eq!(f.host.active, "b");
        let msg = f.host.dock_model().message.unwrap();
        assert_eq!(msg.text, "Moved to Acct B · resumed sess-123");
        // A quick non-zero exit goes back to a.
        let mut ctl = Control::default();
        f.host
            .on_event(HostEvent::ChildExited { code: 1 }, &mut ctl);
        let spec = ctl.take_restart().expect("switch back");
        assert_eq!(env(&spec, "AGP_PROFILE").unwrap(), "a");
        assert_eq!(ctl.quit_code(), None);
        // A later exit quits.
        let mut ctl = Control::default();
        f.host
            .on_event(HostEvent::ChildExited { code: 0 }, &mut ctl);
        assert_eq!(ctl.quit_code(), Some(0));
    }

    /// The config file goes away mid-session (e.g. a rename moved it):
    /// the accounts stay, so a limit still switches.
    #[test]
    fn missing_config_keeps_accounts() {
        let mut f = fixture(100.0, 5.0, true, OnLimit::Auto);
        assert!(!f.host.ctx.paths.config_file().exists());
        let mut ctl = Control::default();
        f.host.on_action(UiAction::ConfigChanged, &mut ctl);
        assert_eq!(f.host.profiles.len(), 2);
        hook(
            &mut f.host,
            "SessionStart",
            serde_json::json!({"session_id":"sess-1234567"}),
        );
        hook(
            &mut f.host,
            "StopFailure",
            serde_json::json!({"error":"rate_limit"}),
        );
        let cd = f.host.dock_model().countdown.expect("countdown");
        assert_eq!(cd.to_label, "Acct B");
    }

    #[test]
    fn polled_limit_waits_for_idle_and_sends_no_prompt() {
        let mut f = fixture(100.0, 5.0, true, OnLimit::Auto);
        hook(
            &mut f.host,
            "SessionStart",
            serde_json::json!({"session_id":"sess-1"}),
        );
        hook(&mut f.host, "UserPromptSubmit", serde_json::json!({}));
        // Busy: a limit seen by polling must not start a countdown.
        f.host.check_active_limit();
        assert!(f.host.dock_model().countdown.is_none());
        // The turn ends normally: now the countdown starts.
        hook(&mut f.host, "Stop", serde_json::json!({}));
        let cd = f.host.dock_model().countdown.expect("countdown");
        let mut ctl = Control::default();
        f.host.test_now = Some(cd.deadline + 1);
        f.host.on_event(HostEvent::Tick, &mut ctl);
        let a = args(&ctl.take_restart().expect("restart"));
        // No turn was interrupted, so no resume prompt.
        assert_eq!(a[a.len() - 2..], ["--resume", "sess-1"]);
    }

    #[test]
    fn menu_key_cancels_countdown() {
        let mut f = fixture(100.0, 5.0, true, OnLimit::Auto);
        hook(
            &mut f.host,
            "SessionStart",
            serde_json::json!({"session_id":"s"}),
        );
        hook(
            &mut f.host,
            "StopFailure",
            serde_json::json!({"error":"rate_limit"}),
        );
        let mut ctl = Control::default();
        f.host.on_event(HostEvent::MenuKey, &mut ctl);
        assert!(!ctl.panel_requested());
        let m = f.host.dock_model();
        assert!(m.countdown.is_none());
        assert_eq!(m.message.unwrap().text, "Staying on Acct A");
    }

    #[test]
    fn switch_refused_without_shared_projects() {
        let mut f = fixture(100.0, 5.0, false, OnLimit::Auto);
        hook(
            &mut f.host,
            "SessionStart",
            serde_json::json!({"session_id":"s"}),
        );
        let mut ctl = Control::default();
        f.host.on_event(
            HostEvent::PanelAction(UiAction::SwitchTo(Some("b".into()))),
            &mut ctl,
        );
        assert!(ctl.take_restart().is_none());
        assert_eq!(f.host.active, "a");
        assert_eq!(
            f.host.dock_model().message.unwrap().text,
            "run agp adopt b to share sessions"
        );
    }

    #[test]
    fn all_limited_message() {
        let mut f = fixture(100.0, 100.0, true, OnLimit::Auto);
        hook(
            &mut f.host,
            "SessionStart",
            serde_json::json!({"session_id":"s"}),
        );
        hook(
            &mut f.host,
            "StopFailure",
            serde_json::json!({"error":"rate_limit"}),
        );
        let m = f.host.dock_model();
        assert!(m.countdown.is_none());
        let text = m.message.unwrap().text;
        assert!(
            text.starts_with("All accounts limited · Acct B resets in"),
            "{text}"
        );
    }

    #[test]
    fn ask_and_confirm() {
        let mut f = fixture(100.0, 5.0, true, OnLimit::Ask);
        hook(
            &mut f.host,
            "SessionStart",
            serde_json::json!({"session_id":"s"}),
        );
        hook(
            &mut f.host,
            "StopFailure",
            serde_json::json!({"error":"rate_limit"}),
        );
        let m = f.host.dock_model();
        assert!(m.countdown.is_none());
        assert_eq!(
            m.message.unwrap().text,
            "Acct A hit the 5h limit · ⌃\\ to choose"
        );
        let mut ctl = Control::default();
        f.host
            .on_event(HostEvent::PanelAction(UiAction::ConfirmSwitch), &mut ctl);
        assert!(ctl.take_restart().is_some());
        assert_eq!(f.host.active, "b");
    }

    #[test]
    fn dock_mode_action_and_auth_failure() {
        let mut f = fixture(10.0, 10.0, true, OnLimit::Auto);
        let mut ctl = Control::default();
        f.host.on_event(
            HostEvent::PanelAction(UiAction::SetDockMode(DockMode::Compact)),
            &mut ctl,
        );
        assert_eq!(ctl.dock_mode_requested(), Some(DockMode::Compact));
        hook(
            &mut f.host,
            "StopFailure",
            serde_json::json!({"error":"authentication_failed"}),
        );
        let msg = f.host.dock_model().message.unwrap();
        assert_eq!(msg.level, MessageLevel::Warn);
        assert!(msg.text.starts_with("Acct A: login expired"));
    }
}
