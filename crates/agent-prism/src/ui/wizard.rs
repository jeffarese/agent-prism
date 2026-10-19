//! The onboarding wizard (DESIGN §10): Welcome · Accounts · Sharing ·
//! Balancing · Dock · Done.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use prism_core::config::{Config, ProfileConfig};
use prism_core::discover::{DirKind, discover};
use prism_core::keychain::{read_credentials, token_state};
use prism_core::model::{
    AccountIdentity, AccountView, DockMode, OnLimit, Policy, TokenState, Tool,
};
use prism_core::profile::Profile;
use prism_core::share::{ShareHealth, share_health};
use prism_term::dock::render_dock;
use prism_term::panel::{Panel, PanelOutcome};
use prism_usage::cache::CachedUsage;
use prism_usage::poller::{UreqFetcher, UsageFetcher};
use prism_usage::views::{AccountInput, ViewCtx, build_views};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::adopt::{AdoptView, FlowOutcome, plan_for, plan_summary, tilde};
use super::data::{TextField, balance_settings, dock_model, now};
use super::login::{LoginCommand, LoginView};
use super::menu::mode_name;
use super::settings::unique_id;
use super::theme as t;
use crate::actions::UiAction;
use crate::app::AppCtx;

const STEPS: [&str; 6] = [
    "Welcome",
    "Accounts",
    "Sharing",
    "Balancing",
    "Dock",
    "Done",
];

const POLICIES: [(Policy, &str, &str); 4] = [
    (
        Policy::Smart,
        "Smart",
        "Recommended. Balances what is left now and this week.",
    ),
    (
        Policy::Priority,
        "Priority",
        "Use the first account until it is full, then the next.",
    ),
    (
        Policy::RoundRobin,
        "Round-robin",
        "Take turns between accounts.",
    ),
    (
        Policy::Headroom,
        "Headroom",
        "Always the account with the most room right now.",
    ),
];

const ON_LIMIT: [(OnLimit, &str, &str); 3] = [
    (
        OnLimit::Auto,
        "Move on",
        "Switch after a 5-second countdown. The chat continues.",
    ),
    (
        OnLimit::Ask,
        "Ask me",
        "Show a prompt and wait for my answer.",
    ),
    (
        OnLimit::Stay,
        "Stay",
        "Only tell me; I switch from the menu.",
    ),
];

const MODES: [(DockMode, &str); 4] = [
    (DockMode::Auto, "full on tall terminals, one line otherwise"),
    (DockMode::Full, "a row with bars for each account"),
    (DockMode::Compact, "one quiet line"),
    (DockMode::Hidden, "nothing — the menu key still works"),
];

/// One account row of step 2.
#[derive(Clone, Debug)]
struct Acct {
    dir: PathBuf,
    tool: Tool,
    /// The common home of the tool (`~/.claude` or `~/.codex`).
    common: PathBuf,
    is_default: bool,
    kind: DirKind,
    identity: AccountIdentity,
    id: String,
    label: String,
    pool: bool,
    health: Option<Result<(ShareHealth, String), String>>,
}

impl Acct {
    fn profile(&self) -> Profile {
        let mut p = Profile::bare(&self.id, self.tool, self.dir.clone(), self.common.clone());
        p.label = self.label.clone();
        p.is_default_install = self.is_default;
        p.pool = self.pool;
        p
    }

    fn logged_in(&self) -> bool {
        self.kind == DirKind::LoggedIn
    }
}

enum Overlay {
    None,
    Login(Box<LoginView>),
    Adopt(Box<AdoptView>),
    EditLabel(usize, TextField),
    EditBackup(TextField),
    Quit,
}

type FetchResult = Vec<(String, CachedUsage)>;

enum Usage {
    Idle,
    Running(Receiver<FetchResult>),
    Done(Vec<AccountView>),
}

/// The onboarding wizard as a full-screen panel.
pub struct Wizard {
    ctx: Arc<AppCtx>,
    step: usize,
    base: Config,
    accts: Vec<Acct>,
    sel: [usize; 6],
    overlay: Overlay,
    usage: Usage,
    tick: usize,
    message: Option<(String, bool)>,
    /// The saved config once the user finished.
    pub result: Option<Config>,
    /// Login command override (tests).
    pub login_cmd: Option<LoginCommand>,
    /// Fetch real usage in step 5 (off in tests).
    pub fetch_usage: bool,
}

impl Wizard {
    /// A wizard starting from the existing config (or the defaults).
    pub fn new(ctx: Arc<AppCtx>) -> Self {
        let base = ctx.load_config().unwrap_or_default();
        let mut w = Self {
            ctx,
            step: 0,
            base,
            accts: Vec::new(),
            sel: [0; 6],
            overlay: Overlay::None,
            usage: Usage::Idle,
            tick: 0,
            message: None,
            result: None,
            login_cmd: None,
            fetch_usage: !crate::launch::polling_disabled(),
        };
        w.rediscover();
        w
    }

