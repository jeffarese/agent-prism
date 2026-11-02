//! The watcher of one background session (DESIGN §11d "The watcher"): a
//! detached `agp bg _watch <session id>` that polls the job's `state.json`
//! and handles each new rate limit once: move the session to another
//! account, or wait for the reset and wake it on the same one.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use prism_core::journal::EventKind;
use prism_core::model::LimitKind;
use prism_core::profile::Profile;

use super::job::{self, JobState};
use super::mover::{self, Env, Outcome, Plan, Session};
use super::record::{self, Limit, Lock, OnLimit, Record, State};
use crate::app::AppCtx;
use crate::launch::unix_now;

/// A wait goes on this long after the reset time.
const WAIT_GRACE_SECS: i64 = 30;
/// With no known reset, check again after this long; also how often a
/// `switch` wait looks for a free account.
const RECHECK_SECS: i64 = 300;
/// No change of the state file for this long: dormant.
const DORMANT_SECS: i64 = 7 * 86_400;
/// After this long without a change, poll slower.
const SLOW_AFTER_SECS: i64 = 600;
/// The time budget of one stop-and-resume.
const ACT_BUDGET: Duration = Duration::from_secs(120);

/// The poll interval: 10 s (test hook: `AGP_BG_POLL_MS`).
fn base_interval() -> Duration {
    prism_core::env::var("AGP_BG_POLL_MS")
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(10))
}

/// What the watcher does this round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// A limit not handled yet.
    NewLimit,
    /// The wait is over: check the reset, then wake.
    WaitDue,
    /// While a `switch` waits: look for a free account.
    CheckOthers,
    /// The session went on by itself: drop the limit.
    Cancel,
}

/// The step for `rec` given the job's state now.
pub fn decide(rec: &Record, job: &JobState, on_limit: OnLimit, now: i64, check_at: i64) -> Step {
    if job.rate_limited() {
        if rec.limit_key.as_deref() != Some(job.limit_key().as_str()) {
            return Step::NewLimit;
        }
        if rec.state == State::Waiting {
            if rec.wait_until.is_some_and(|t| now >= t) {
                return Step::WaitDue;
            }
            if on_limit == OnLimit::Switch && now >= check_at {
                return Step::CheckOthers;
            }
        }
        return Step::Nothing;
    }
    if matches!(rec.state, State::Limited | State::Waiting) {
        return Step::Cancel;
    }
    Step::Nothing
}

/// When a wait for a reset at `resets_at` (limit since `since`) is over:
/// `Err` when it is past `max_wait`.
pub fn wait_end(
    resets_at: Option<i64>,
    since: i64,
    now: i64,
    max_wait: Option<i64>,
) -> std::result::Result<i64, String> {
    let until = resets_at.map_or(now + RECHECK_SECS, |t| t + WAIT_GRACE_SECS);
    if let Some(max) = max_wait {
        let need = resets_at.map_or(now - since, |t| t + WAIT_GRACE_SECS - since);
        if need > max {
            return Err(match resets_at {
                Some(t) => format!(
                    "the reset is in {}, past --max-wait",
                    prism_usage::balance::fmt_duration(t - now)
                ),
                None => "waited past --max-wait".to_string(),
            });
        }
    }
    Ok(until)
}

/// `agp bg _watch <session id>`: runs until the job is gone or dormant.
pub fn run(ctx: &AppCtx, sid: &str) -> Result<i32> {
    // A probe from `agp bg` holds the lock shared for an instant: retry.
    let Some(_lock) = Lock::wait(
        &record::watcher_lock(&ctx.paths, sid),
        Duration::from_secs(2),
    )?
    else {
        log::info!("bg {sid}: a watcher runs already");
        return Ok(0);
    };
    let Some(rec) = record::load(&ctx.paths, sid) else {
        log::warn!("bg {sid}: no record; nothing to watch");
        return Ok(0);
    };
    // Its moves pick for a run of the weight learned at its launch.
    if let Some(w) = rec.load {
        crate::launch::set_run_weight(w, "learned at its launch".into());
    }
    log::info!("bg {}: watching on {}", rec.short_id, rec.profile);
    record::update(&ctx.paths, sid, |r| {
        r.watcher_pid = Some(std::process::id());
        if matches!(r.state, State::Ended | State::Dormant) {
            r.state = State::Watching;
        }
    })?;
    let mut w = Watcher {
        ctx,
        sid: sid.to_string(),
        started: unix_now(),
        seen: None,
        job: None,
        changed_at: unix_now(),
        check_at: 0,
        revive_checked: None,
    };
    w.run()
}

