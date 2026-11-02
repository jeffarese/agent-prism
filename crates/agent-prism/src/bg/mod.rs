//! Claude Code background sessions (`claude --bg`, DESIGN §11d): the
//! launch (`agp … --bg …`), the watcher that handles their limits, and
//! `agp bg [--json] | watch | move`.

mod claude;
mod job;
mod mover;
mod record;
mod settings;
mod watch;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use prism_core::journal::{EventKind, Recorder, RunMode};
use prism_core::model::{Policy, Tool};
use prism_core::profile::Profile;

use self::claude::Claude;
use self::job::JobState;
use self::mover::{Env, Outcome, Plan, Session};
use self::record::{Launch, Lock, OnLimit, Options, Record, State};
use crate::app::AppCtx;
use crate::cli::{BgCommand, LaunchArgs, RunOptions};
use crate::launch::unix_now;
use crate::session::Prepared;

pub(crate) use self::claude::spawn_detached;
pub(crate) use self::settings::value as settings_value;

/// How long a launch waits for the job's `state.json`.
const JOB_WAIT: Duration = Duration::from_secs(5);
/// How long `claude --bg`'s stdout is read after it exited.
const DRAIN: Duration = Duration::from_secs(1);
/// The time budget of `agp bg move`.
const MOVE_BUDGET: Duration = Duration::from_secs(60);

pub fn run(ctx: &AppCtx, cmd: BgCommand) -> Result<i32> {
    match cmd {
        BgCommand::List { json } => list(ctx, json),
        BgCommand::Watch {
            session,
            as_profile,
            policy,
            run,
        } => watch_cmd(ctx, &session, as_profile.as_deref(), policy, &run),
        BgCommand::Move { session, to, json } => move_cmd(ctx, &session, to.as_deref(), json),
        BgCommand::Watcher { session_id } => watch::run(ctx, &session_id),
    }
}

/// `agp [opts] --bg <claude args…>`: pick the account as print mode does,
/// run `claude --bg …` there (stdin, stdout, stderr and the exit code pass
/// through), then leave a detached watcher for the session.
pub fn launch(ctx: &AppCtx, args: &LaunchArgs) -> Result<i32> {
    let key = settings::new_key();
    let acct = match crate::session::prepare_print(ctx, args, Some(&key))? {
        Prepared::Exit(code) => return Ok(code),
        Prepared::Plain(cmd) => return crate::session::exec_print(cmd, Tool::Claude),
        Prepared::Account(a) => a,
    };
    let crate::session::PrintAccount {
        cmd,
        config,
        profile,
        reason,
        user_settings,
        cwd,
        ..
    } = *acct;
    // The pick's claim holds the account until the session's record
    // counts it (`running`), and goes when the launch returns.
    let _claim = LaunchClaim(&ctx.paths);
    crate::launch::sync_shared(&ctx.paths, &config, &profile);
    let started = unix_now();
    let (code, stdout) = passthrough(cmd)?;
    if code != 0 {
        log::info!("bg launch on {} exited {code}", profile.id);
        return Ok(code);
    }
    let on_limit = OnLimit::effective(args.run.on_limit, config.balance.on_limit);
    let claude = Claude {
        bin: ctx.claude_bin(&config)?,
    };
    let launched = job::parse_launch_output(&stdout);
    let short = launched
        .as_ref()
        .map(|l| l.short.clone())
        .or_else(|| newest_job(&profile, &cwd, started));
    let Some(short) = short else {
        note("could not tell which background session started; its limits are not handled");
        return Ok(code);
    };
    let Some((sid, job)) = find_job(&claude, &profile, &short, JOB_WAIT) else {
        note(&format!(
            "no job state for {short} in {}; its limits are not handled",
            profile.label
        ));
        return Ok(code);
    };
    let journal = Recorder::new(ctx.paths.clone(), Tool::Claude, RunMode::Bg, started);
    let mut ev = journal.event(EventKind::Start, &profile.id, started);
    ev.reason = Some(reason);
    ev.session = Some(sid.clone());
    ev.cwd = Some(cwd.to_string_lossy().into_owned());
    journal.write(ev);
    log::info!(
        "bg {short}: launched on {} (on limit: {on_limit:?})",
        profile.id
    );
    let added = crate::session::added_args(&config, &profile);
    let req = WatchReq {
        profile: &profile,
        short: &short,
        sid: &sid,
        job: &job,
        name: launched.and_then(|l| l.name),
        options: Options::new(&args.run, args.policy),
        on_limit,
        launch: Some(Launch {
            user_settings: settings::user_value(user_settings, &cwd),
            key: Some(key),
            added: [(profile.id.clone(), added)].into(),
        }),
        run: Some(journal.run().to_string()),
    };
    // `stay`: only the record (its settings file can be written again by
    // `agp bg watch`, which also arms a watcher), no watcher.
    let res = if on_limit == OnLimit::Stay {
        write_record(ctx, &req).map(|_| ())
    } else {
        ensure_watcher(ctx, &req).map(|_| ())
    };
    if let Err(e) = res {
        note(&format!("recording {short}: {e:#}"));
    }
    Ok(code)
}

