//! `agp -p` / `agp codex exec` with `--on-limit` (DESIGN §7.4, §9): the
//! line runs as a child of agent-prism instead of replacing it, so that a
//! limit can be handled: wait for the reset, move to another account, or
//! give up with exit code 75. After a wait or a move the same conversation
//! goes on (`claude -p --resume`, `codex exec resume`).
//!
//! The run keeps the drop-in contract of print mode where it can: same
//! args, stdin for the first attempt, stderr, and the exit code of the last
//! attempt. stdout of an attempt that a limit stopped is dropped, unless
//! the output streams (`--output-format stream-json`, `codex exec --json`).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use prism_core::config::Config;
use prism_core::journal::{EventKind, Recorder, RunMode};
use prism_core::model::{LimitKind, Limited, OnLimit, Tool};
use prism_core::profile::Profile;
use prism_usage::balance::{self, fmt_duration};
use serde_json::Value;

use crate::app::AppCtx;
use crate::cli::{EXIT_LIMITED, LaunchArgs, LimitAction};
use crate::launch::{self, unix_now};
use crate::tokens::KeychainTokens;

/// Limit recoveries (moves and waits) per run before giving up.
const MAX_RECOVERIES: u32 = 20;
/// A wait goes on this long after the reset time.
const WAIT_GRACE_SECS: i64 = 30;
/// With no known reset time, check the usage again after this long.
const WAIT_RECHECK_SECS: i64 = 300;
/// The longest single sleep, so that freed accounts are seen.
const WAIT_STEP_SECS: i64 = 300;

/// The pid of the running child, for forwarding signals (0 = none).
static CHILD: AtomicI32 = AtomicI32::new(0);

/// What one attempt of the tool did.
#[derive(Debug, Default)]
struct Outcome {
    code: i32,
    /// A `StopFailure` hook with `error == rate_limit` came in.
    rate_limited: bool,
    /// Claude's session id (`SessionStart`) or the Codex thread id.
    session: Option<String>,
    /// The buffered stdout, when it is buffered.
    stdout: Option<Vec<u8>>,
}

/// One supervised print-mode run.
struct Run<'a> {
    ctx: &'a AppCtx,
    args: &'a LaunchArgs,
    config: Config,
    profiles: Vec<Profile>,
    tokens: Arc<KeychainTokens>,
    user_args: Vec<String>,
    user_settings: Option<String>,
    model: Option<String>,
    journal: Recorder,
    action: LimitAction,
    /// Seconds left for waits (`--max-wait`).
    wait_left: Option<i64>,
    cwd: PathBuf,
    sock: PathBuf,
    hooks: Option<mpsc::Receiver<(String, Value)>>,
}

/// The entry point for print mode with `--on-limit`.
pub fn run(ctx: &AppCtx, args: &LaunchArgs) -> Result<i32> {
    let tool = args.tool;
    let mut config = match Config::load(&ctx.paths) {
        Ok(Some(c)) if c.profiles.iter().any(|p| p.tool == tool) => c,
        // No accounts: print mode as usual (a note, then the plain tool).
        _ => return crate::session::run_print(ctx, args),
    };
    args.run.apply(&mut config, tool)?;
    let profiles = config.profiles_for(&ctx.paths, tool);
    let tokens = Arc::new(KeychainTokens::new(ctx.store.clone(), profiles.clone()));
    let (user_args, user_settings) = match tool {
        Tool::Claude => launch::take_user_settings(&args.tool_args),
        Tool::Codex => (args.tool_args.clone(), None),
    };
    let model = tool
        .is_claude()
        .then(|| launch::session_model(&user_args, &config.common_home(&ctx.paths)))
        .flatten();
    let now = unix_now();
    let run_dir = ctx.paths.run_dir(std::process::id());
    let mut run = Run {
        ctx,
        args,
        profiles,
        tokens,
        user_args,
        user_settings,
        model,
        journal: Recorder::new(ctx.paths.clone(), tool, RunMode::Print, now),
        action: args
            .run
            .on_limit
            .unwrap_or(LimitAction::Mode(config.balance.on_limit)),
        wait_left: args.run.max_wait,
        cwd: std::env::current_dir().context("current dir")?,
        sock: sock_path(&run_dir),
        hooks: None,
        config,
    };
    forward_signals();
    if tool.is_claude() {
        run.hooks = listen(&run.sock);
    }
    let code = run.go();
    let _ = std::fs::remove_file(&run.sock);
    let _ = std::fs::remove_dir_all(&run_dir);
    code
}