    /// Scan the home dir again, keeping choices already made.
    fn rediscover(&mut self) {
        let existing = self.base.profiles(&self.ctx.paths);
        for d in discover(&self.ctx.paths, &self.base, self.ctx.store.as_ref()) {
            if d.kind == DirKind::Proxy {
                continue;
            }
            if let Some(a) = self.accts.iter_mut().find(|a| a.dir == d.dir) {
                if a.kind != DirKind::LoggedIn {
                    a.kind = d.kind;
                }
                if d.identity.email.is_some() {
                    a.identity = d.identity;
                }
                continue;
            }
            let known = existing.iter().find(|p| p.dir == d.dir);
            let (id, label, pool) = match known {
                Some(p) => (p.id.clone(), p.label.clone(), p.pool),
                None => (
                    d.suggested_id.clone(),
                    d.suggested_label.clone(),
                    d.kind == DirKind::LoggedIn && !d.is_default_install,
                ),
            };
            self.accts.push(Acct {
                dir: d.dir,
                tool: d.tool,
                common: self.base.common_home_for(&self.ctx.paths, d.tool),
                is_default: d.is_default_install,
                kind: d.kind,
                identity: d.identity,
                id,
                label,
                pool,
                health: None,
            });
        }
    }

    fn pool(&self) -> Vec<usize> {
        (0..self.accts.len())
            .filter(|&i| self.accts[i].pool && self.accts[i].logged_in())
            .collect()
    }

    /// The config the wizard would write.
    pub fn build_config(&self) -> Config {
        let mut c = self.base.clone();
        let rows: Vec<&PathBuf> = self.accts.iter().map(|a| &a.dir).collect();
        let existing = self.base.profiles(&self.ctx.paths);
        c.profiles = self
            .base
            .profiles
            .iter()
            .zip(&existing)
            .filter(|(_, p)| !rows.contains(&&p.dir))
            .map(|(pc, _)| pc.clone())
            .collect();
        for a in self.accts.iter().filter(|a| a.logged_in()) {
            let old = self
                .base
                .profiles
                .iter()
                .zip(&existing)
                .find(|(_, p)| p.dir == a.dir)
                .map(|(pc, _)| pc.clone());
            let mut pc = old.unwrap_or_else(|| ProfileConfig {
                dir: tilde(&self.ctx, &a.dir),
                tool: a.tool,
                ..Default::default()
            });
            pc.id = unique_id(&c, &a.id);
            pc.label = a.label.clone();
            pc.pool = a.pool;
            c.profiles.push(pc);
        }
        c
    }

