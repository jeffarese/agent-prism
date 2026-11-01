//! Stopping a background session and resuming it on another account, or
//! waking it on the same one (DESIGN §11d "Moving a session").
//!
//! The order matters: the record says `moving` before the stop, the old
//! process is gone before the resume (else Claude starts a copy), nobody
//! revived the session in between, and the resume in a profile that
//! already has the job uses the no-flag form (flags there start a copy).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use prism_core::config::Config;
use prism_core::journal::{EventKind, Recorder, RunMode};
use prism_core::model::{LimitKind, Limited, Policy, Tool};
use prism_core::profile::Profile;
use prism_usage::balance;

use super::claude::Claude;
use super::job::{self, JobState};
use super::record::{self, Launch, Move, Options, State};
use crate::app::AppCtx;
use crate::launch::{self, unix_now};
use crate::tokens::KeychainTokens;

/// How long the old process may take to stop.
const STOP_WAIT: Duration = Duration::from_secs(20);
/// How long the resumed session may take to show a pid.
const VERIFY_WAIT: Duration = Duration::from_secs(15);

/// Everything a move needs: the config with the watch's run options
/// applied, the Claude accounts and the `claude` binary.
pub struct Env<'a> {
    pub app: &'a AppCtx,
    pub config: Config,
    pub profiles: Vec<Profile>,
    pub tokens: Arc<KeychainTokens>,
    pub claude: Claude,
    pub policy: Option<Policy>,
}

impl<'a> Env<'a> {
    /// The env for a watch with `opts`. Account ids that left the config
    /// since are dropped from the options instead of failing the watch.
    pub fn load(app: &'a AppCtx, opts: &Options) -> Result<Env<'a>> {
        let mut config = app.load_config()?;
        let known: Vec<String> = config
            .profiles
            .iter()
            .filter(|p| p.tool == Tool::Claude)
            .map(|p| p.id.clone())
            .collect();
        let mut run = opts.run();
        run.accounts = run
            .accounts
            .map(|a| a.into_iter().filter(|id| known.contains(id)).collect());
        run.exclude.retain(|id| known.contains(id));
        run.apply(&mut config, Tool::Claude)?;
        let profiles = config.profiles_for(&app.paths, Tool::Claude);
        let tokens = Arc::new(KeychainTokens::new(app.store.clone(), profiles.clone()));
        let claude = Claude {
            bin: app.claude_bin(&config)?,
        };
        Ok(Env {
            app,
            config,
            profiles,
            tokens,
            claude,
            policy: opts.policy,
        })
    }

    pub fn profile(&self, id: &str) -> Option<Profile> {
        self.profiles.iter().find(|p| p.id == id).cloned()
    }

    /// The record's profile; one that left the config since stays
    /// watchable through its `config_dir` (out of the pool).
    pub fn record_profile(&self, rec: &record::Record) -> Profile {
        self.profile(&rec.profile).unwrap_or_else(|| {
            let common = self.config.common_home(&self.app.paths);
            let dir = rec
                .config_dir
                .as_ref()
                .map_or(common.clone(), PathBuf::from);
            let mut p = Profile::bare(&rec.profile, Tool::Claude, dir, common);
            p.label = rec.label.clone();
            p
        })
    }

    /// The model for scoped windows: the session's `--model`, else the
    /// shared settings.
    pub fn model(&self, flags: &[String]) -> Option<String> {
        launch::session_model(flags, &self.config.common_home(&self.app.paths))
    }

    /// The limit of `p` now, its usage refreshed when older than 30 s.
    pub fn limited(&self, p: &Profile, model: Option<String>) -> Option<Limited> {
        let mut fresh = self.config.clone();
        fresh.usage.poll_active_secs = 30;
        launch::refresh_stale(
            &self.app.cache,
            &fresh,
            std::slice::from_ref(p),
            &self.tokens,
            Duration::from_secs(5),
        );
        let now = unix_now();
        let inputs =
            launch::account_inputs(&self.app.paths, &self.profiles, &self.tokens, None, now);
        let usage = launch::load_usage(&self.app.cache, &self.profiles);
        let vctx = launch::view_ctx(&self.config, self.policy, model, None, now);
        prism_usage::views::build_views(&inputs, &usage, &HashMap::new(), &vctx)
            .into_iter()
            .find(|v| v.profile_id == p.id)
            .and_then(|v| v.limited)
    }

    /// The run journal of the session, when it has a run.
    pub fn journal(&self, run: Option<&str>, started_at: i64) -> Option<Recorder> {
        run.map(|r| {
            Recorder::for_run(
                self.app.paths.clone(),
                r.to_string(),
                Tool::Claude,
                RunMode::Bg,
                started_at,
            )
        })
    }
}

/// A session to move, where it is now.
#[derive(Clone, Debug)]
pub struct Session {
    pub sid: String,
    pub short: String,
    pub from: Profile,
    pub job: JobState,
    /// The folder the session was launched in; a resume runs there.
    pub cwd: PathBuf,
    pub launch: Option<Launch>,
    pub run: Option<String>,
    pub created_at: i64,
}

/// The window of a limit in words, for notes and `moves[].reason`.
pub fn window_words(kind: Option<LimitKind>) -> &'static str {
    match kind {
        Some(LimitKind::FiveHour) => "5-hour limit",
        Some(LimitKind::SevenDay) => "weekly limit",
        Some(LimitKind::Scoped) => "model limit",
        None => "limit",
    }
}

