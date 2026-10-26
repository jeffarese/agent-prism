//! `agp [args]` / `agp codex [args]`: pick an account and run claude
//! (or codex) in the wrapper, or exec it directly in print mode (codex: a
//! non-interactive line) and in nested agent-prism sessions.

use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use prism_core::config::Config;
use prism_core::journal::{EventKind, Recorder, RunMode};
use prism_core::model::{OnLimit, Tool};
use prism_core::profile::Profile;
use prism_core::registry;
use prism_term::runner::{ExternalEvent, Runner, RunnerOptions};
use prism_usage::balance::fmt_duration;
use prism_usage::poller::{PollTarget, Poller, PollerConfig};

use crate::app::AppCtx;
use crate::cli::{LaunchArgs, LimitAction};
use crate::host::{HostInit, SessionEvent, SessionHost};
use crate::launch::{self, SpecBuilder, Start, unix_now};
use crate::live::Live;
use crate::tokens::KeychainTokens;

/// Asks a yes/no question on the terminal; default no.
pub fn confirm(question: &str) -> bool {
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes")
}

fn exec(mut cmd: Command, tool: Tool) -> Result<i32> {
    let err = cmd.exec();
    Err(err).with_context(|| format!("starting {}", tool.bin()))
}

/// Whether the agent-prism session listening on `sock` is still running. A
/// multiplexer (herdr, tmux) started inside a session keeps its `AGP_SOCK`
/// after that session exits; its panes are not nested.
fn session_alive(sock: &Path) -> bool {
    UnixStream::connect(sock).is_ok()
}

/// Execs the tool for print mode; if that fails, exits 127 like a shell.
pub(crate) fn exec_print(mut cmd: Command, tool: Tool) -> Result<i32> {
    let err = cmd.exec();
    eprintln!("agp: starting {}: {err}", tool.bin());
    Ok(127)
}

/// Execs this binary again with `argv[0] = claude` (or `codex`; same pid
/// and args) unless it already is. agent-prism stays the foreground process of
/// the terminal for the whole session, and terminals such as Rex pick the
/// tab icon and name from the base name of its argv[0]. If the exec fails,
/// agent-prism goes on as it is.
fn reexec_as(tool: Tool) {
    let mut argv = std::env::args_os();
    let Some(arg0) = argv.next() else { return };
    let name = tool.bin();
    if std::path::Path::new(&arg0).file_name() == Some(name.as_ref()) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let err = Command::new(exe).arg0(name).args(argv).exec();
    log::warn!("re-exec as {name}: {err}");
}

/// Removes the run dir and the registry record when the session ends.
struct RunGuard {
    ctx: Arc<AppCtx>,
    run_dir: PathBuf,
    sock: PathBuf,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock);
        let _ = std::fs::remove_dir_all(&self.run_dir);
        registry::remove_record(&self.ctx.paths, std::process::id());
    }
}