    fn compute_health(&mut self) {
        let cfg = self.build_config();
        for i in self.pool() {
            let p = self.accts[i].profile();
            let h = share_health(&p.dir, &p.common_home, &cfg.share_rules(p.tool))
                .map_err(|e| format!("{e:#}"))
                .map(|h| {
                    let plan = if h.to_fix > 0 {
                        plan_for(&self.ctx, &cfg, &p)
                            .map(|p| plan_summary(&p))
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    (h, plan)
                });
            self.accts[i].health = Some(h);
        }
    }

    fn start_usage(&mut self) {
        if !matches!(self.usage, Usage::Idle) {
            return;
        }
        let ctx = self.ctx.clone();
        let profiles: Vec<Profile> = self
            .pool()
            .iter()
            .map(|&i| self.accts[i].profile())
            .collect();
        let fetch = self.fetch_usage;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let fetcher = UreqFetcher::default();
            let t = now();
            let mut out = Vec::new();
            for p in profiles {
                let creds = read_credentials(ctx.store.as_ref(), &p).ok().flatten();
                let live = creds
                    .as_ref()
                    .filter(|c| c.expires_at.is_none_or(|e| e > t));
                let fetched = match (fetch, live) {
                    (true, Some(c)) => {
                        let tok = crate::tokens::access_token(c);
                        let r = fetcher.fetch(&tok, t);
                        let cached = CachedUsage {
                            snapshot: r.as_ref().ok().cloned(),
                            last_attempt: t,
                            last_error: r.as_ref().err().map(|e| e.to_string()),
                            backoff_until: None,
                            token_expires_at: c.expires_at,
                        };
                        if cached.snapshot.is_some() {
                            let _ = ctx.cache.store(&p.id, &cached);
                        }
                        Some(cached)
                    }
                    _ => None,
                };
                if let Some(c) = fetched
                    .filter(|c| c.snapshot.is_some())
                    .or_else(|| ctx.cache.load(&p.id))
                {
                    out.push((p.id.clone(), c));
                }
            }
            let _ = tx.send(out);
        });
        self.usage = Usage::Running(rx);
    }

    fn views_from(&self, usage: FetchResult) -> Vec<AccountView> {
        let cfg = self.build_config();
        let t = now();
        let inputs: Vec<AccountInput> = self
            .pool()
            .iter()
            .map(|&i| {
                let a = &self.accts[i];
                let p = a.profile();
                let mut identity = a.identity.clone();
                let token = match read_credentials(self.ctx.store.as_ref(), &p) {
                    Ok(c) => {
                        if let Some(c) = &c {
                            identity.subscription = c.subscription_type.clone();
                            identity.rate_limit_tier = c.rate_limit_tier.clone();
                        }
                        token_state(c.as_ref(), t)
                    }
                    Err(_) => TokenState::Missing,
                };
                AccountInput {
                    profile_id: a.id.clone(),
                    label: a.label.clone(),
                    color: None,
                    in_pool: true,
                    weight: 1.0,
                    reserve_7d: 0,
                    identity,
                    token,
                    sessions_elsewhere: 0,
                    active_here: false,
                    last_used: None,
                }
            })
            .collect();
        let usage: HashMap<String, CachedUsage> = usage.into_iter().collect();
        let vctx = ViewCtx {
            now: t,
            model: None,
            settings: balance_settings(&cfg),
            exclude: None,
            stale_after_secs: 600,
        };
        build_views(&inputs, &usage, &HashMap::new(), &vctx)
    }

    fn go(&mut self, step: usize) {
        self.message = None;
        self.step = step.min(STEPS.len() - 1);
        match self.step {
            2 => self.compute_health(),
            4 => self.start_usage(),
            _ => {}
        }
    }

    fn finish(&mut self) -> PanelOutcome<UiAction> {
        let cfg = self.build_config();
        match cfg.save(&self.ctx.paths) {
            Ok(()) => {
                self.result = Some(cfg);
                PanelOutcome::Close
            }
            Err(e) => {
                self.message = Some((format!("Could not save the config: {e:#}"), true));
                PanelOutcome::Continue
            }
        }
    }

    fn rows_len(&self) -> usize {
        match self.step {
            1 => self.accts.len() + 1,
            2 => self.pool().len() + 1,
            3 => POLICIES.len() + ON_LIMIT.len(),
            4 => MODES.len(),
            _ => 0,
        }
    }

    fn on_select(&mut self, sel: usize, space: bool) -> PanelOutcome<UiAction> {
        match self.step {
            0 => self.go(1),
            1 if sel == self.accts.len() => {
                let mut v = LoginView::add(self.ctx.clone(), &self.build_config(), None, None);
                v.command = self.login_cmd.clone();
                self.overlay = Overlay::Login(Box::new(v));
            }
            1 => {
                let a = &mut self.accts[sel];
                if !a.logged_in() {
                    if space {
                        self.message =
                            Some(("Log in first: press ⏎ on this account.".into(), true));
                    } else {
                        let profile = a.profile();
                        let cfg = self.build_config();
                        let v = LoginView::relogin(
                            self.ctx.clone(),
                            &cfg,
                            &profile,
                            self.login_cmd.clone(),
                        );
                        self.overlay = Overlay::Login(Box::new(v));
                    }
                } else if space {
                    a.pool = !a.pool;
                } else {
                    self.overlay = Overlay::EditLabel(sel, TextField::new(&a.label));
                }
            }
            2 => {
                let pool = self.pool();
                if let Some(&i) = pool.get(sel) {
                    let p = self.accts[i].profile();
                    if !p.is_default_install {
                        let cfg = self.build_config();
                        self.overlay =
                            Overlay::Adopt(Box::new(AdoptView::new(self.ctx.clone(), &cfg, p)));
                    }
                } else {
                    self.overlay = Overlay::EditBackup(TextField::new(&self.base.backup.target));
                }
            }
            3 if sel < POLICIES.len() => self.base.balance.policy = POLICIES[sel].0,
            3 => self.base.balance.on_limit = ON_LIMIT[sel - POLICIES.len()].0,
            4 => self.base.dock.mode = MODES[sel].0,
            5 if !space => return self.finish(),
            _ => {}
        }
        PanelOutcome::Continue
    }

    fn overlay_key(&mut self, key: KeyEvent) -> Option<PanelOutcome<UiAction>> {
        let ov = std::mem::replace(&mut self.overlay, Overlay::None);
        let (ov, out) = match ov {
            Overlay::None => return None,
            Overlay::Quit => match key.code {
                KeyCode::Char('y') => (Overlay::None, PanelOutcome::Close),
                KeyCode::Char('n') | KeyCode::Esc => (Overlay::None, PanelOutcome::Continue),
                _ => (Overlay::Quit, PanelOutcome::Continue),
            },
            Overlay::EditLabel(i, mut f) => match key.code {
                KeyCode::Enter => {
                    if !f.text.trim().is_empty() {
                        self.accts[i].label = f.text.trim().to_string();
                    }
                    (Overlay::None, PanelOutcome::Continue)
                }
                KeyCode::Esc => (Overlay::None, PanelOutcome::Continue),
                _ => {
                    f.handle_key(key);
                    (Overlay::EditLabel(i, f), PanelOutcome::Continue)
                }
            },
            Overlay::EditBackup(mut f) => match key.code {
                KeyCode::Enter => {
                    if !f.text.trim().is_empty() {
                        self.base.backup.target = f.text.trim().to_string();
                    }
                    (Overlay::None, PanelOutcome::Continue)
                }
                KeyCode::Esc => (Overlay::None, PanelOutcome::Continue),
                _ => {
                    f.handle_key(key);
                    (Overlay::EditBackup(f), PanelOutcome::Continue)
                }
            },
            Overlay::Adopt(mut v) => match v.handle_key(key) {
                FlowOutcome::Continue => (Overlay::Adopt(v), PanelOutcome::Continue),
                FlowOutcome::Done(_) => {
                    self.compute_health();
                    (Overlay::None, PanelOutcome::Continue)
                }
            },
            Overlay::Login(mut v) => match v.handle_key(key) {
                FlowOutcome::Continue => (Overlay::Login(v), PanelOutcome::Continue),
                FlowOutcome::Done(_) => {
                    if let Some(done) = v.result.clone() {
                        let dir = self.ctx.paths.expand(&done.dir);
                        self.rediscover();
                        if !self.accts.iter().any(|a| a.dir == dir) {
                            self.accts.push(Acct {
                                dir: dir.clone(),
                                tool: done.tool,
                                common: self.base.common_home_for(&self.ctx.paths, done.tool),
                                is_default: false,
                                kind: DirKind::LoggedIn,
                                identity: done.identity.clone(),
                                id: done.id.clone(),
                                label: done.label.clone(),
                                pool: true,
                                health: None,
                            });
                        }
                        if let Some(a) = self.accts.iter_mut().find(|a| a.dir == dir) {
                            a.kind = DirKind::LoggedIn;
                            a.identity = done.identity.clone();
                            a.pool = !a.is_default;
                            if done.is_new {
                                a.id = done.id.clone();
                                a.label = done.label.clone();
                            }
                        }
                        self.message = Some((format!("{} is ready.", done.label), false));
                    }
                    (Overlay::None, PanelOutcome::Continue)
                }
            },
        };
        self.overlay = ov;
        Some(out)
    }

    // ----- rendering ----------------------------------------------------

    fn render_rail(&self, area: Rect, buf: &mut Buffer) {
        let mut lines = vec![Line::from("")];
        for (i, name) in STEPS.iter().enumerate() {
            let (mark, style) = if i == self.step {
                ("▸", t::selected())
            } else if i < self.step {
                ("✓", t::ok())
            } else {
                (" ", t::dim())
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {mark} "), style),
                Span::styled(
                    format!("{} {name}", i + 1),
                    if i == self.step {
                        style
                    } else if i < self.step {
                        Style::default()
                    } else {
                        t::dim()
                    },
                ),
            ]));
            lines.push(Line::from(""));
        }
        t::draw_lines(buf, area, lines);
    }

    fn content(&mut self, area: Rect, buf: &mut Buffer) {
        if let Overlay::Login(v) = &mut self.overlay {
            v.render(area, buf);
            return;
        }
        if let Overlay::Adopt(v) = &mut self.overlay {
            v.render(area, buf);
            return;
        }
        let mut lines: Vec<Line<'static>> = Vec::new();
        let heading = |s: &str| Line::from(Span::styled(format!(" {s}"), t::title()));
        let para = |s: &str| Line::from(format!(" {s}"));
        let note = |s: &str| Line::from(Span::styled(format!(" {s}"), t::dim()));
        let sel = self.sel[self.step];
        let mut sel_line = 0;
        let mut area = area;
        match self.step {
            0 => {
                // The prism above the welcome text, when it fits.
                let art_h = area.height.saturating_sub(12).min(16);
                if art_h >= 6 && area.width >= 24 {
                    let art_w = area.width.min(art_h * 5 + 10);
                    let art = Rect {
                        x: area.x + (area.width - art_w) / 2,
                        width: art_w,
                        height: art_h,
                        ..area
                    };
                    let secs = self.tick as f64 * 0.1;
                    t::draw_lines(buf, art, super::art::prism_hero(secs, art_w, art_h));
                    area.y += art_h;
                    area.height -= art_h;
                }
                lines.extend([
                    Line::from(""),
                    heading("Welcome to agent-prism"),
                    Line::from(""),
                    para("agent-prism starts Claude Code or Codex on the account with the most usage left."),
                    para("When one account hits its limit, the same conversation continues on another."),
                    para("A small dock under the tool shows how much each account has used."),
                    note("Claude accounts use Claude plans. Codex accounts use ChatGPT plans."),
                    Line::from(""),
                    note("This takes about a minute. Nothing changes until the last step,"),
                    note("except things you confirm on the way."),
                    Line::from(""),
                    Line::from(vec![Span::styled(" Press ", t::dim()), Span::styled("⏎", t::bold()), Span::styled(" to begin.", t::dim())]),
                ]);
            }
            1 => {
                let mixed = self.accts.iter().any(|a| a.tool == Tool::Codex);
                lines.extend([
                    heading(if mixed {
                        "Your Claude and Codex accounts"
                    } else {
                        "Your Claude accounts"
                    }),
                    note(
                        "Choose which accounts agent-prism may use. ⏎ renames, or logs in where needed.",
                    ),
                    Line::from(""),
                ]);
                for (i, a) in self.accts.iter().enumerate() {
                    let on = i == sel;
                    if on {
                        sel_line = lines.len();
                    }
                    let editing = matches!(&self.overlay, Overlay::EditLabel(j, _) if *j == i);
                    let mut spans = vec![t::marker(on)];
                    let tag = if mixed {
                        format!("{:<7}", a.tool.name())
                    } else {
                        String::new()
                    };
                    if a.logged_in() {
                        spans.push(t::check(a.pool));
                        spans.push(Span::raw(" "));
                        spans.push(Span::styled(tag, t::dim()));
                        if let (true, Overlay::EditLabel(_, f)) = (editing, &self.overlay) {
                            spans.extend(f.spans(true));
                            spans.push(Span::raw("  "));
                        } else {
                            spans.push(Span::styled(t::fit(&a.label, 22), t::row(on)));
                        }
                        let email = a.identity.email.clone().unwrap_or_default();
                        spans.push(Span::raw(t::fit(&email, 26)));
                        if area.width >= 90 {
                            let plan = a.identity.subscription.clone().unwrap_or_default();
                            let org = a.identity.org.clone().unwrap_or_default();
                            spans.push(Span::styled(t::fit(&org, 18), t::dim()));
                            spans.push(Span::styled(t::fit(&plan, 6), t::dim()));
                        }
                        spans.push(Span::styled(
                            t::fit(&tilde(&self.ctx, &a.dir), 16),
                            t::dim(),
                        ));
                        if a.is_default {
                            spans.push(Span::styled(" main install", t::dim()));
                        }
                    } else {
                        spans.push(Span::styled("[ ] ", t::dim()));
                        spans.push(Span::styled(tag, t::dim()));
                        spans.push(Span::styled(
                            t::fit(&tilde(&self.ctx, &a.dir), 22),
                            t::dim(),
                        ));
                        spans.push(Span::styled("not logged in · ", t::dim()));
                        spans.push(Span::styled(
                            "⏎ log in",
                            if on {
                                t::selected()
                            } else {
                                Style::default().fg(t::ACCENT)
                            },
                        ));
                    }
                    lines.push(Line::from(spans));
                }
                let on = sel == self.accts.len();
                if on {
                    sel_line = lines.len();
                }
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    t::marker(on),
                    Span::styled(
                        "+ Add a new account",
                        Style::default().fg(t::ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  log in to another Claude or ChatGPT account", t::dim()),
                ]));
                if self.pool().is_empty() {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        " Pick at least one account to continue.",
                        t::warn(),
                    )));
                }
            }
            2 => {
                let codex = self
                    .pool()
                    .iter()
                    .any(|&i| self.accts[i].tool == Tool::Codex);
                lines.extend([
                    heading("Share sessions and settings"),
                    note("Accounts share ~/.claude: settings, skills, memory and past sessions."),
                ]);
                if codex {
                    lines.push(note("Codex accounts share ~/.codex in the same way."));
                }
                lines.extend([
                    note("Shared sessions let a conversation move between accounts. Logins stay separate."),
                    Line::from(""),
                ]);
                let pool = self.pool();
                for (k, &i) in pool.iter().enumerate() {
                    let a = &self.accts[i];
                    let on = k == sel;
                    if on {
                        sel_line = lines.len();
                    }
                    let (txt, style) = if a.is_default {
                        (
                            format!("this is {} itself", tilde(&self.ctx, &a.common)),
                            t::dim(),
                        )
                    } else {
                        match &a.health {
                            Some(Ok((h, _))) if h.to_fix == 0 && h.sessions_shared => {
                                ("✓ already shared".into(), t::ok())
                            }
                            Some(Ok((h, _))) if h.to_fix == 0 => {
                                ("sessions not shared yet".into(), t::warn())
                            }
                            Some(Ok((_, plan))) => (format!("{plan} · ⏎ review"), t::warn()),
                            Some(Err(e)) => (e.clone(), t::error()),
                            None => ("…".into(), t::dim()),
                        }
                    };
                    lines.push(Line::from(vec![
                        t::marker(on),
                        Span::styled(t::fit(&a.label, 22), t::row(on)),
                        Span::styled(txt, style),
                    ]));
                }
                let on = sel == pool.len();
                if on {
                    sel_line = lines.len();
                }
                lines.push(Line::from(""));
                let mut spans = vec![t::marker(on), Span::styled("Backups go to  ", t::row(on))];
                match &self.overlay {
                    Overlay::EditBackup(f) => spans.extend(f.spans(true)),
                    _ => spans.push(Span::raw(self.base.backup.target.clone())),
                }
                lines.push(Line::from(spans));
                lines.push(Line::from(""));
                lines.push(note(
                    "Skip with → if you prefer; Settings › Sharing can do it later.",
                ));
            }
            3 => {
                lines.extend([heading("Which account for a new session?"), Line::from("")]);
                for (i, (p, name, desc)) in POLICIES.iter().enumerate() {
                    if i == sel {
                        sel_line = lines.len();
                    }
                    lines.push(radio_line(
                        i == sel,
                        self.base.balance.policy == *p,
                        name,
                        desc,
                    ));
                }
                lines.extend([
                    Line::from(""),
                    heading("When an account hits its limit"),
                    Line::from(""),
                ]);
                for (i, (o, name, desc)) in ON_LIMIT.iter().enumerate() {
                    let k = i + POLICIES.len();
                    if k == sel {
                        sel_line = lines.len();
                    }
                    lines.push(radio_line(
                        k == sel,
                        self.base.balance.on_limit == *o,
                        name,
                        desc,
                    ));
                }
            }
            4 => {
                lines.extend([
                    heading("The dock"),
                    note("It sits under Claude or Codex and shows each account's usage."),
                    Line::from(""),
                ]);
                for (i, (m, desc)) in MODES.iter().enumerate() {
                    lines.push(radio_line(
                        i == sel,
                        self.base.dock.mode == *m,
                        mode_name(*m),
                        desc,
                    ));
                }
                lines.push(Line::from(""));
                let views = match &self.usage {
                    Usage::Running(_) => {
                        lines.push(Line::from(vec![
                            Span::styled(format!(" {} ", t::spinner(self.tick)), t::title()),
                            Span::styled("Fetching your usage…", t::dim()),
                        ]));
                        None
                    }
                    Usage::Done(v) => {
                        lines.push(Line::from(Span::styled(" Preview", t::bold())));
                        Some(v.clone())
                    }
                    Usage::Idle => None,
                };
                let used = lines.len() as u16;
                t::draw_lines(buf, area, lines);
                if let Some(views) = views {
                    let cfg = self.build_config();
                    let mode = match self.base.dock.mode {
                        DockMode::Auto | DockMode::Full => DockMode::Full,
                        m => m,
                    };
                    let h = match mode {
                        DockMode::Full => views.len() as u16 + 2,
                        _ => 1,
                    };
                    let r = Rect {
                        y: area.y + used + 1,
                        height: h.min(area.height.saturating_sub(used + 1)),
                        ..area
                    };
                    if mode == DockMode::Hidden {
                        t::draw_lines(
                            buf,
                            r,
                            vec![Line::from(Span::styled("   (no dock)", t::dim()))],
                        );
                    } else if r.height > 0 {
                        render_dock(&dock_model(&cfg, &views, mode), r, buf);
                    }
                }
                return;
            }
            _ => {
                let cfg = self.build_config();
                let pool: Vec<String> = cfg
                    .profiles
                    .iter()
                    .filter(|p| p.pool)
                    .map(|p| p.label.clone())
                    .collect();
                let others = cfg.profiles.len() - pool.len();
                let has_codex = cfg.profiles.iter().any(|p| p.tool == Tool::Codex);
                let kv = |k: &str, v: String| {
                    Line::from(vec![
                        Span::styled(format!("   {k:<12}"), t::dim()),
                        Span::raw(v),
                    ])
                };
                lines.extend([
                    Line::from(""),
                    heading("All set"),
                    Line::from(""),
                    kv(
                        "Pool",
                        if pool.is_empty() {
                            "none".into()
                        } else {
                            pool.join(", ")
                        },
                    ),
                    kv(
                        "Others",
                        format!(
                            "{others} known account{}, used only with agp --as",
                            if others == 1 { "" } else { "s" }
                        ),
                    ),
                    kv(
                        "Balancing",
                        format!(
                            "{} · on limit: {}",
                            policy_name(cfg.balance.policy),
                            on_limit_name(cfg.balance.on_limit)
                        ),
                    ),
                    kv("Dock", mode_name(cfg.dock.mode).to_string()),
                    Line::from(""),
                    Line::from(vec![
                        Span::raw(" Start with "),
                        Span::styled("agp", t::title()),
                        Span::raw(" instead of claude"),
                        Span::raw(if has_codex { ", and " } else { "." }),
                        Span::styled(if has_codex { "agp codex" } else { "" }, t::title()),
                        Span::raw(if has_codex { " instead of codex." } else { "" }),
                    ]),
                    Line::from(vec![
                        Span::raw(" Open the menu with "),
                        Span::styled("⌃\\", t::title()),
                        Span::raw(" (ctrl+backslash)."),
                    ]),
                    Line::from(vec![
                        Span::raw(" Add accounts any time with "),
                        Span::styled("agp add", t::title()),
                        Span::raw("."),
                    ]),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled(" Press ", t::dim()),
                        Span::styled("⏎", t::bold()),
                        Span::styled(" to save and finish.", t::dim()),
                    ]),
                ]);
            }
        }
        let top = t::scroll_top(sel_line, lines.len(), area.height as usize);
        t::draw_lines(buf, area, lines.into_iter().skip(top).collect());
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.overlay {
            Overlay::Login(v) => return v.hints(),
            Overlay::Adopt(v) => return v.hints(),
            Overlay::EditLabel(..) | Overlay::EditBackup(_) => {
                return vec![("⏎", "save"), ("esc", "cancel")];
            }
            Overlay::Quit => return vec![("y", "quit"), ("n", "stay")],
            Overlay::None => {}
        }
        match self.step {
            0 => vec![("⏎", "begin"), ("q", "quit")],
            1 => vec![
                ("↑↓", "move"),
                ("␣", "use / skip"),
                ("⏎", "rename · log in"),
                ("→", "next"),
                ("esc", "back"),
            ],
            2 => vec![
                ("↑↓", "move"),
                ("⏎", "review"),
                ("→", "next"),
                ("esc", "back"),
            ],
            3 | 4 => vec![
                ("↑↓", "move"),
                ("⏎", "choose"),
                ("→", "next"),
                ("esc", "back"),
            ],
            _ => vec![("⏎", "finish"), ("esc", "back")],
        }
    }
}