/// The journal code of a window.
pub fn limit_code(kind: LimitKind) -> String {
    match kind {
        LimitKind::FiveHour => "5h",
        LimitKind::SevenDay => "7d",
        LimitKind::Scoped => "scoped",
    }
    .into()
}

/// `<dir>/projects` of both profiles is the same dir, so the transcript
/// of the session is visible in both (DESIGN §9 step 7).
pub fn shares_projects(a: &Profile, b: &Profile) -> bool {
    let real = |p: &Profile| p.dir.join("projects").canonicalize().ok();
    matches!((real(a), real(b)), (Some(x), Some(y)) if x == y)
}

/// The git toplevel of `dir`: Claude counts a trusted parent only up to it.
fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// Whether `target` trusts `cwd`, after giving it the other accounts'
/// trust (`share.sync_trust`, §4.7). A background session can't answer
/// the trust dialog.
pub fn trusted(env: &Env, target: &Profile, cwd: &Path) -> bool {
    if env.config.share.sync_trust {
        prism_core::trust::sync(&env.app.paths, target, &env.profiles);
    }
    prism_core::trust::trusts(&env.app.paths, target, cwd, repo_root(cwd).as_deref())
}

/// The accounts to move `s` to, best first: the balancer's picks among
/// the pool (with the run options) without the current account, only
/// those that share `projects/` with it and trust the session's folder.
pub fn candidates(env: &Env, s: &Session) -> Vec<Profile> {
    let pool: Vec<Profile> = env.profiles.iter().filter(|p| p.pool).cloned().collect();
    launch::refresh_stale(
        &env.app.cache,
        &env.config,
        &pool,
        &env.tokens,
        Duration::from_millis(2500),
    );
    let now = unix_now();
    let mut inputs = launch::account_inputs(&env.app.paths, &env.profiles, &env.tokens, None, now);
    let usage = launch::load_usage(&env.app.cache, &env.profiles);
    let vctx = launch::view_ctx(
        &env.config,
        env.policy,
        env.model(&s.job.respawn_flags),
        Some(s.from.id.clone()),
        now,
    );
    for i in inputs.iter_mut() {
        let ok = env
            .profile(&i.profile_id)
            .is_some_and(|p| shares_projects(&s.from, &p));
        i.in_pool &= ok;
    }
    let mut out = Vec::new();
    while let Some(id) = balance::pick(&inputs, &usage, &vctx).chosen {
        if let Some(i) = inputs.iter_mut().find(|i| i.profile_id == id) {
            i.in_pool = false;
        }
        match env.profile(&id) {
            Some(p) if trusted(env, &p, &s.cwd) => out.push(p),
            Some(p) => log::info!(
                "bg {}: {} does not trust {}",
                s.short,
                p.id,
                s.cwd.display()
            ),
            None => {}
        }
    }
    out
}