/// Removes the launch's claim from the run registry when dropped.
struct LaunchClaim<'a>(&'a prism_core::paths::Paths);

impl Drop for LaunchClaim<'_> {
    fn drop(&mut self) {
        prism_core::registry::remove_record(self.0, std::process::id());
    }
}

/// A launched job counts as running this long before its state is known.
const JUST_LAUNCHED_SECS: i64 = 120;

/// The profile and weight of every background session agp knows that uses
/// its account now: a watched (or `stay`) session whose job is working, or
/// that was launched moments ago. Sessions agp is moving are counted by the
/// mover's claim. Read-only, for the balancer (DESIGN §6.6).
pub fn running(paths: &prism_core::paths::Paths, now: i64) -> Vec<(String, f64)> {
    let Ok(rd) = std::fs::read_dir(record::dir(paths)) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let sid = name.strip_suffix(".json")?;
            if sid.starts_with('.') {
                return None;
            }
            let r = record::load(paths, sid)?;
            if r.state != State::Watching || r.short_id.is_empty() {
                return None;
            }
            let dir = r
                .config_dir
                .as_ref()
                .map_or_else(|| paths.home.join(".claude"), PathBuf::from);
            let working = match job::read_state(&dir, &r.short_id) {
                Some(st) => st.state == "working",
                None => now - r.created_at < JUST_LAUNCHED_SECS,
            };
            let weight = r.load.filter(|w| *w > 0.0).unwrap_or(1.0);
            working.then_some((r.profile, weight))
        })
        .collect()
}

/// The account of the background session `id` (a session or short id)
/// agp knows, for resume affinity.
pub fn session_profile(paths: &prism_core::paths::Paths, id: &str) -> Option<String> {
    let r = record::find(paths, id)?;
    (!r.profile.is_empty()).then_some(r.profile)
}

/// One stderr note (stdout stays claude's).
fn note(text: &str) {
    log::warn!("bg: {text}");
    eprintln!("agp: {text}");
}

/// Runs `cmd` with stdin and stderr inherited and stdout copied through
/// as it comes; returns the exit code and stdout.
fn passthrough(mut cmd: Command) -> Result<(i32, String)> {
    cmd.stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdout(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agp: starting claude: {e}");
            return Ok((127, String::new()));
        }
    };
    let buf = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    if let Some(mut so) = child.stdout.take() {
        let buf = buf.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            let mut out = std::io::stdout();
            while let Ok(n) = so.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let _ = out.write_all(&chunk[..n]);
                let _ = out.flush();
                buf.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(&chunk[..n]);
            }
            let _ = tx.send(());
        });
    }
    let status = child.wait().context("waiting for claude")?;
    // A supervisor that claude started may keep the pipe: don't wait for it.
    let _ = rx.recv_timeout(DRAIN);
    let text = String::from_utf8_lossy(&buf.lock().unwrap_or_else(|e| e.into_inner())).into_owned();
    Ok((claude::exit_code(status), text))
}

/// The short id of the newest job created in `profile` for `cwd` since
/// `since`, when the launch output could not be read.
fn newest_job(profile: &Profile, cwd: &Path, since: i64) -> Option<String> {
    let rd = std::fs::read_dir(profile.dir.join("jobs")).ok()?;
    let cwd = cwd.to_string_lossy();
    rd.flatten()
        .filter_map(|e| {
            let short = e.file_name().to_string_lossy().into_owned();
            let j = job::read_state(&profile.dir, &short)?;
            let created = j.created_at.as_deref().and_then(job::iso_to_unix)?;
            (created >= since - 5 && j.cwd.as_deref() == Some(cwd.as_ref()))
                .then_some((created, short))
        })
        .max()
        .map(|(_, s)| s)
}

/// The session id and state of job `short` in `profile`, waiting up to
/// `wait` for the state file; else from `claude agents`.
fn find_job(
    claude: &Claude,
    profile: &Profile,
    short: &str,
    wait: Duration,
) -> Option<(String, JobState)> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(j) = job::read_state(&profile.dir, short)
            && let Some(sid) = j.session_id.clone()
        {
            return Some((sid, j));
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let a = claude
        .agents(profile)
        .ok()?
        .into_iter()
        .find(|a| a.id == short)?;
    let j = job::read_state(&profile.dir, short).unwrap_or_default();
    Some((a.session_id?, j))
}