struct Watcher<'a> {
    ctx: &'a AppCtx,
    sid: String,
    started: i64,
    /// The state file last read: profile, path and mtime.
    seen: Option<(String, PathBuf, SystemTime)>,
    job: Option<JobState>,
    /// When the state file last changed, or the watch started if later.
    changed_at: i64,
    /// While a `switch` waits: when to look for a free account next.
    check_at: i64,
    /// The old job (profile, mtime) last checked for a revived session, so
    /// `claude agents` runs once per change, not every round.
    revive_checked: Option<(String, SystemTime)>,
}

impl Watcher<'_> {
    fn run(&mut self) -> Result<i32> {
        loop {
            match self.round() {
                Ok(Some(code)) => return Ok(code),
                Ok(None) => {}
                Err(e) => log::warn!("bg {}: {e:#}", self.sid),
            }
            std::thread::sleep(self.interval());
        }
    }

    fn interval(&self) -> Duration {
        let base = base_interval();
        if unix_now() - self.changed_at > SLOW_AFTER_SECS {
            base * 6
        } else {
            base
        }
    }

    fn update(&self, f: impl FnOnce(&mut Record)) -> Result<Record> {
        record::update(&self.ctx.paths, &self.sid, f)
    }

    /// One poll. `Some(code)`: the watch is over.
    fn round(&mut self) -> Result<Option<i32>> {
        let paths = &self.ctx.paths;
        let Some(rec) = record::load(paths, &self.sid) else {
            log::info!("bg {}: record removed; stopping", self.sid);
            return Ok(Some(0));
        };
        // `agp bg move` (or nobody, after a crash) holds it.
        if record::held(&record::action_lock(paths, &self.sid)) {
            return Ok(None);
        }
        let env = Env::load(self.ctx, &rec.options)?;
        let on_limit = OnLimit::effective(rec.options.run().on_limit, env.config.balance.on_limit);
        let mut profile = env.record_profile(&rec);
        let short = rec.short_id.clone();
        if let Some(p) = self.revived_elsewhere(&env, &rec, &profile) {
            log::info!("bg {short}: the session runs in {} now", p.id);
            self.update(|r| {
                if r.state != State::Moving {
                    r.set_profile(&p);
                    r.note = Some(format!("the session runs in {} now", p.label));
                }
            })?;
            profile = p;
        }
        if !job::job_dir(&profile.dir, &short).is_dir() {
            self.end(&rec, &env, &profile)?;
            return Ok(Some(0));
        }
        let now = unix_now();
        self.read_job(&profile, &short);
        let Some(job) = self.job.clone() else {
            return Ok(None);
        };
        if now - self.changed_at > DORMANT_SECS {
            log::info!("bg {short}: no change for 7 days; dormant");
            self.update(|r| {
                r.state = State::Dormant;
                r.watcher_pid = None;
                r.note = Some("no change for 7 days; `agp bg watch` re-arms it".into());
            })?;
            return Ok(Some(0));
        }
        if rec.state == State::Moving {
            // Left by a move that died (the action lock is free): look at
            // the limit again.
            self.update(|r| {
                r.state = State::Watching;
                r.limit_key = None;
            })?;
            return Ok(None);
        }
        if rec.on_limit != on_limit {
            self.update(|r| r.on_limit = on_limit)?;
        }
        match decide(&rec, &job, on_limit, now, self.check_at) {
            Step::Nothing => {}
            Step::Cancel => {
                log::info!("bg {short}: the session went on by itself");
                self.update(|r| {
                    // A move that started since this round looked keeps it.
                    if r.state != State::Moving {
                        r.state = State::Watching;
                        r.limit = None;
                        r.wait_until = None;
                        r.note = Some("the session went on by itself".into());
                    }
                })?;
            }
            step => self.act(step, &env, &profile, &job, on_limit)?,
        }
        Ok(None)
    }

    /// An account the session ran in before where it runs again: its old
    /// job changed after the current one's and `claude agents` there shows
    /// a live pid (someone ran `claude respawn` in the old account). A move
    /// writes the old job (the stop) before the new one, so a move itself
    /// never looks like this; `claude agents` runs once per such change.
    fn revived_elsewhere(&mut self, env: &Env, rec: &Record, current: &Profile) -> Option<Profile> {
        let short = &rec.short_id;
        let now_at = job::state_mtime(&current.dir, short)?;
        let (t, p) = rec
            .previous
            .iter()
            .filter_map(|p| env.profile(&p.profile))
            .filter_map(|p| Some((job::state_mtime(&p.dir, short)?, p)))
            .filter(|(t, _)| {
                t.duration_since(now_at)
                    .is_ok_and(|d| d > Duration::from_secs(2))
            })
            .max_by_key(|(t, _)| *t)?;
        let key = (p.id.clone(), t);
        if self.revive_checked.as_ref() == Some(&key) {
            return None;
        }
        self.revive_checked = Some(key);
        match env.claude.live_pid(&p, &rec.session_id, short) {
            Ok(Some(_)) => Some(p),
            Ok(None) => None,
            Err(e) => {
                log::warn!("bg {short}: checking {}: {e:#}", p.id);
                None
            }
        }
    }

    /// Reads the state file when it changed (or the profile did).
    fn read_job(&mut self, profile: &Profile, short: &str) {
        let file = job::state_file(&profile.dir, short);
        let Some(mtime) = std::fs::metadata(&file).and_then(|m| m.modified()).ok() else {
            self.job = None;
            return;
        };
        let secs = mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        // The last change, but not before the watch started (a re-armed
        // watch counts from then).
        self.changed_at = self.started.max(secs);
        let same = self
            .seen
            .as_ref()
            .is_some_and(|(p, f, t)| *p == profile.id && *f == file && *t == mtime);
        if same && self.job.is_some() {
            return;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        self.job = JobState::parse(&text);
        if self.job.is_some() {
            self.seen = Some((profile.id.clone(), file, mtime));
        }
    }

    fn end(&self, rec: &Record, env: &Env, profile: &Profile) -> Result<()> {
        log::info!("bg {}: the job is gone in {}", rec.short_id, profile.id);
        self.update(|r| {
            r.state = State::Ended;
            r.watcher_pid = None;
            r.note = Some(format!("the job is gone in {}", profile.label));
        })?;
        if let Some(j) = env.journal(rec.run.as_deref(), rec.created_at) {
            let now = unix_now();
            let mut ev = j.event(EventKind::End, &profile.id, now);
            ev.session = Some(rec.session_id.clone());
            ev.secs = Some((now - rec.created_at).max(0));
            j.write(ev);
        }
        Ok(())
    }

    /// A step that may stop and resume the session: under the action lock,
    /// and only if the record still points where this round looked.
    fn act(
        &mut self,
        step: Step,
        env: &Env,
        profile: &Profile,
        job: &JobState,
        on_limit: OnLimit,
    ) -> Result<()> {
        let paths = &self.ctx.paths;
        let Some(_act) = Lock::try_take(&record::action_lock(paths, &self.sid))? else {
            return Ok(());
        };
        let Some(rec) = record::load(paths, &self.sid) else {
            return Ok(());
        };
        if rec.profile != profile.id || rec.state == State::Moving {
            return Ok(());
        }
        let session = Session {
            sid: rec.session_id.clone(),
            short: rec.short_id.clone(),
            from: profile.clone(),
            job: job.clone(),
            cwd: job
                .cwd
                .clone()
                .or(rec.cwd.clone())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/")),
            launch: rec.launch.clone(),
            run: rec.run.clone(),
            created_at: rec.created_at,
        };
        match step {
            Step::NewLimit => self.new_limit(env, &rec, &session, on_limit),
            Step::WaitDue => self.wait_due(env, &rec, &session),
            Step::CheckOthers => {
                self.check_at = unix_now() + RECHECK_SECS;
                let limit = rec.limit.clone().unwrap_or_default();
                self.try_switch(env, &session, &limit.window, rec.wait_until);
                Ok(())
            }
            Step::Nothing | Step::Cancel => Ok(()),
        }
    }

    fn new_limit(&mut self, env: &Env, rec: &Record, s: &Session, on_limit: OnLimit) -> Result<()> {
        let now = unix_now();
        let since = s
            .job
            .updated_at
            .as_deref()
            .and_then(job::iso_to_unix)
            .unwrap_or(now);
        let text = s.job.limit_text();
        let (text_kind, text_reset) = job::parse_limit(&text, since);
        let usage = env.limited(&s.from, env.model(&s.job.respawn_flags));
        let kind: Option<LimitKind> = usage.as_ref().map(|l| l.kind).or(text_kind);
        let resets_at = usage.as_ref().and_then(|l| l.until).or(text_reset);
        let limit = Limit {
            text: text.clone(),
            since,
            resets_at,
            window: mover::window_words(kind).to_string(),
        };
        log::info!(
            "bg {}: {} hit the {} ({text}); on limit: {on_limit:?}",
            s.short,
            s.from.id,
            limit.window
        );
        let key = s.job.limit_key();
        self.update(|r| {
            r.limit = Some(limit.clone());
            r.limit_key = Some(key);
            r.state = State::Limited;
            r.wait_until = None;
            r.note = Some(format!("{} hit the {}", s.from.label, limit.window));
        })?;
        if let Some(j) = env.journal(rec.run.as_deref(), rec.created_at) {
            let mut ev = j.event(EventKind::Limit, &s.from.id, now);
            ev.limit = kind.map(mover::limit_code);
            ev.session = Some(s.sid.clone());
            j.write(ev);
        }
        match on_limit {
            OnLimit::Stay => Ok(()),
            OnLimit::Switch => {
                if self.try_switch(env, s, &limit.window, None) {
                    return Ok(());
                }
                self.check_at = unix_now() + RECHECK_SECS;
                self.start_wait(env, rec, s, &limit)
            }
            OnLimit::Wait => self.start_wait(env, rec, s, &limit),
        }
    }

    /// Moves `s` to the best other account. True when the limit is dealt
    /// with (moved, or someone revived the session meanwhile).
    fn try_switch(&self, env: &Env, s: &Session, window: &str, wait_until: Option<i64>) -> bool {
        let targets = mover::candidates(env, s);
        if targets.is_empty() {
            log::info!("bg {}: no other account to move to", s.short);
            return false;
        }
        let config = &env.config;
        let from = s.from.clone();
        let reason = if window.is_empty() { "limit" } else { window };
        let prompt = |to: &Profile| mover::prompt(config, &mover::move_note(&from, to, reason));
        let plan = Plan {
            targets: &targets,
            reason,
            prompt: &prompt,
            deadline: Instant::now() + ACT_BUDGET,
            // Stopped where it was: wait there (the caller sets the wait).
            on_fail: Some((State::Waiting, wait_until)),
        };
        !matches!(mover::relocate(env, s, &plan), Outcome::Failed(_))
    }

    fn start_wait(&self, env: &Env, rec: &Record, s: &Session, limit: &Limit) -> Result<()> {
        let now = unix_now();
        match wait_end(limit.resets_at, limit.since, now, rec.options.max_wait) {
            Err(why) => {
                log::info!("bg {}: not waiting: {why}", s.short);
                self.update(|r| {
                    r.state = State::Limited;
                    r.wait_until = None;
                    r.note = Some(why);
                })?;
            }
            Ok(until) => {
                log::info!("bg {}: waiting on {} until {until}", s.short, s.from.id);
                self.update(|r| {
                    r.state = State::Waiting;
                    r.wait_until = Some(until);
                    r.note = Some(format!("waiting for the reset of {}", s.from.label));
                })?;
                if let Some(j) = env.journal(rec.run.as_deref(), rec.created_at) {
                    let mut ev = j.event(EventKind::Wait, &s.from.id, now);
                    ev.until = limit.resets_at;
                    ev.session = Some(s.sid.clone());
                    j.write(ev);
                }
            }
        }
        Ok(())
    }

    /// The wait is over: wait again if the account still resets later,
    /// else wake the session there.
    fn wait_due(&self, env: &Env, rec: &Record, s: &Session) -> Result<()> {
        let now = unix_now();
        let mut limit = rec.limit.clone().unwrap_or_default();
        if let Some(l) = env.limited(&s.from, env.model(&s.job.respawn_flags))
            && l.until.is_some_and(|t| t > now)
        {
            limit.resets_at = l.until;
            self.update(|r| r.limit = Some(limit.clone()))?;
            return self.start_wait(env, rec, s, &limit);
        }
        let config = &env.config;
        let label = s.from.label.clone();
        let window = if limit.window.is_empty() {
            "limit".to_string()
        } else {
            limit.window.clone()
        };
        let prompt = |_: &Profile| {
            mover::prompt(
                config,
                &format!(
                    "agent-prism waited for the reset on the same account ({label}) after the {window}"
                ),
            )
        };
        let plan = Plan {
            targets: &[],
            reason: &window,
            prompt: &prompt,
            deadline: Instant::now() + ACT_BUDGET,
            on_fail: Some((State::Waiting, Some(now + RECHECK_SECS))),
        };
        mover::relocate(env, s, &plan);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(state: &str, updated: &str) -> JobState {
        JobState {
            state: state.into(),
            needs: Some("rate limited — wait and retry · You've hit your 5-hour limit".into()),
            updated_at: Some(updated.into()),
            ..Default::default()
        }
    }

    #[test]
    fn steps() {
        let mut rec = Record::default();
        let limited = job("blocked", "t1");
        // Each limit once, keyed by updatedAt.
        assert_eq!(
            decide(&rec, &limited, OnLimit::Switch, 100, 0),
            Step::NewLimit
        );
        rec.limit_key = Some("t1".into());
        rec.state = State::Limited;
        assert_eq!(decide(&rec, &limited, OnLimit::Stay, 100, 0), Step::Nothing);
        assert_eq!(
            decide(&rec, &job("blocked", "t2"), OnLimit::Stay, 100, 0),
            Step::NewLimit
        );
        // A wait: due at wait_until; a switch looks for free accounts.
        rec.state = State::Waiting;
        rec.wait_until = Some(200);
        assert_eq!(decide(&rec, &limited, OnLimit::Wait, 100, 0), Step::Nothing);
        assert_eq!(
            decide(&rec, &limited, OnLimit::Switch, 100, 50),
            Step::CheckOthers
        );
        assert_eq!(
            decide(&rec, &limited, OnLimit::Switch, 100, 150),
            Step::Nothing
        );
        assert_eq!(decide(&rec, &limited, OnLimit::Wait, 200, 0), Step::WaitDue);
        // It went on by itself: the pending wait is dropped.
        assert_eq!(
            decide(&rec, &job("working", "t3"), OnLimit::Wait, 100, 0),
            Step::Cancel
        );
        rec.state = State::Watching;
        assert_eq!(
            decide(&rec, &job("working", "t3"), OnLimit::Wait, 100, 0),
            Step::Nothing
        );
    }

    #[test]
    fn wait_ends() {
        assert_eq!(wait_end(Some(1000), 100, 500, None), Ok(1030));
        assert_eq!(wait_end(None, 100, 500, None), Ok(800));
        assert!(wait_end(Some(10_000), 100, 500, Some(3600)).is_err());
        assert_eq!(wait_end(Some(1000), 100, 500, Some(3600)), Ok(1030));
        // Unknown reset: give up once waited past --max-wait.
        assert!(wait_end(None, 100, 5000, Some(3600)).is_err());
    }
}