impl Run<'_> {
    fn tool(&self) -> Tool {
        self.args.tool
    }

    fn go(&mut self) -> Result<i32> {
        let Some((mut profile, reason)) = self.start_account()? else {
            return Ok(EXIT_LIMITED);
        };
        let now = unix_now();
        let mut ev = self.journal.event(EventKind::Start, &profile.id, now);
        ev.reason = Some(reason);
        ev.cwd = Some(self.cwd.to_string_lossy().into_owned());
        self.journal.write(ev);
        launch::touch_last_used(&self.ctx.paths, &profile.id, now);

        let mut session: Option<String> = None;
        let mut prompt: Option<String> = None;
        let mut recoveries = 0;
        loop {
            let first = recoveries == 0;
            let out = self.attempt(&profile, session.as_deref(), prompt.take(), first)?;
            if out.session.is_some() {
                session = out.session.clone();
            }
            let limit = self.hit_limit(&profile, &out);
            let Some(kind) = limit else {
                return Ok(self.finish(&profile, out, session));
            };
            let mut ev = self
                .journal
                .event(EventKind::Limit, &profile.id, unix_now());
            ev.limit = kind.map(limit_code);
            ev.session = session.clone();
            self.journal.write(ev);
            let what = kind.map_or("usage", window_name);
            let reason = format!("hit the {what} limit");
            note(&format!("{} {reason}", profile.label));
            recoveries += 1;
            if recoveries > MAX_RECOVERIES {
                note("too many limits in one run; giving up");
                return Ok(self.finish(&profile, out, session));
            }
            let Some(sid) = session.clone() else {
                note("no session id to resume; giving up");
                return Ok(self.finish(&profile, out, session));
            };
            match self.action {
                LimitAction::Fail => {
                    self.finish(&profile, out, session);
                    return Ok(EXIT_LIMITED);
                }
                LimitAction::Mode(OnLimit::Stay) => {
                    return Ok(self.finish(&profile, out, session));
                }
                LimitAction::Mode(OnLimit::Wait) => {
                    if !self.wait_for(&profile, &sid)? {
                        self.finish(&profile, out, session);
                        return Ok(EXIT_LIMITED);
                    }
                    let mut ev = self
                        .journal
                        .event(EventKind::Resume, &profile.id, unix_now());
                    ev.session = Some(sid);
                    self.journal.write(ev);
                    prompt = Some(self.resume_prompt(&profile, &reason, None));
                }
                LimitAction::Mode(OnLimit::Auto | OnLimit::Ask) => {
                    let Some(next) = self.other_account(&profile) else {
                        note("no other account to move to");
                        return Ok(self.finish(&profile, out, session));
                    };
                    note(&format!("moving to {}", next.label));
                    let mut ev = self.journal.event(EventKind::Switch, &next.id, unix_now());
                    ev.from = Some(profile.id.clone());
                    ev.reason = Some("limit".into());
                    ev.session = Some(sid);
                    self.journal.write(ev);
                    prompt = Some(self.resume_prompt(&profile, &reason, Some(&next)));
                    launch::touch_last_used(&self.ctx.paths, &next.id, unix_now());
                    profile = next;
                }
            }
        }
    }

    /// Writes the buffered stdout and the end event; returns the code.
    fn finish(&self, profile: &Profile, out: Outcome, session: Option<String>) -> i32 {
        if let Some(buf) = &out.stdout {
            let mut so = std::io::stdout().lock();
            let _ = so.write_all(buf);
            let _ = so.flush();
        }
        self.journal.end(&profile.id, unix_now(), out.code, session);
        out.code
    }

    /// The account to start on and why; `None` when the run gives up.
    fn start_account(&mut self) -> Result<Option<(Profile, String)>> {
        loop {
            let snap = launch::snapshot(
                self.ctx,
                &self.config,
                &self.profiles,
                &self.tokens,
                self.args.policy,
                self.model.clone(),
            );
            let (candidate, limited, reason) = match &self.args.as_profile {
                Some(id) => {
                    let p = self
                        .profiles
                        .iter()
                        .find(|p| &p.id == id)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("no profile {id:?} in the config"))?;
                    let lim = self.limited(&p.id);
                    (Some(p), lim, "pinned with --as".to_string())
                }
                None => match snap.pick.chosen.as_ref() {
                    Some(id) => (self.find(id), None, snap.pick.reason.clone()),
                    None => {
                        let p = snap
                            .pick
                            .earliest_reset
                            .as_ref()
                            .and_then(|(label, _)| self.profiles.iter().find(|p| &p.label == label))
                            .cloned();
                        let until = snap.pick.earliest_reset.as_ref().map(|(_, t)| *t);
                        let lim = Limited {
                            kind: LimitKind::FiveHour,
                            until,
                        };
                        (p, Some(lim), snap.pick.reason.clone())
                    }
                },
            };
            let Some(limited) = limited else {
                let p = candidate.context("no account to use")?;
                log::info!("print run on {}: {reason}", p.id);
                return Ok(Some((p, reason)));
            };
            let now = unix_now();
            let who = candidate
                .as_ref()
                .map_or("every account".to_string(), |p| p.label.clone());
            let resets = limited
                .until
                .map(|t| format!("; {who} resets in {}", fmt_duration(t - now)))
                .unwrap_or_default();
            match self.action {
                LimitAction::Fail => {
                    note(&format!("all usable accounts are limited{resets}"));
                    return Ok(None);
                }
                LimitAction::Mode(OnLimit::Wait) => {
                    let id = candidate.as_ref().map(|p| p.id.clone()).unwrap_or_default();
                    let mut ev = self.journal.event(EventKind::Wait, &id, now);
                    ev.until = limited.until;
                    self.journal.write(ev);
                    let until = limited.until.map(|t| t + WAIT_GRACE_SECS);
                    if !self.sleep_until(&id, until, &who)? {
                        return Ok(None);
                    }
                }
                _ => {
                    let p = candidate.context("no account to use")?;
                    note(&format!(
                        "all usable accounts are limited; using {}{resets}",
                        p.label
                    ));
                    return Ok(Some((p, format!("all limited{resets}"))));
                }
            }
        }
    }

    fn find(&self, id: &str) -> Option<Profile> {
        self.profiles.iter().find(|p| p.id == id).cloned()
    }

    /// The limit of account `id` now (usage refreshed first), if any.
    fn limited(&self, id: &str) -> Option<Limited> {
        let p = self.find(id)?;
        let mut fresh = self.config.clone();
        fresh.usage.poll_active_secs = 0;
        launch::refresh_stale(
            &self.ctx.cache,
            &fresh,
            std::slice::from_ref(&p),
            &self.tokens,
            Duration::from_secs(5),
        );
        let now = unix_now();
        let inputs =
            launch::account_inputs(&self.ctx.paths, &self.profiles, &self.tokens, None, now);
        let usage = launch::load_usage(&self.ctx.cache, &self.profiles);
        let vctx = launch::view_ctx(
            &self.config,
            self.args.policy,
            self.model.clone(),
            None,
            now,
        );
        prism_usage::views::build_views(&inputs, &usage, &HashMap::new(), &vctx)
            .into_iter()
            .find(|v| v.profile_id == id)
            .and_then(|v| v.limited)
    }

    /// The best other account for a move, if one is not limited.
    fn other_account(&self, current: &Profile) -> Option<Profile> {
        let now = unix_now();
        let inputs =
            launch::account_inputs(&self.ctx.paths, &self.profiles, &self.tokens, None, now);
        let usage = launch::load_usage(&self.ctx.cache, &self.profiles);
        let vctx = launch::view_ctx(
            &self.config,
            self.args.policy,
            self.model.clone(),
            Some(current.id.clone()),
            now,
        );
        let pick = balance::pick(&inputs, &usage, &vctx);
        pick.chosen.and_then(|id| self.find(&id))
    }

    /// Whether the attempt stopped on a limit, and which one (`Some(None)`
    /// when the kind is unknown).
    fn hit_limit(&self, profile: &Profile, out: &Outcome) -> Option<Option<LimitKind>> {
        if out.code == 0 && !out.rate_limited {
            return None;
        }
        let lim = self.limited(&profile.id);
        match (out.rate_limited, lim) {
            (_, Some(l)) => Some(Some(l.kind)),
            (true, None) => Some(None),
            (false, None) => None,
        }
    }

    /// Waits until `profile` resets (within `--max-wait`). False: gave up.
    fn wait_for(&mut self, profile: &Profile, sid: &str) -> Result<bool> {
        let until = self.limited(&profile.id).and_then(|l| l.until);
        let mut ev = self.journal.event(EventKind::Wait, &profile.id, unix_now());
        ev.until = until;
        ev.session = Some(sid.to_string());
        self.journal.write(ev);
        let until = until.map(|t| t + WAIT_GRACE_SECS);
        if !self.sleep_until(&profile.id, until, &profile.label)? {
            return Ok(false);
        }
        Ok(true)
    }

    /// Sleeps until `until` (unknown: one recheck interval), waking every
    /// few minutes to see whether `id` (or, with no id, any account) is
    /// free earlier. False when the wait is longer than `--max-wait`.
    fn sleep_until(&mut self, id: &str, until: Option<i64>, who: &str) -> Result<bool> {
        let start = unix_now();
        let target = until.unwrap_or(start + WAIT_RECHECK_SECS).max(start + 1);
        let need = target - start;
        if let Some(left) = self.wait_left
            && need > left
        {
            note(&format!(
                "{who} resets in {}, longer than --max-wait allows; giving up",
                fmt_duration(need)
            ));
            return Ok(false);
        }
        note(&format!(
            "waiting {} for {who} to reset (until {})",
            fmt_duration(need),
            clock(target)
        ));
        loop {
            let now = unix_now();
            if now >= target {
                break;
            }
            std::thread::sleep(Duration::from_secs(
                (target - now).min(WAIT_STEP_SECS) as u64
            ));
            // Freed early (a fresh poll, or another account for a new run).
            if unix_now() < target && !id.is_empty() && self.limited(id).is_none() {
                break;
            }
        }
        if let Some(left) = &mut self.wait_left {
            *left -= unix_now() - start;
        }
        Ok(true)
    }

    /// The first prompt of a resumed attempt.
    fn resume_prompt(&self, from: &Profile, reason: &str, to: Option<&Profile>) -> String {
        let moved = match to {
            Some(t) => format!("this run moved to account {}", t.label),
            None => "agent-prism waited for the reset on the same account".to_string(),
        };
        let resume = match self.config.balance.resume_prompt.as_str() {
            "" => "continue",
            r => r,
        };
        format!(
            "[agent-prism] Account {} {reason}, so {moved} and `{}` restarted. \
             Background tasks and subagents that ran in the old process have \
             stopped; start again the ones you still need.\n\n{resume}",
            from.label,
            self.tool().bin(),
        )
    }

    /// Runs the tool once on `profile`; a resume when `session` is set.
    fn attempt(
        &mut self,
        profile: &Profile,
        session: Option<&str>,
        prompt: Option<String>,
        first: bool,
    ) -> Result<Outcome> {
        let program = self.ctx.tool_bin(&self.config, self.tool())?;
        let started_ms = unix_now() * 1000;
        let mut cmd = Command::new(&program);
        let resume = session.zip(prompt);
        cmd.args(match self.tool() {
            Tool::Claude => self.claude_args(profile, resume),
            Tool::Codex => self.codex_args(profile, resume),
        });
        let sock = if self.hooks.is_some() {
            self.sock.clone()
        } else {
            PathBuf::new()
        };
        let (set, remove) = launch::child_env(profile, &sock);
        for k in remove {
            cmd.env_remove(k);
        }
        cmd.envs(set);
        if !first {
            cmd.stdin(Stdio::null());
        }
        let buffer = !self.streams();
        if buffer {
            cmd.stdout(Stdio::piped());
        }
        log::info!(
            "print attempt on {} (resume: {})",
            profile.id,
            session.is_some()
        );
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                note(&format!("starting {}: {e}", self.tool().bin()));
                return Ok(Outcome {
                    code: 127,
                    ..Default::default()
                });
            }
        };
        CHILD.store(child.id() as i32, Ordering::SeqCst);
        let reader = child.stdout.take().map(|mut so| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = so.read_to_end(&mut buf);
                buf
            })
        });
        let status = child.wait().context("waiting for the tool")?;
        CHILD.store(0, Ordering::SeqCst);
        let stdout = reader.and_then(|h| h.join().ok());
        let code = status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(0)
        });
        let mut out = Outcome {
            code,
            stdout,
            ..Default::default()
        };
        if let Some(rx) = &self.hooks {
            // Hooks finish before the tool exits; take what came in.
            while let Ok((event, payload)) = rx.recv_timeout(Duration::from_millis(100)) {
                match event.as_str() {
                    "SessionStart" => {
                        out.session = payload
                            .get("session_id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    "StopFailure" => {
                        out.rate_limited |=
                            payload.get("error").and_then(Value::as_str) == Some("rate_limit");
                    }
                    _ => {}
                }
            }
        }
        if self.tool() == Tool::Codex {
            out.session = session
                .map(str::to_string)
                .or_else(|| self.codex_thread(profile, started_ms));
        }
        Ok(out)
    }

    /// True when the tool's stdout streams events, so it is passed through
    /// live instead of buffered.
    fn streams(&self) -> bool {
        let opts = launch::options(&self.user_args);
        match self.tool() {
            Tool::Claude => {
                opts.windows(2)
                    .any(|w| w[0] == "--output-format" && w[1] == "stream-json")
                    || opts.iter().any(|a| a == "--output-format=stream-json")
            }
            Tool::Codex => opts
                .iter()
                .any(|a| a == "--json" || a == "--experimental-json"),
        }
    }

    fn claude_args(&self, profile: &Profile, resume: Option<(&str, String)>) -> Vec<String> {
        let user = match &resume {
            None => self.user_args.clone(),
            Some((sid, prompt)) => {
                let mut a = launch::strip_positionals(&self.user_args);
                a.extend(["--resume".to_string(), sid.to_string(), prompt.clone()]);
                a
            }
        };
        let overlay = self.ctx.paths.profile_overlay(&profile.id);
        let overlay = overlay.is_file().then_some(overlay);
        let settings = crate::session::print_settings(
            self.user_settings.clone(),
            overlay.as_deref(),
            |u, o| launch::memory_dir(profile, &self.cwd, &[u, o], |k| std::env::var(k).ok()),
        );
        let settings = self.with_hooks(settings);
        crate::session::print_args(&self.config, profile, &user, settings)
    }

    /// `settings` (a path or inline JSON) with agent-prism's hooks added.
    /// When it can't be read, it goes through as is (no hooks).
    fn with_hooks(&self, settings: Option<String>) -> Option<String> {
        let (Some(_), Ok(exe)) = (&self.hooks, std::env::current_exe()) else {
            return settings;
        };
        let extra = match &settings {
            None => None,
            Some(s) => match launch::load_user_settings(s) {
                Ok(v) => Some(v),
                Err(_) => return settings,
            },
        };
        let v = launch::settings_json(&exe, None, extra.as_ref());
        serde_json::to_string(&v).ok().or(settings)
    }

    fn codex_args(&self, profile: &Profile, resume: Option<(&str, String)>) -> Vec<String> {
        let (_, config_args) = self.config.tool_bin_args(Tool::Codex);
        let mut out: Vec<String> = config_args.to_vec();
        out.extend(profile.args.iter().cloned());
        match resume {
            None => out.extend(self.user_args.iter().cloned()),
            Some((id, prompt)) => {
                out.push("exec".into());
                out.extend(crate::codex::strip_for_switch(&self.user_args));
                out.extend(["resume".to_string(), id.to_string(), prompt]);
            }
        }
        out
    }

    /// The thread of a `codex exec` that started at `since_ms`; only for
    /// `exec` lines (other lines can't be resumed).
    fn codex_thread(&self, profile: &Profile, since_ms: i64) -> Option<String> {
        if !matches!(
            crate::codex::subcommand(&self.user_args),
            Some("exec" | "e")
        ) {
            return None;
        }
        let home = launch::codex_sqlite_home(profile, |k| std::env::var(k).ok());
        let db = crate::codex::state_db(&home)?;
        let cwd = crate::codex::work_dir(&self.user_args, &self.cwd);
        crate::codex::find_exec_thread(&db, &cwd, since_ms)
    }
}