/// What a watch needs to know.
struct WatchReq<'a> {
    profile: &'a Profile,
    short: &'a str,
    sid: &'a str,
    job: &'a JobState,
    name: Option<String>,
    options: Options,
    on_limit: OnLimit,
    /// Set by `agp --bg`.
    launch: Option<Launch>,
    run: Option<String>,
}

/// Writes the record and starts the detached watcher unless one runs (it
/// reads the new options from the record).
fn ensure_watcher(ctx: &AppCtx, req: &WatchReq) -> Result<(Record, bool)> {
    let rec = write_record(ctx, req)?;
    if record::watcher_alive(&ctx.paths, req.sid) {
        log::info!("bg {}: a watcher runs already; options updated", req.short);
        return Ok((rec, false));
    }
    let exe = std::env::current_exe().context("locating the agp binary")?;
    let mut cmd = Command::new(exe);
    cmd.args(["bg", "_watch", req.sid]);
    let pid = claude::spawn_detached(cmd)?;
    log::info!("bg {}: watcher started (pid {pid})", req.short);
    Ok((rec, true))
}

/// The record of a launch or a watch request.
fn write_record(ctx: &AppCtx, req: &WatchReq) -> Result<Record> {
    record::update(&ctx.paths, req.sid, |r| {
        r.short_id = req.short.to_string();
        r.name = req
            .name
            .clone()
            .or_else(|| req.job.name.clone())
            .or(r.name.take());
        if r.state != State::Moving {
            r.set_profile(req.profile);
        }
        r.cwd = req.job.cwd.clone().or(r.cwd.take());
        if r.on_limit != req.on_limit {
            // A new mode: plan the current limit again.
            r.limit_key = None;
        }
        r.on_limit = req.on_limit;
        r.options = req.options.clone();
        if req.launch.is_some() {
            r.launch = req.launch.clone();
            r.load = crate::launch::claim_load();
        }
        if req.run.is_some() {
            r.run = req.run.clone();
        }
        if matches!(r.state, State::Ended | State::Dormant) {
            r.state = State::Watching;
            r.note = Some("watch re-armed".into());
        }
        if req.on_limit == OnLimit::Stay && req.launch.is_some() {
            r.note = Some("on limit: stay, so no watcher; `agp bg watch` arms one".into());
        }
    })
}

/// The profile and short id of a session given by session or short id:
/// `as_profile`, else the profile whose `jobs/` has it. After a move the
/// old account keeps its stopped job; the newest state file is where the
/// session last ran (a `claude respawn` there counts).
fn locate(
    ctx: &AppCtx,
    profiles: &[Profile],
    id: &str,
    as_profile: Option<&str>,
) -> Result<(Profile, String, Option<Record>)> {
    anyhow::ensure!(job::valid_id(id), "{id:?} is not a session id");
    let rec = record::find(&ctx.paths, id);
    let short = rec
        .as_ref()
        .map(|r| r.short_id.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| job::short_of(id));
    let has = |p: &Profile| job::job_dir(&p.dir, &short).is_dir();
    if let Some(a) = as_profile {
        let p = profiles
            .iter()
            .find(|p| p.id == a)
            .cloned()
            .with_context(|| format!("no Claude account {a:?} in the config"))?;
        anyhow::ensure!(has(&p), "{} has no background session {short}", p.label);
        return Ok((p, short, rec));
    }
    let p = profiles
        .iter()
        .filter(|p| has(p))
        .max_by_key(|p| job::state_mtime(&p.dir, &short))
        .cloned()
        .with_context(|| format!("no account has a background session {short}"))?;
    Ok((p, short, rec))
}