/// The launch entry point.
pub fn run(ctx: Arc<AppCtx>, args: LaunchArgs) -> Result<i32> {
    let tool = args.tool;
    if args.print_mode() {
        if args.run.on_limit.is_some() {
            return crate::supervise::run(&ctx, &args);
        }
        return run_print(&ctx, &args);
    }
    if args.run.on_limit == Some(LimitAction::Fail) {
        bail!("--on-limit fail is for -p and codex exec; use stay or wait in a session");
    }
    // Nested agent-prism (interactive): run the plain tool.
    if prism_core::env::var_os("AGP_SOCK").is_some_and(|s| session_alive(Path::new(&s))) {
        eprintln!(
            "agp: already inside an agent-prism session; starting plain {}",
            tool.bin()
        );
        let config = ctx.load_config()?;
        let mut cmd = Command::new(ctx.tool_bin(&config, tool)?);
        cmd.args(&args.tool_args);
        return exec(cmd, tool);
    }
    reexec_as(tool);
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    // A config with no accounts (e.g. only a backup target) also starts the
    // onboarding; the wizard keeps its other settings.
    let mut config = match Config::load(&ctx.paths)?.filter(|c| !c.profiles.is_empty()) {
        Some(c) => c,
        None if tty && !args.print_mode() => match crate::ui::run_onboarding(ctx.clone())? {
            Some(c) => c,
            None => return Ok(0),
        },
        None => bail!(
            "no config at {}; run agp setup",
            ctx.paths.config_file().display()
        ),
    };
    args.run.apply(&mut config, tool)?;
    let profiles = config.profiles_for(&ctx.paths, tool);
    if profiles.is_empty() {
        let flag = if tool.is_claude() { "" } else { " --codex" };
        bail!(
            "no {} accounts configured; run agp setup or agp add{flag}",
            tool.name()
        );
    }
    let tokens = Arc::new(KeychainTokens::new(ctx.store.clone(), profiles.clone()));
    let (user_args, user_settings) = match tool {
        Tool::Claude => launch::take_user_settings(&args.tool_args),
        Tool::Codex => (args.tool_args.clone(), None),
    };
    let common = config.common_home_for(&ctx.paths, tool);
    let model = tool
        .is_claude()
        .then(|| launch::session_model(&user_args, &common))
        .flatten();

    let profile = choose(&ctx, &config, &profiles, &tokens, &args, model.clone(), tty)?;
    let Some((profile, reason)) = profile else {
        return Ok(1);
    };
    log::info!("starting {} on {}", tool.bin(), profile.id);
    let program = ctx.tool_bin(&config, tool)?;
    let now = unix_now();
    let journal = Recorder::new(ctx.paths.clone(), tool, RunMode::Tui, now);
    launch::touch_last_used(&ctx.paths, &profile.id, now);

    let user_settings = user_settings
        .as_deref()
        .map(launch::load_user_settings)
        .transpose()?;
    let pid = std::process::id();
    let run_dir = ctx.paths.run_dir(pid);
    let mut guard = RunGuard {
        ctx: ctx.clone(),
        run_dir: run_dir.clone(),
        sock: PathBuf::new(),
    };
    let builder = SpecBuilder {
        paths: ctx.paths.clone(),
        tool,
        program,
        user_args,
        user_settings,
        run_dir,
        exe: std::env::current_exe().context("locating the agp binary")?,
        cwd: std::env::current_dir().context("current dir")?,
        memory_key: Default::default(),
    };
    guard.sock = builder.sock();
    let (spec, session_id) = builder.build(&config, &profile, &Start::Fresh)?;
    let mut ev = journal.event(EventKind::Start, &profile.id, now);
    ev.reason = Some(reason);
    ev.session = session_id.clone();
    ev.cwd = Some(builder.cwd.to_string_lossy().into_owned());
    journal.write(ev);

    let dock_mode = args.dock.unwrap_or(config.dock.mode);
    let (mut runner, tx) = Runner::<SessionEvent, crate::actions::UiAction>::new(RunnerOptions {
        initial_dock_mode: dock_mode,
        ..Default::default()
    });
    crate::hooks::serve(&builder.sock(), tx.clone())?;

    let poller = (!launch::polling_disabled()).then(|| {
        let tx = tx.clone();
        Poller::spawn(
            PollerConfig {
                cache_dir: ctx.paths.usage_dir(),
                poll_active_secs: config.usage.poll_active_secs,
                poll_idle_secs: config.usage.poll_idle_secs,
            },
            profiles
                .iter()
                .map(|p| PollTarget {
                    profile_id: p.id.clone(),
                    active: p.id == profile.id,
                })
                .collect(),
            tokens.clone(),
            Box::new(move |u| {
                let _ = tx.send(ExternalEvent::Custom(SessionEvent::Usage(u)));
            }),
        )
    });

    let mut host = SessionHost::new(HostInit {
        ctx: ctx.clone(),
        live: Live::default(),
        config,
        tokens,
        builder,
        model,
        policy: args.policy,
        dock_mode,
        tool,
        active: profile.id.clone(),
        session_id,
        poller,
        run: args.run.clone(),
        journal: journal.clone(),
    });
    host.write_registry();
    let outcome = runner.run(&mut host, spec)?;
    let (active, session) = host.journal_end();
    journal.end(&active, unix_now(), outcome.exit_code, session);
    drop(host);
    drop(guard);
    Ok(outcome.exit_code)
}