/// One stderr note; print mode writes nothing to stdout.
fn note(text: &str) {
    log::info!("print run: {text}");
    eprintln!("agp: {text}");
}

/// Local wall-clock time, e.g. "14:05" (or "Mon 14:05" beyond a day).
fn clock(ts: i64) -> String {
    let Ok(t) = jiff::Timestamp::from_second(ts) else {
        return ts.to_string();
    };
    let z = t.to_zoned(jiff::tz::TimeZone::system());
    if ts - unix_now() > 86_400 {
        z.strftime("%a %H:%M").to_string()
    } else {
        z.strftime("%H:%M").to_string()
    }
}

/// The journal code of a limit window.
fn limit_code(kind: LimitKind) -> String {
    match kind {
        LimitKind::FiveHour => "5h",
        LimitKind::SevenDay => "7d",
        LimitKind::Scoped => "scoped",
    }
    .into()
}

fn window_name(kind: LimitKind) -> &'static str {
    match kind {
        LimitKind::FiveHour => "5h",
        LimitKind::SevenDay => "weekly",
        LimitKind::Scoped => "model",
    }
}

/// The longest socket path macOS takes (`sun_path` is 104 bytes).
const MAX_SOCK_PATH: usize = 103;

/// `<run>/ctl.sock`, or a socket in the per-user temp dir when that path
/// is too long for a Unix socket (a deep `AGP_STATE_DIR`).
fn sock_path(run_dir: &Path) -> PathBuf {
    let p = run_dir.join("ctl.sock");
    if p.as_os_str().len() <= MAX_SOCK_PATH {
        return p;
    }
    std::env::temp_dir().join(format!("agp-{}.sock", std::process::id()))
}