/// The flags of a resume in `target` (a profile with no job for the
/// session): the saved launch flags. When agent-prism launched the
/// session, the args it added for the old account give way to the
/// target's (returned too, for the record) and its `--settings` is built
/// again for `target`.
fn flags_for(env: &Env, s: &Session, target: &Profile) -> (Vec<String>, Option<Vec<String>>) {
    let flags = s.job.respawn_flags.clone();
    let Some(l) = &s.launch else {
        return (flags, None);
    };
    let old = l.added.get(&s.from.id).map_or(&[][..], Vec::as_slice);
    let added = crate::session::added_args(&env.config, target);
    let mut out = added.clone();
    out.extend(job::remove_args(&flags, old));
    let settings = match &l.key {
        Some(key) => super::settings::value(
            &env.app.paths,
            key,
            target,
            &s.cwd,
            l.user_settings.as_deref(),
        ),
        None => {
            let overlay = env.app.paths.profile_overlay(&target.id);
            let overlay = overlay.is_file().then_some(overlay);
            crate::session::print_settings(l.user_settings.clone(), overlay.as_deref(), |u, o| {
                launch::memory_dir(target, &s.cwd, &[u, o], |k| std::env::var(k).ok())
            })
        }
    };
    (job::replace_settings(&out, settings), Some(added))
}

/// Before a resume in `target`: the flags of the flags form (`None`: the
/// wake form). Records the args agent-prism adds there, and for a wake
/// writes the session's settings file for `target` again (the woken job's
/// saved flags name it).
fn prepare_resume(env: &Env, s: &Session, target: &Profile, has_job: bool) -> Option<Vec<String>> {
    let paths = &env.app.paths;
    if has_job {
        if let Some(l) = &s.launch
            && let Some(key) = &l.key
            && let Err(e) =
                super::settings::refresh(paths, key, target, &s.cwd, l.user_settings.as_deref())
        {
            log::warn!(
                "bg {}: writing the settings for {} again: {e:#}",
                s.short,
                target.id
            );
        }
        return None;
    }
    let (flags, added) = flags_for(env, s, target);
    if let Some(added) = added
        && let Err(e) = record::update(paths, &s.sid, |r| {
            if let Some(l) = &mut r.launch {
                l.added.insert(target.id.clone(), added);
            }
        })
    {
        log::warn!(
            "bg {}: recording the args for {}: {e:#}",
            s.short,
            target.id
        );
    }
    Some(flags)
}

/// The first prompt after a stop: a one-line note, a blank line, then the
/// config's `resume_prompt` (§9).
pub fn prompt(config: &Config, note: &str) -> String {
    let resume = match config.balance.resume_prompt.as_str() {
        "" => "continue",
        r => r,
    };
    format!(
        "[agent-prism] {note}; the old process's background tasks and subagents stopped, \
         so start again the ones you still need.\n\n{resume}"
    )
}

/// The note of a move from `from` to `to` for `reason`.
pub fn move_note(from: &Profile, to: &Profile, reason: &str) -> String {
    let why = if reason == MANUAL {
        " on request".to_string()
    } else {
        format!(" after the {reason}")
    };
    format!(
        "This session moved from account {} to {}{why}",
        from.label, to.label
    )
}

/// `moves[].reason` of `agp bg move`.
pub const MANUAL: &str = "manual move";

/// How a stop-and-resume ended.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Resumed in this other account.
    Moved(Profile),
    /// Woken in the same account.
    Woke,
    /// Someone revived the session meanwhile; it runs in this account.
    Aborted(Profile),
    /// Nothing runs the session now (it is stopped where it was).
    Failed(String),
}

/// What a stop-and-resume does.
pub struct Plan<'p> {
    /// Accounts to try in order; empty: wake the session where it is.
    pub targets: &'p [Profile],
    /// `moves[].reason`.
    pub reason: &'p str,
    /// The first prompt in an account.
    pub prompt: &'p dyn Fn(&Profile) -> String,
    pub deadline: Instant,
    /// The record's state and `wait_until` when every attempt failed;
    /// `None` leaves it `moving` for the caller.
    pub on_fail: Option<(State, Option<i64>)>,
}