fn radio_line(sel: bool, on: bool, name: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        t::marker(sel),
        t::radio(on),
        Span::styled(format!(" {}", t::fit(name, 13)), t::row(sel)),
        Span::styled(desc.to_string(), t::dim()),
    ])
}

fn policy_name(p: Policy) -> &'static str {
    POLICIES.iter().find(|x| x.0 == p).map_or("smart", |x| x.1)
}

fn on_limit_name(o: OnLimit) -> &'static str {
    ON_LIMIT.iter().find(|x| x.0 == o).map_or("auto", |x| x.1)
}

impl Panel<UiAction> for Wizard {
    fn preferred_height(&self, rows: u16) -> u16 {
        rows
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let inner = t::frame(buf, area, "agp setup");
        let (body, foot) = t::split_footer(inner);
        let rail_w = if body.width >= 70 { 16 } else { 0 };
        let (rail, content) = (
            Rect {
                width: rail_w,
                ..body
            },
            Rect {
                x: body.x + rail_w + 1,
                width: body.width.saturating_sub(rail_w + 2),
                y: body.y + 1,
                height: body.height.saturating_sub(2),
            },
        );
        if rail_w > 0 {
            self.render_rail(rail, buf);
            for y in body.y..body.y + body.height {
                buf[(body.x + rail_w - 1, y)]
                    .set_symbol("│")
                    .set_style(t::dim());
            }
        } else if body.height > 0 {
            t::draw_lines(
                buf,
                Rect { height: 1, ..body },
                vec![Line::from(Span::styled(
                    format!(" Step {} of 6 · {}", self.step + 1, STEPS[self.step]),
                    t::dim(),
                ))],
            );
        }
        self.content(content, buf);
        let msg = match (&self.overlay, &self.message) {
            (Overlay::Quit, _) => Some(("Quit setup? Nothing is saved yet.".to_string(), true)),
            (_, m) => m.clone(),
        };
        if let Some((m, err)) = msg
            && body.height > 1
        {
            let r = Rect {
                y: body.y + body.height - 1,
                height: 1,
                ..content
            };
            t::draw_lines(
                buf,
                r,
                vec![Line::from(Span::styled(
                    format!(" {m}"),
                    if err { t::warn() } else { t::ok() },
                ))],
            );
        }
        t::draw_lines(buf, foot, vec![t::hints(&self.hints())]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> PanelOutcome<UiAction> {
        let ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        let in_flow = matches!(self.overlay, Overlay::Login(_) | Overlay::Adopt(_));
        if ctrl_c && !in_flow {
            self.overlay = Overlay::Quit;
            return PanelOutcome::Continue;
        }
        if let Some(out) = self.overlay_key(key) {
            return out;
        }
        let n = self.rows_len();
        let sel = self.sel[self.step].min(n.saturating_sub(1));
        match key.code {
            KeyCode::Char('q') => self.overlay = Overlay::Quit,
            KeyCode::Esc | KeyCode::Left => {
                if self.step == 0 {
                    self.overlay = Overlay::Quit;
                } else {
                    self.go(self.step - 1);
                }
            }
            KeyCode::Right | KeyCode::Tab | KeyCode::Char('n') if self.step < 5 => {
                if self.step == 1 && self.pool().is_empty() {
                    self.message = Some(("Pick at least one account first.".into(), true));
                } else {
                    self.go(self.step + 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') if n > 0 => self.sel[self.step] = (sel + n - 1) % n,
            KeyCode::Down | KeyCode::Char('j') if n > 0 => self.sel[self.step] = (sel + 1) % n,
            KeyCode::Char('e')
                if self.step == 1 && sel < self.accts.len() && self.accts[sel].logged_in() =>
            {
                self.overlay = Overlay::EditLabel(sel, TextField::new(&self.accts[sel].label));
            }
            KeyCode::Char(' ') => return self.on_select(sel, true),
            KeyCode::Enter => {
                let out = self.on_select(sel, false);
                // Choosing a radio option moves on in steps 4 and 5.
                return out;
            }
            _ => {}
        }
        PanelOutcome::Continue
    }

    fn handle_paste(&mut self, text: &str) -> PanelOutcome<UiAction> {
        match &mut self.overlay {
            Overlay::Login(v) => v.handle_paste(text),
            Overlay::EditLabel(_, f) | Overlay::EditBackup(f) => f.insert(text),
            _ => {}
        }
        PanelOutcome::Continue
    }

    fn tick(&mut self) {
        self.tick += 1;
        match &mut self.overlay {
            Overlay::Login(v) => v.tick(),
            Overlay::Adopt(v) => v.tick(),
            _ => {}
        }
        if let Usage::Running(rx) = &self.usage
            && let Ok(r) = rx.try_recv()
        {
            let views = self.views_from(r);
            self.usage = Usage::Done(views);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testutil::*;

    fn wizard() -> (Fixture, Wizard) {
        let f = Fixture::new();
        let mut w = Wizard::new(f.ctx.clone());
        w.fetch_usage = false;
        (f, w)
    }

    fn wait_usage(w: &mut Wizard) {
        for _ in 0..300 {
            w.tick();
            if matches!(w.usage, Usage::Done(_)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("usage never finished");
    }

    #[test]
    fn walks_every_step_and_saves() {
        let (f, mut w) = wizard();
        let expect = [
            "Welcome to agent-prism",
            "Your Claude accounts",
            "Share sessions and settings",
            "Which account for a new session?",
            "The dock",
            "All set",
        ];
        for (i, want) in expect.iter().enumerate() {
            if i == 4 {
                wait_usage(&mut w);
            }
            for (wd, h) in SIZES {
                let s = render(&mut w, wd, h);
                assert!(s.contains(want), "step {i} {wd}x{h}:\n{s}");
            }
            if i == 0 {
                w.handle_key(key(KeyCode::Enter));
            } else if i < 5 {
                w.handle_key(key(KeyCode::Right));
            }
        }
        let s = render(&mut w, 120, 40);
        assert!(s.contains("Ann, Bob"), "{s}");
        assert!(matches!(
            w.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Close
        ));
        let c = Config::load(f.paths()).unwrap().unwrap();
        assert_eq!(c, w.result.clone().unwrap());
        let ids: Vec<_> = c.profiles.iter().map(|p| (p.id.as_str(), p.pool)).collect();
        assert_eq!(ids, vec![("a", true), ("b", true)]);
        assert_eq!(c.profiles[0].dir, "~/.claude-a");
    }

    #[test]
    fn accounts_step_lists_and_toggles() {
        let (_f, mut w) = wizard();
        w.handle_key(key(KeyCode::Enter));
        let s = render(&mut w, 120, 40);
        assert!(
            s.contains("ann@example.com") && s.contains("Acme Team"),
            "{s}"
        );
        assert!(s.contains("~/.claude-c") && s.contains("⏎ log in"), "{s}");
        assert!(s.contains("+ Add a new account"));
        // Rows: .claude (no login), a, b, c.
        w.handle_key(key(KeyCode::Down));
        w.handle_key(ch(' '));
        assert!(!w.accts[1].pool);
        w.handle_key(ch(' '));
        w.handle_key(ch('e'));
        w.handle_key(ctrl('u'));
        for c in "Work".chars() {
            w.handle_key(ch(c));
        }
        w.handle_key(key(KeyCode::Enter));
        assert_eq!(w.accts[1].label, "Work");
        w.handle_key(key(KeyCode::Up));
        w.handle_key(ch(' '));
        assert!(!w.accts[0].pool, "not logged in cannot join the pool");
    }

    #[test]
    fn needs_a_pool_account() {
        let (_f, mut w) = wizard();
        w.handle_key(key(KeyCode::Enter));
        for a in &mut w.accts {
            a.pool = false;
        }
        w.handle_key(key(KeyCode::Right));
        assert_eq!(w.step, 1);
    }

    #[test]
    fn quit_confirms() {
        let (_f, mut w) = wizard();
        w.handle_key(ch('q'));
        assert!(render(&mut w, 80, 24).contains("Quit setup?"));
        w.handle_key(ch('n'));
        w.handle_key(key(KeyCode::Esc));
        assert!(matches!(w.handle_key(ch('y')), PanelOutcome::Close));
        assert!(w.result.is_none());
    }

    #[test]
    fn esc_goes_back() {
        let (_f, mut w) = wizard();
        w.handle_key(key(KeyCode::Enter));
        w.handle_key(key(KeyCode::Right));
        assert_eq!(w.step, 2);
        w.handle_key(key(KeyCode::Esc));
        assert_eq!(w.step, 1);
    }

    #[test]
    fn sharing_step_opens_adopt() {
        let (_f, mut w) = wizard();
        w.handle_key(key(KeyCode::Enter));
        w.handle_key(key(KeyCode::Right));
        let s = render(&mut w, 120, 40);
        assert!(s.contains("Backups go to"), "{s}");
        assert!(
            s.contains("to link") || s.contains("to fix") || s.contains("review"),
            "{s}"
        );
        w.handle_key(key(KeyCode::Enter));
        assert!(render(&mut w, 80, 24).contains("Share sessions and settings · Ann"));
        w.handle_key(key(KeyCode::Esc));
        assert!(matches!(w.overlay, Overlay::None));
    }

    #[test]
    fn choices_land_in_config() {
        let (_f, mut w) = wizard();
        w.go(3);
        w.handle_key(key(KeyCode::Down));
        w.handle_key(key(KeyCode::Enter));
        for _ in 0..5 {
            w.handle_key(key(KeyCode::Down));
        }
        w.handle_key(key(KeyCode::Enter));
        w.go(4);
        w.handle_key(key(KeyCode::Down));
        w.handle_key(key(KeyCode::Down));
        w.handle_key(key(KeyCode::Enter));
        let c = w.build_config();
        assert_eq!(c.balance.policy, Policy::Priority);
        assert_eq!(c.balance.on_limit, OnLimit::Stay);
        assert_eq!(c.dock.mode, DockMode::Compact);
    }

    #[test]
    fn add_account_in_wizard() {
        let (_f, mut w) = wizard();
        w.login_cmd = Some(LoginCommand {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"printf '{"oauthAccount":{"displayName":"Neo","emailAddress":"neo@example.com"}}' > "$CLAUDE_CONFIG_DIR/.claude.json""#.into(),
            ],
        });
        w.handle_key(key(KeyCode::Enter));
        w.handle_key(key(KeyCode::Up)); // wraps to "+ Add"
        w.handle_key(key(KeyCode::Enter));
        w.handle_key(key(KeyCode::Enter)); // Tool: Claude
        for c in "neo".chars() {
            w.handle_key(ch(c));
        }
        w.handle_key(key(KeyCode::Enter));
        w.handle_key(key(KeyCode::Enter));
        for _ in 0..300 {
            w.tick();
            if render(&mut w, 80, 24).contains("Logged in as") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        w.handle_key(key(KeyCode::Enter));
        let c = w.build_config();
        assert!(
            c.profiles.iter().any(|p| p.id == "neo" && p.pool),
            "{:?}",
            c.profiles
        );
    }
}