/// Listens for Claude's hooks on `sock`. `None` if the socket can't be
/// bound (the run then sees limits through the usage only).
fn listen(sock: &Path) -> Option<mpsc::Receiver<(String, Value)>> {
    if let Some(dir) = sock.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    let _ = std::fs::remove_file(sock);
    let listener = match UnixListener::bind(sock) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("print run: no hook socket: {e}");
            return None;
        }
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
            let mut line = String::new();
            if BufReader::new(conn.take(2 * 1024 * 1024))
                .read_line(&mut line)
                .is_err()
            {
                continue;
            }
            if let Some(ev) = crate::hooks::parse_line(&line)
                && tx.send(ev).is_err()
            {
                return;
            }
        }
    });
    Some(rx)
}

/// SIGINT, SIGTERM and SIGHUP go to the running child; with no child (a
/// wait) agent-prism exits as the signal would.
fn forward_signals() {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP]) else {
        return;
    };
    std::thread::spawn(move || {
        for sig in signals.forever() {
            let pid = CHILD.load(Ordering::SeqCst);
            if pid > 0 {
                // SAFETY: sending a signal to our own child process.
                unsafe { libc::kill(pid, sig) };
            } else {
                std::process::exit(128 + sig);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        assert_eq!(limit_code(LimitKind::SevenDay), "7d");
        assert_eq!(window_name(LimitKind::FiveHour), "5h");
        assert!(!clock(0).is_empty());
    }
}