/// Print mode: like `claude -p`, plus the account env and settings (codex:
/// a non-interactive line plus the account env). Never prompts, never
/// blocks; when agent-prism can't pick it still runs the tool.
pub(crate) fn run_print(ctx: &AppCtx, args: &LaunchArgs) -> Result<i32> {
    let tool = args.tool;
    let loaded = Config::load(&ctx.paths);
    let (config, reason) = match loaded {
        Ok(Some(c)) if c.profiles.iter().any(|p| p.tool == tool) => (c, None),
        Ok(Some(c)) => (c, Some("no accounts configured".to_string())),
        Ok(None) => (Config::default(), Some("no config".to_string())),
        Err(e) => (
            Config::default(),
            Some(format!(
                "config error: {}",
                format!("{e:#}")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            )),
        ),
    };
    let program = match ctx.tool_bin(&config, tool) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agp: {e:#}");
            return Ok(127);
        }
    };
    let plain = |why: &str| {
        eprintln!(
            "agp: {why}; running {} without account selection",
            tool.bin()
        );
        log::warn!("print mode without account: {why}");
        let mut cmd = Command::new(&program);
        cmd.args(&args.tool_args);
        exec_print(cmd, tool)
    };
    if let Some(why) = reason {
        return plain(&why);
    }
    let profiles = config.profiles_for(&ctx.paths, tool);
    if profiles.is_empty() {
        return plain("no accounts configured");
    }
    let tokens = Arc::new(KeychainTokens::new(ctx.store.clone(), profiles.clone()));
    let (user_args, user_settings) = match tool {
        Tool::Claude => launch::take_user_settings(&args.tool_args),
        Tool::Codex => (args.tool_args.clone(), None),
    };
    let profile = if let Some(id) = &args.as_profile {
        profiles
            .iter()
            .find(|p| &p.id == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no profile {id:?} in the config"))?
    } else {
        let model = tool
            .is_claude()
            .then(|| launch::session_model(&user_args, &config.common_home(&ctx.paths)))
            .flatten();
        let snap = launch::snapshot(ctx, &config, &profiles, &tokens, args.policy, model);
        match snap
            .pick
            .chosen
            .as_ref()
            .and_then(|id| profiles.iter().find(|p| &p.id == id))
        {
            Some(p) => {
                log::info!("pick {}: {}", p.id, snap.pick.reason);
                p.clone()
            }
            None => {
                let reset = snap
                    .pick
                    .earliest_reset
                    .as_ref()
                    .and_then(|(label, until)| {
                        let p = profiles.iter().find(|p| &p.label == label)?;
                        Some((p.clone(), *until))
                    });
                let (p, note) = match reset {
                    Some((p, until)) => {
                        let d = fmt_duration(until - snap.ctx.now);
                        eprintln!(
                            "agp: all pool accounts are limited; using {} (resets in {d})",
                            p.label
                        );
                        (p, format!("resets in {d}"))
                    }
                    None => {
                        let p = profiles
                            .iter()
                            .find(|p| p.pool)
                            .unwrap_or(&profiles[0])
                            .clone();
                        let why = snap.pick.reason.clone();
                        eprintln!("agp: no account to pick ({why}); using {}", p.label);
                        (p, why)
                    }
                };
                log::warn!("print mode fallback to {}: {note}", p.id);
                p
            }
        }
    };
    log::info!("print mode on {}", profile.id);
    let now = unix_now();
    launch::touch_last_used(&ctx.paths, &profile.id, now);
    let journal = Recorder::new(ctx.paths.clone(), tool, RunMode::Exec, now);
    let mut ev = journal.event(EventKind::Start, &profile.id, now);
    ev.cwd = std::env::current_dir()
        .ok()
        .map(|d| d.to_string_lossy().into_owned());
    journal.write(ev);
    if tool == Tool::Codex {
        let (_, config_args) = config.tool_bin_args(tool);
        let mut cmd = Command::new(&program);
        cmd.args(config_args).args(&profile.args).args(&user_args);
        let (set, remove) = launch::child_env(&profile, std::path::Path::new(""));
        for k in remove {
            cmd.env_remove(k);
        }
        cmd.envs(set);
        return exec_print(cmd, tool);
    }
    let overlay = ctx.paths.profile_overlay(&profile.id);
    let overlay = overlay.is_file().then_some(overlay);
    let cwd = std::env::current_dir().unwrap_or_default();
    let settings = print_settings(user_settings, overlay.as_deref(), |u, o| {
        launch::memory_dir(&profile, &cwd, &[u, o], |k| std::env::var(k).ok())
    });
    let mut cmd = Command::new(&program);
    cmd.args(print_args(&config, &profile, &user_args, settings));
    let (set, remove) = launch::child_env(&profile, std::path::Path::new(""));
    for k in remove {
        cmd.env_remove(k);
    }
    cmd.envs(set);
    exec_print(cmd, tool)
}