/// `agp bg watch <session> [--as <id>] [run opts]`.
fn watch_cmd(
    ctx: &AppCtx,
    id: &str,
    as_profile: Option<&str>,
    policy: Option<Policy>,
    run: &RunOptions,
) -> Result<i32> {
    let mut config = ctx.load_config()?;
    run.apply(&mut config, Tool::Claude)?;
    let profiles = config.profiles_for(&ctx.paths, Tool::Claude);
    let (profile, short, rec) = locate(ctx, &profiles, id, as_profile)?;
    let job = job::read_state(&profile.dir, &short)
        .with_context(|| format!("no readable state for {short} in {}", profile.label))?;
    let sid = job
        .session_id
        .clone()
        .or_else(|| rec.as_ref().map(|r| r.session_id.clone()))
        .filter(|s| !s.is_empty())
        .with_context(|| format!("no session id for {short}"))?;
    anyhow::ensure!(job::valid_id(&sid), "bad session id {sid:?} for {short}");
    let on_limit = OnLimit::effective(run.on_limit, config.balance.on_limit);
    // A session agent-prism did not launch gets its own run in the journal.
    let new_run = rec.as_ref().and_then(|r| r.run.clone()).is_none().then(|| {
        let now = unix_now();
        let j = Recorder::new(ctx.paths.clone(), Tool::Claude, RunMode::Bg, now);
        let mut ev = j.event(EventKind::Start, &profile.id, now);
        ev.reason = Some("agp bg watch".into());
        ev.session = Some(sid.clone());
        ev.cwd = job.cwd.clone();
        j.write(ev);
        j.run().to_string()
    });
    let req = WatchReq {
        profile: &profile,
        short: &short,
        sid: &sid,
        job: &job,
        name: None,
        options: Options::new(run, policy),
        on_limit,
        launch: None,
        run: new_run,
    };
    // Before a `claude respawn`: the respawned process reads this file.
    if let Some(l) = rec.as_ref().and_then(|r| r.launch.as_ref())
        && let Some(key) = &l.key
    {
        let cwd = job
            .cwd
            .clone()
            .or_else(|| rec.as_ref().and_then(|r| r.cwd.clone()));
        let cwd = cwd.map(PathBuf::from).unwrap_or_default();
        match settings::refresh(&ctx.paths, key, &profile, &cwd, l.user_settings.as_deref()) {
            Ok(true) => log::info!("bg {short}: settings for {} written again", profile.id),
            Ok(false) => {}
            Err(e) => note(&format!("writing the settings of {short} again: {e:#}")),
        }
    }
    let (rec, started) = ensure_watcher(ctx, &req)?;
    let name = rec
        .name
        .as_deref()
        .map(|n| format!(" ({n})"))
        .unwrap_or_default();
    if started {
        println!("watching {short}{name} on {}", profile.label);
    } else {
        println!("a watcher already runs for {short}{name}; options updated");
    }
    Ok(0)
}