/// Stops `s` and resumes it per `plan`. The record is `moving` from before
/// the stop until the resumed session shows a pid (or the attempts failed).
pub fn relocate(env: &Env, s: &Session, plan: &Plan) -> Outcome {
    let paths = &env.app.paths;
    let short = s.short.as_str();
    let set = |f: &dyn Fn(&mut record::Record)| {
        if let Err(e) = record::update(paths, &s.sid, |r| f(r)) {
            log::warn!("bg {short}: record: {e:#}");
        }
    };
    set(&|r| {
        r.state = State::Moving;
        r.note = Some(format!("moving ({})", plan.reason));
    });
    let out = relocate_inner(env, s, plan);
    let journal = env.journal(s.run.as_deref(), s.created_at);
    let now = unix_now();
    match &out {
        Outcome::Moved(to) => {
            log::info!("bg {short}: moved from {} to {}", s.from.id, to.id);
            let mv = Move {
                at: now,
                from: s.from.id.clone(),
                from_label: s.from.label.clone(),
                from_config_dir: record::config_dir(&s.from),
                to: to.id.clone(),
                to_label: to.label.clone(),
                to_config_dir: record::config_dir(to),
                reason: plan.reason.to_string(),
            };
            set(&|r| {
                r.moves.push(mv.clone());
                r.set_profile(to);
                r.state = State::Watching;
                r.limit = None;
                r.wait_until = None;
                r.note = Some(format!(
                    "moved from {} to {} ({})",
                    s.from.label, to.label, plan.reason
                ));
            });
            if let Some(j) = journal {
                let mut ev = j.event(EventKind::Switch, &to.id, now);
                ev.from = Some(s.from.id.clone());
                ev.reason = Some(plan.reason.to_string());
                ev.session = Some(s.sid.clone());
                j.write(ev);
            }
        }
        Outcome::Woke => {
            log::info!("bg {short}: woke on {}", s.from.id);
            set(&|r| {
                r.state = State::Watching;
                r.limit = None;
                r.wait_until = None;
                r.note = Some(format!("woke on {}", s.from.label));
            });
            if let Some(j) = journal {
                let mut ev = j.event(EventKind::Resume, &s.from.id, now);
                ev.session = Some(s.sid.clone());
                j.write(ev);
            }
        }
        Outcome::Aborted(p) => {
            log::warn!(
                "bg {short}: move aborted; the session runs again in {}",
                p.id
            );
            // It runs where an earlier attempt resumed it: that is a move.
            let mv = (p.id != s.from.id).then(|| Move {
                at: now,
                from: s.from.id.clone(),
                from_label: s.from.label.clone(),
                from_config_dir: record::config_dir(&s.from),
                to: p.id.clone(),
                to_label: p.label.clone(),
                to_config_dir: record::config_dir(p),
                reason: plan.reason.to_string(),
            });
            set(&|r| {
                if r.profile != p.id
                    && let Some(m) = &mv
                {
                    r.moves.push(m.clone());
                }
                r.set_profile(p);
                r.state = State::Watching;
                r.limit = None;
                r.wait_until = None;
                r.note = Some(format!(
                    "move aborted: the session runs again in {}",
                    p.label
                ));
            });
        }
        Outcome::Failed(e) => {
            log::warn!("bg {short}: {e}");
            if let Some((state, until)) = plan.on_fail {
                set(&|r| {
                    r.state = state;
                    r.wait_until = until;
                    r.note = Some(e.clone());
                });
            }
        }
    }
    out
}

fn relocate_inner(env: &Env, s: &Session, plan: &Plan) -> Outcome {
    let (sid, short) = (s.sid.as_str(), s.short.as_str());
    let claude = &env.claude;
    match claude.stop(&s.from, short) {
        Ok(o) if o.code != 0 => log::info!(
            "bg {short}: claude stop in {} exited {}: {}",
            s.from.id,
            o.code,
            o.stderr.trim()
        ),
        Ok(_) => {}
        Err(e) => log::warn!("bg {short}: {e:#}"),
    }
    let stop_by = (Instant::now() + STOP_WAIT).min(plan.deadline);
    if !claude.wait_stopped(&s.from, sid, short, stop_by) {
        // Resuming now would start a copy next to the old process.
        return Outcome::Failed(format!(
            "the session did not stop in {} in time",
            s.from.label
        ));
    }
    let wake = plan.targets.is_empty();
    let targets: Vec<Profile> = if wake {
        vec![s.from.clone()]
    } else {
        plan.targets.to_vec()
    };
    // Every account the session may run in now.
    let mut seen: Vec<Profile> = vec![s.from.clone()];
    let mut errors = Vec::new();
    for target in &targets {
        if Instant::now() >= plan.deadline {
            errors.push("out of time".to_string());
            break;
        }
        if !seen.iter().any(|p| p.id == target.id) {
            seen.push(target.clone());
        }
        // Someone may have revived it (`claude respawn` from another tool).
        for p in &seen {
            match claude.live_pid(p, sid, short) {
                Ok(Some(pid)) => {
                    log::warn!("bg {short}: runs again in {} (pid {pid})", p.id);
                    return Outcome::Aborted(p.clone());
                }
                Ok(None) => {}
                Err(e) => log::warn!("bg {short}: {e:#}"),
            }
        }
        match resume_in(env, s, target, (plan.prompt)(target)) {
            Ok(()) if wake => return Outcome::Woke,
            Ok(()) => return Outcome::Moved(target.clone()),
            Err(e) => {
                log::warn!("bg {short}: resume in {}: {e:#}", target.id);
                errors.push(format!("{}: {e:#}", target.label));
            }
        }
    }
    let what = if wake { "waking" } else { "moving" };
    Outcome::Failed(format!("{what} the session failed ({})", errors.join("; ")))
}