/// The `--settings` value for print mode: the overlay path, the user value,
/// or both merged (user wins) as compact JSON. If merging fails the user
/// value goes through unchanged so claude reports its own error. When
/// `mem` (given the user and overlay values) returns a memory dir, the
/// result is inline JSON with `autoMemoryDirectory` added.
pub(crate) fn print_settings(
    user: Option<String>,
    overlay: Option<&std::path::Path>,
    mem: impl FnOnce(
        Option<&serde_json::Value>,
        Option<&serde_json::Value>,
    ) -> Option<launch::MemoryDir>,
) -> Option<String> {
    let user_v = match user.as_deref().map(launch::load_user_settings) {
        Some(Err(_)) => None,
        Some(Ok(v)) => Some(Some(v)),
        None => Some(None),
    };
    // Memory needs a loadable (or no) user value.
    if let Some(user_v) = user_v {
        let overlay_v: Option<serde_json::Value> = overlay
            .and_then(|o| std::fs::read_to_string(o).ok())
            .and_then(|t| serde_json::from_str(&t).ok());
        if let Some(m) = mem(user_v.as_ref(), overlay_v.as_ref()) {
            let mut v = overlay_v.unwrap_or_else(|| serde_json::json!({}));
            if let Some(u) = &user_v {
                launch::merge_settings(&mut v, u);
            }
            if let Some(o) = v.as_object_mut() {
                o.insert("autoMemoryDirectory".into(), m.dir.into());
                return serde_json::to_string(&v).ok();
            }
        }
    }
    let (user, overlay) = match (user, overlay) {
        (None, None) => return None,
        (None, Some(o)) => return Some(o.to_string_lossy().into_owned()),
        (Some(u), None) => return Some(u),
        (Some(u), Some(o)) => (u, o),
    };
    let merged = (|| -> Result<String> {
        let u = launch::load_user_settings(&user)?;
        let text = std::fs::read_to_string(overlay)?;
        let mut v: serde_json::Value = serde_json::from_str(&text)?;
        launch::merge_settings(&mut v, &u);
        Ok(serde_json::to_string(&v)?)
    })();
    match merged {
        Ok(s) => Some(s),
        Err(e) => {
            log::warn!("print mode: not merging settings: {e:#}");
            Some(user)
        }
    }
}

/// Print-mode args: config, profile, then the user args with agent-prism's
/// `--settings` before any user `--`.
pub(crate) fn print_args(
    config: &Config,
    profile: &Profile,
    user_args: &[String],
    settings: Option<String>,
) -> Vec<String> {
    let mut out = config.claude.args.clone();
    out.extend(profile.args.iter().cloned());
    let extra = settings
        .map(|s| vec!["--settings".into(), s])
        .unwrap_or_default();
    out.extend(launch::splice_args(user_args, extra));
    out
}