/// `agp bg move <session> [--to <id>] [--json]`: moves now, in the
/// foreground, under the same action lock the watcher takes.
fn move_cmd(ctx: &AppCtx, id: &str, to: Option<&str>, json: bool) -> Result<i32> {
    let deadline = Instant::now() + MOVE_BUDGET;
    let opts = record::find(&ctx.paths, id)
        .map(|r| r.options)
        .unwrap_or_default();
    let env = Env::load(ctx, &opts)?;
    let (from, short, rec) = locate(ctx, &env.profiles, id, None)?;
    if let Some(w) = rec.as_ref().and_then(|r| r.load) {
        crate::launch::set_run_weight(w, "learned at its launch".into());
    }
    let job = job::read_state(&from.dir, &short)
        .with_context(|| format!("no readable state for {short} in {}", from.label))?;
    let sid = job
        .session_id
        .clone()
        .or_else(|| rec.as_ref().map(|r| r.session_id.clone()))
        .filter(|s| job::valid_id(s))
        .with_context(|| format!("no session id for {short}"))?;
    let _act = Lock::wait(
        &record::action_lock(&ctx.paths, &sid),
        Duration::from_secs(10),
    )?
    .context("another move of this session is running; try again")?;
    let cwd = job
        .cwd
        .clone()
        .or_else(|| rec.as_ref().and_then(|r| r.cwd.clone()))
        .map(PathBuf::from)
        .context("the job has no cwd")?;
    // The record before the move, so `agp bg --json` shows `moving`.
    let rec = record::update(&ctx.paths, &sid, |r| {
        r.short_id = short.clone();
        if r.profile.is_empty() {
            r.set_profile(&from);
            r.on_limit = OnLimit::effective(opts.run().on_limit, env.config.balance.on_limit);
        }
        r.name = r.name.take().or_else(|| job.name.clone());
        r.cwd = Some(cwd.to_string_lossy().into_owned());
    })?;
    let rec = if rec.run.is_none() {
        let now = unix_now();
        let j = Recorder::new(ctx.paths.clone(), Tool::Claude, RunMode::Bg, now);
        let mut ev = j.event(EventKind::Start, &from.id, now);
        ev.reason = Some("agp bg move".into());
        ev.session = Some(sid.clone());
        ev.cwd = Some(cwd.to_string_lossy().into_owned());
        j.write(ev);
        let run = j.run().to_string();
        record::update(&ctx.paths, &sid, |r| r.run = Some(run))?
    } else {
        rec
    };
    let s = Session {
        sid: sid.clone(),
        short: short.clone(),
        from: from.clone(),
        job,
        cwd,
        launch: rec.launch.clone(),
        run: rec.run.clone(),
        created_at: rec.created_at,
    };
    let targets = match to {
        Some(t) => {
            let p = env
                .profile(t)
                .with_context(|| format!("no Claude account {t:?} in the config"))?;
            anyhow::ensure!(p.id != from.id, "the session runs in {} already", p.label);
            anyhow::ensure!(
                mover::shares_projects(&from, &p),
                "{} does not share projects/ with {}; run agp adopt {}",
                p.label,
                from.label,
                p.id
            );
            anyhow::ensure!(
                mover::trusted(&env, &p, &s.cwd),
                "{} does not trust {}",
                p.label,
                s.cwd.display()
            );
            vec![p]
        }
        None => mover::candidates(&env, &s),
    };
    if targets.is_empty() {
        bail!(
            "no other account to move {short} to (none free that shares projects/ with {} and trusts {})",
            from.label,
            s.cwd.display()
        );
    }
    let config = &env.config;
    let prompt = |p: &Profile| mover::prompt(config, &mover::move_note(&from, p, mover::MANUAL));
    let plan = Plan {
        targets: &targets,
        reason: mover::MANUAL,
        prompt: &prompt,
        deadline,
        on_fail: None,
    };
    let to = match mover::relocate(&env, &s, &plan) {
        Outcome::Moved(p) => p,
        Outcome::Woke => unreachable!("a move has targets"),
        Outcome::Aborted(p) => bail!("move aborted: the session runs again in {}", p.label),
        Outcome::Failed(e) => {
            // Don't leave it stopped: go on where it was.
            let label = from.label.clone();
            let back = |_: &Profile| {
                mover::prompt(
                    config,
                    &format!(
                        "A move to another account failed, so this session goes on on {label}"
                    ),
                )
            };
            let plan = Plan {
                targets: &[],
                reason: mover::MANUAL,
                prompt: &back,
                deadline: Instant::now() + Duration::from_secs(40),
                on_fail: Some((State::Watching, None)),
            };
            let back = match mover::relocate(&env, &s, &plan) {
                Outcome::Woke => format!("; it goes on in {label}"),
                Outcome::Aborted(p) => format!("; it runs in {}", p.label),
                _ => format!("; it is stopped in {label}"),
            };
            bail!("{e}{back}");
        }
    };
    if json {
        let v = serde_json::json!({
            "moved": true,
            "session_id": sid,
            "short_id": short,
            "from": from.id,
            "from_label": from.label,
            "to": to.id,
            "to_label": to.label,
            "to_config_dir": record::config_dir(&to),
        });
        println!("{v}");
    } else {
        println!("moved {short} from {} to {}", from.label, to.label);
    }
    Ok(0)
}

/// `agp bg [--json]`.
fn list(ctx: &AppCtx, json: bool) -> Result<i32> {
    let records = record::list(&ctx.paths);
    if json {
        let v = serde_json::json!({ "sessions": records });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    if records.is_empty() {
        println!("no background sessions watched (start one with agp --bg …)");
        return Ok(0);
    }
    println!(
        "  {:<9} {:<24} {:<12} {:<9} {:<8} {:<7} NOTE",
        "SESSION", "NAME", "ACCOUNT", "STATE", "ON-LIMIT", "WATCHER"
    );
    let now = unix_now();
    for r in &records {
        let state = match r.state {
            State::Watching => "watching",
            State::Limited => "limited",
            State::Waiting => "waiting",
            State::Moving => "moving",
            State::Ended => "ended",
            State::Dormant => "dormant",
        };
        let on = match r.on_limit {
            OnLimit::Switch => "switch",
            OnLimit::Wait => "wait",
            OnLimit::Stay => "stay",
        };
        let watcher = if r.watching { "on" } else { "off" };
        let mut note = r.note.clone().unwrap_or_default();
        if let Some(t) = r.wait_until.filter(|_| r.state == State::Waiting) {
            note = format!(
                "{note} · next check in {}",
                prism_usage::balance::fmt_duration(t - now)
            );
        }
        let name: String = r
            .name
            .clone()
            .unwrap_or_default()
            .chars()
            .take(24)
            .collect();
        println!(
            "  {:<9} {:<24} {:<12} {:<9} {:<8} {:<7} {note}",
            r.short_id, name, r.label, state, on, watcher
        );
    }
    Ok(0)
}