/// Resumes `s` in `target` and checks that the same session runs there.
fn resume_in(env: &Env, s: &Session, target: &Profile, prompt: String) -> Result<()> {
    let short = s.short.as_str();
    // A profile that has the job takes the no-flag form; flags would start
    // a copy under a new id.
    let has_job = job::job_dir(&target.dir, short).is_dir();
    let flags = prepare_resume(env, s, target, has_job);
    let args = job::resume_args(&s.sid, flags.as_deref(), &prompt);
    log::info!(
        "bg {short}: resume in {} ({} form)",
        target.id,
        if has_job { "wake" } else { "flags" }
    );
    let (out, launched) = env
        .claude
        .launch(target, &s.cwd, &args)
        .context("starting claude")?;
    let launched = launched.with_context(|| {
        format!(
            "claude exited {} without a session id: {}",
            out.code,
            out.stderr.trim()
        )
    })?;
    if launched.short != short {
        // A copy under a new id: stop it before it touches the worktree.
        let _ = env.claude.stop(target, &launched.short);
        anyhow::bail!(
            "claude started a copy ({}) instead of resuming {short}; stopped it",
            launched.short
        );
    }
    let deadline = Instant::now() + VERIFY_WAIT;
    loop {
        match env.claude.live_pid(target, &s.sid, short) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(e) => log::warn!("bg {short}: {e:#}"),
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the session shows no process in {}",
            target.label
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes() {
        let mut a = Profile::bare(
            "a",
            Tool::Claude,
            "/h/.claude-a".into(),
            "/h/.claude".into(),
        );
        a.label = "PB2".into();
        let mut b = a.clone();
        b.id = "b".into();
        b.label = "PB4".into();
        let mut c = Config::default();
        let p = prompt(&c, &move_note(&a, &b, "5-hour limit"));
        assert_eq!(
            p,
            "[agent-prism] This session moved from account PB2 to PB4 after the 5-hour limit; \
             the old process's background tasks and subagents stopped, so start again the ones \
             you still need.\n\ncontinue"
        );
        assert!(move_note(&a, &b, MANUAL).ends_with("PB4 on request"));
        c.balance.resume_prompt = String::new();
        assert!(prompt(&c, "x").ends_with("\n\ncontinue"));
        assert_eq!(window_words(Some(LimitKind::SevenDay)), "weekly limit");
        assert_eq!(window_words(None), "limit");
    }

    #[test]
    fn projects_sharing() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let common = root.join("common");
        std::fs::create_dir_all(common.join("projects")).unwrap();
        let mk = |id: &str, link: bool| {
            let d = root.join(id);
            std::fs::create_dir_all(&d).unwrap();
            if link {
                std::os::unix::fs::symlink(common.join("projects"), d.join("projects")).unwrap();
            } else {
                std::fs::create_dir_all(d.join("projects")).unwrap();
            }
            Profile::bare(id, Tool::Claude, d, common.clone())
        };
        let (a, b, c) = (mk("a", true), mk("b", true), mk("c", false));
        let default = Profile::bare("d", Tool::Claude, common.clone(), common.clone());
        assert!(shares_projects(&a, &b));
        assert!(shares_projects(&a, &default));
        assert!(!shares_projects(&a, &c));
    }
}