/// `--as`, or the balancer's pick (asking when all are limited), with the
/// reason for the journal. With `on_limit = wait` a session starts on the
/// account that resets first and waits there.
fn choose(
    ctx: &AppCtx,
    config: &Config,
    profiles: &[Profile],
    tokens: &Arc<KeychainTokens>,
    args: &LaunchArgs,
    model: Option<String>,
    tty: bool,
) -> Result<Option<(Profile, String)>> {
    if let Some(id) = &args.as_profile {
        return profiles
            .iter()
            .find(|p| &p.id == id)
            .cloned()
            .map(|p| Some((p, "pinned with --as".into())))
            .ok_or_else(|| anyhow::anyhow!("no profile {id:?} in the config"));
    }
    let snap = launch::snapshot(ctx, config, profiles, tokens, args.policy, model);
    if let Some(id) = &snap.pick.chosen {
        log::info!("pick {id}: {}", snap.pick.reason);
        return Ok(profiles
            .iter()
            .find(|p| &p.id == id)
            .cloned()
            .map(|p| (p, snap.pick.reason.clone())));
    }
    let Some((label, until)) = snap.pick.earliest_reset.clone() else {
        bail!("no account to use: {}", snap.pick.reason);
    };
    let fallback = profiles.iter().find(|p| p.label == label).cloned();
    let resets = format!("{label} resets in {}", fmt_duration(until - snap.ctx.now));
    if config.balance.on_limit == OnLimit::Wait
        && let Some(p) = fallback.clone()
    {
        eprintln!("agp: all pool accounts are limited; {resets}; starting there to wait");
        return Ok(Some((p, format!("all limited, {resets}"))));
    }
    let q = format!("All pool accounts are limited; {resets}. Start on it anyway?");
    if tty && fallback.is_some() && confirm(&q) {
        return Ok(fallback.map(|p| (p, format!("all limited, {resets}"))));
    }
    eprintln!(
        "agp: all pool accounts are limited; {label} resets in {}",
        fmt_duration(until - snap.ctx.now)
    );
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn no(
        _: Option<&serde_json::Value>,
        _: Option<&serde_json::Value>,
    ) -> Option<launch::MemoryDir> {
        None
    }

    fn yes(
        _: Option<&serde_json::Value>,
        _: Option<&serde_json::Value>,
    ) -> Option<launch::MemoryDir> {
        Some(launch::MemoryDir {
            dir: "/m".into(),
            key: "k".into(),
            at_base: true,
        })
    }

    #[test]
    fn session_alive_needs_a_listener() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("ctl.sock");
        assert!(!session_alive(&sock));
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(session_alive(&sock));
        // Dropping the listener would race with children that other tests
        // fork; a leftover file with no listener behind it stands in.
        let stale = dir.path().join("stale.sock");
        std::fs::write(&stale, "").unwrap();
        assert!(!session_alive(&stale));
    }

    #[test]
    fn settings_memory() {
        let dir = tempfile::tempdir().unwrap();
        let o = dir.path().join("o.json");
        std::fs::write(&o, r#"{"model":"opus"}"#).unwrap();
        let j = |s: Option<String>| serde_json::from_str::<serde_json::Value>(&s.unwrap()).unwrap();
        assert_eq!(
            j(print_settings(None, None, yes)),
            serde_json::json!({"autoMemoryDirectory":"/m"})
        );
        assert_eq!(
            j(print_settings(None, Some(&o), yes)),
            serde_json::json!({"model":"opus","autoMemoryDirectory":"/m"})
        );
        assert_eq!(
            j(print_settings(Some(r#"{"x":1}"#.into()), Some(&o), yes)),
            serde_json::json!({"model":"opus","x":1,"autoMemoryDirectory":"/m"})
        );
        // The closure sees both values.
        let seen = print_settings(Some(r#"{"x":1}"#.into()), Some(&o), |u, ov| {
            assert_eq!(u.unwrap()["x"], 1);
            assert_eq!(ov.unwrap()["model"], "opus");
            None
        });
        assert!(seen.unwrap().contains("\"x\":1"));
    }

    #[test]
    fn settings_cases() {
        let dir = tempfile::tempdir().unwrap();
        let o = dir.path().join("o.json");
        std::fs::write(&o, r#"{"model":"opus","env":{"A":"1"}}"#).unwrap();
        assert_eq!(print_settings(None, None, no), None);
        assert_eq!(
            print_settings(None, Some(&o), no).unwrap(),
            o.to_string_lossy()
        );
        assert_eq!(
            print_settings(Some("u.json".into()), None, no).unwrap(),
            "u.json"
        );
        let m = print_settings(Some(r#"{"model":"sonnet"}"#.into()), Some(&o), no).unwrap();
        let m: serde_json::Value = serde_json::from_str(&m).unwrap();
        assert_eq!(m["model"], "sonnet");
        assert_eq!(m["env"]["A"], "1");
        // Unreadable user value: passed through for claude to report.
        assert_eq!(
            print_settings(Some("/nope.json".into()), Some(&o), yes).unwrap(),
            "/nope.json"
        );
    }

    #[test]
    fn args_order() {
        let mut config = Config::default();
        config.claude.args = v(&["--ca"]);
        let mut profile = Profile::bare(
            "p",
            Tool::Claude,
            "/h/.claude-p".into(),
            "/h/.claude".into(),
        );
        profile.label = "P".into();
        profile.args = v(&["--pa"]);
        assert_eq!(
            print_args(&config, &profile, &v(&["-p", "hi"]), Some("s".into())),
            v(&["--ca", "--pa", "-p", "hi", "--settings", "s"])
        );
        assert_eq!(
            print_args(&config, &profile, &v(&["-p", "--", "-x"]), Some("s".into())),
            v(&["--ca", "--pa", "-p", "--settings", "s", "--", "-x"])
        );
        assert_eq!(
            print_args(&config, &profile, &v(&["-p"]), None),
            v(&["--ca", "--pa", "-p"])
        );
    }
}
