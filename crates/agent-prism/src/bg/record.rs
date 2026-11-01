//! The record of a watched background session, `<state>/bg/<session id>.json`,
//! and the locks next to it (DESIGN §11d). `agp bg --json` prints these
//! records; another program (chief) reads that JSON, so the keys are a
//! contract.
//!
//! Locks (`flock`, released when the holder exits):
//! - `<id>.lock`: held by the watcher for its whole life (one per session).
//! - `<id>.act.lock`: held while something stops and resumes the session
//!   (the watcher acting, `agp bg move`), so the two never race.
//! - `<id>.rec.lock`: held for each read-modify-write of the record.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use prism_core::model::Policy;
use prism_core::paths::Paths;
use prism_core::profile::Profile;
use serde::{Deserialize, Serialize};

use crate::cli::{LimitAction, RunOptions};
use crate::launch::unix_now;

/// What the watcher does on a limit (`--on-limit`, else the config).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnLimit {
    /// Move to the best other account; none free: wait.
    #[default]
    Switch,
    /// Wait for the reset, then wake the session on the same account.
    Wait,
    /// Only record the limit.
    Stay,
}

impl OnLimit {
    /// The effective value: `--on-limit` if given, else the config's
    /// `on_limit`. Nobody can answer `ask`, so it switches; `fail` has no
    /// process to fail, so it stays.
    pub fn effective(flag: Option<LimitAction>, config: prism_core::model::OnLimit) -> OnLimit {
        use prism_core::model::OnLimit as C;
        let mode = match flag {
            Some(LimitAction::Fail) => return OnLimit::Stay,
            Some(LimitAction::Mode(m)) => m,
            None => config,
        };
        match mode {
            C::Auto | C::Ask => OnLimit::Switch,
            C::Wait => OnLimit::Wait,
            C::Stay => OnLimit::Stay,
        }
    }
}

/// The record's state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Not limited (or the limit is handled).
    #[default]
    Watching,
    /// Limited, and the watcher does nothing about it (`stay`, a reset past
    /// `--max-wait`).
    Limited,
    /// Waiting for the reset (`wait_until`).
    Waiting,
    /// Stopping and resuming the session: don't revive it meanwhile.
    Moving,
    /// The job is gone.
    Ended,
    /// No change for 7 days; `agp bg watch` re-arms it.
    Dormant,
}

/// The limit the session is stopped on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Limit {
    /// Claude Code's words, e.g. "You've hit your 5-hour limit · resets …".
    pub text: String,
    /// When it hit (the job's `updatedAt`), unix seconds.
    pub since: i64,
    /// When the window resets, if known.
    pub resets_at: Option<i64>,
    /// "5-hour limit", "weekly limit", "model limit" or "limit".
    #[serde(default)]
    pub window: String,
}

/// One move to another account.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Move {
    pub at: i64,
    pub from: String,
    pub from_label: String,
    pub from_config_dir: Option<String>,
    pub to: String,
    pub to_label: String,
    pub to_config_dir: Option<String>,
    /// "5-hour limit", "weekly limit", "model limit", "limit" (window
    /// unknown) or "manual move".
    pub reason: String,
}

/// A profile the session ran in.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Place {
    pub profile: String,
    pub label: String,
    pub config_dir: Option<String>,
}

/// The per-run options of the watch (`agp --bg` or `agp bg watch`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Options {
    /// `--on-limit` as given (`switch`, `ask`, `stay`, `wait`, `fail`).
    pub on_limit: Option<String>,
    pub max_wait: Option<i64>,
    pub accounts: Option<Vec<String>>,
    #[serde(default)]
    pub exclude: Vec<String>,
    pub max_usage: Option<f64>,
    pub policy: Option<Policy>,
}

impl Options {
    pub fn new(run: &RunOptions, policy: Option<Policy>) -> Options {
        Options {
            on_limit: run.on_limit.map(|a| {
                match a {
                    LimitAction::Fail => "fail",
                    LimitAction::Mode(prism_core::model::OnLimit::Auto) => "switch",
                    LimitAction::Mode(prism_core::model::OnLimit::Ask) => "ask",
                    LimitAction::Mode(prism_core::model::OnLimit::Stay) => "stay",
                    LimitAction::Mode(prism_core::model::OnLimit::Wait) => "wait",
                }
                .to_string()
            }),
            max_wait: run.max_wait,
            accounts: run.accounts.clone(),
            exclude: run.exclude.clone(),
            max_usage: run.max_usage,
            policy,
        }
    }

    pub fn run(&self) -> RunOptions {
        RunOptions {
            on_limit: self
                .on_limit
                .as_deref()
                .and_then(|v| crate::cli::parse_limit_action(v).ok()),
            max_wait: self.max_wait,
            accounts: self.accounts.clone(),
            exclude: self.exclude.clone(),
            max_usage: self.max_usage,
        }
    }
}

/// How agent-prism launched the session (`agp --bg`), so a move can build
/// the target's `--settings` and args again.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Launch {
    /// The user's `--settings` value before agent-prism merged it (a path
    /// made absolute).
    pub user_settings: Option<String>,
    /// Names the session's settings files (`settings::file`).
    pub key: Option<String>,
    /// Per profile id: the args agent-prism put before the user's
    /// (`[claude] args`, then the profile's) when it started the job there,
    /// so a move takes out exactly those.
    pub added: BTreeMap<String, Vec<String>>,
}

/// `<state>/bg/<session id>.json`. The keys up to `updated_at` are the
/// contract of `agp bg --json`; the rest are extras.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub session_id: String,
    pub short_id: String,
    pub name: Option<String>,
    /// The profile the session runs in now.
    pub profile: String,
    pub label: String,
    /// `CLAUDE_CONFIG_DIR` of that profile; null for the default install.
    pub config_dir: Option<String>,
    pub cwd: Option<String>,
    pub on_limit: OnLimit,
    pub state: State,
    pub limit: Option<Limit>,
    pub moves: Vec<Move>,
    /// Every profile the session ran in before the current one, oldest
    /// first, without repeats and without the current profile.
    pub previous: Vec<Place>,
    pub watcher_pid: Option<u32>,
    /// A watcher holds the lock (set when listed).
    pub watching: bool,
    pub updated_at: i64,
    pub created_at: i64,
    /// While `waiting`: when the watcher checks the reset next.
    pub wait_until: Option<i64>,
    /// The last thing that happened, for people.
    pub note: Option<String>,
    pub options: Options,
    /// The run id in the run journal.
    pub run: Option<String>,
    pub launch: Option<Launch>,
    /// The `updatedAt` of the last limit handled (each is handled once).
    pub limit_key: Option<String>,
}

impl Record {
    /// Points the record at `p`.
    pub fn set_profile(&mut self, p: &Profile) {
        self.profile = p.id.clone();
        self.label = p.label.clone();
        self.config_dir = config_dir(p);
    }

    /// Recomputes `previous` from `moves`.
    pub fn fill_previous(&mut self) {
        let mut out: Vec<Place> = Vec::new();
        for m in &self.moves {
            if m.from != self.profile && !out.iter().any(|p| p.profile == m.from) {
                out.push(Place {
                    profile: m.from.clone(),
                    label: m.from_label.clone(),
                    config_dir: m.from_config_dir.clone(),
                });
            }
        }
        self.previous = out;
    }
}

/// The `config_dir` of a profile: null for the default install.
pub fn config_dir(p: &Profile) -> Option<String> {
    p.config_dir_env().map(|d| d.to_string_lossy().into_owned())
}

/// `<state>/bg`.
pub fn dir(paths: &Paths) -> PathBuf {
    paths.state_dir.join("bg")
}

fn file(paths: &Paths, sid: &str, ext: &str) -> PathBuf {
    dir(paths).join(format!("{sid}.{ext}"))
}

pub fn path(paths: &Paths, sid: &str) -> PathBuf {
    file(paths, sid, "json")
}

pub fn load(paths: &Paths, sid: &str) -> Option<Record> {
    let text = std::fs::read_to_string(path(paths, sid)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Read-modify-write of the record under its lock; a missing record starts
/// from the default with `session_id` set. The write is atomic, so readers
/// without the lock see the old or the new record, never half of one.
pub fn update(paths: &Paths, sid: &str, f: impl FnOnce(&mut Record)) -> Result<Record> {
    let _lock = Lock::wait(&file(paths, sid, "rec.lock"), Duration::from_secs(10))?
        .context("the record lock is held")?;
    let mut rec = load(paths, sid).unwrap_or_else(|| Record {
        session_id: sid.to_string(),
        created_at: unix_now(),
        ..Default::default()
    });
    f(&mut rec);
    rec.fill_previous();
    rec.updated_at = unix_now();
    let data = serde_json::to_vec_pretty(&rec)?;
    write_atomic(&path(paths, sid), &data)?;
    Ok(rec)
}

pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("record path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// Records of ended sessions are removed after this long.
const KEEP_ENDED_SECS: i64 = 7 * 86_400;

/// Every record, newest change first, with `watching` filled in. Records
/// of sessions that ended a week ago are removed.
pub fn list(paths: &Paths) -> Vec<Record> {
    let Ok(rd) = std::fs::read_dir(dir(paths)) else {
        return Vec::new();
    };
    let mut out: Vec<Record> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let sid = name.strip_suffix(".json")?;
            if sid.starts_with('.') {
                return None;
            }
            let mut r = load(paths, sid)?;
            if r.state == State::Ended && unix_now() - r.updated_at > KEEP_ENDED_SECS {
                for ext in ["json", "lock", "act.lock", "rec.lock"] {
                    let _ = std::fs::remove_file(file(paths, sid, ext));
                }
                if let Some(key) = r.launch.as_ref().and_then(|l| l.key.as_deref()) {
                    super::settings::remove(paths, key);
                }
                return None;
            }
            r.watching = watcher_alive(paths, &r.session_id);
            if !r.watching
                && !matches!(r.state, State::Ended | State::Moving)
                && job_gone(paths, &r)
            {
                // Nobody watches (`stay`, or a watcher that died) and the job
                // is gone: it ended.
                if let Ok(u) = update(paths, sid, |x| {
                    x.state = State::Ended;
                    x.watcher_pid = None;
                    x.note = Some("the job is gone".into());
                }) {
                    r = u;
                }
            }
            Some(r)
        })
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    out
}

/// The job folder of the record's profile is gone (`config_dir` null: the
/// default install, `~/.claude`).
fn job_gone(paths: &Paths, r: &Record) -> bool {
    if r.short_id.is_empty() {
        return false;
    }
    let dir = r
        .config_dir
        .as_ref()
        .map_or_else(|| paths.home.join(".claude"), PathBuf::from);
    !dir.join("jobs").join(&r.short_id).is_dir()
}

/// The record of a session id or a short id.
pub fn find(paths: &Paths, id: &str) -> Option<Record> {
    if let Some(r) = load(paths, id) {
        return Some(r);
    }
    let short = super::job::short_of(id);
    list(paths)
        .into_iter()
        .filter(|r| r.short_id == short || r.session_id == id)
        .max_by_key(|r| r.updated_at)
}

/// The watcher's lock file.
pub fn watcher_lock(paths: &Paths, sid: &str) -> PathBuf {
    file(paths, sid, "lock")
}

/// The lock taken to stop and resume the session.
pub fn action_lock(paths: &Paths, sid: &str) -> PathBuf {
    file(paths, sid, "act.lock")
}

/// Whether a watcher holds the session's lock.
pub fn watcher_alive(paths: &Paths, sid: &str) -> bool {
    held(&watcher_lock(paths, sid))
}

/// Whether another process holds the lock at `path`. A shared probe: a
/// watcher that starts during it retries its exclusive lock.
pub fn held(path: &Path) -> bool {
    let Ok(f) = File::open(path) else {
        return false;
    };
    matches!(
        fs4::FileExt::try_lock_shared(&f),
        Err(fs4::TryLockError::WouldBlock)
    )
}

/// An exclusive `flock`; released on drop or when the process exits.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

impl Lock {
    /// The lock, or `None` when another process holds it.
    pub fn try_take(path: &Path) -> Result<Option<Lock>> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => Ok(Some(Lock { _file: file })),
            Err(fs4::TryLockError::WouldBlock) => Ok(None),
            Err(fs4::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("locking {}", path.display()))
            }
        }
    }

    /// The lock, trying for up to `wait`.
    pub fn wait(path: &Path, wait: Duration) -> Result<Option<Lock>> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(l) = Lock::try_take(path)? {
                return Ok(Some(l));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, dir: &str, default: bool) -> Profile {
        let common = if default { dir } else { "/h/.claude" };
        let mut p = Profile::bare(
            id,
            prism_core::model::Tool::Claude,
            dir.into(),
            common.into(),
        );
        p.label = id.to_uppercase();
        p
    }

    fn mv(from: &str, to: &str) -> Move {
        Move {
            at: 1,
            from: from.into(),
            from_label: from.to_uppercase(),
            from_config_dir: Some(format!("/h/.claude-{from}")),
            to: to.into(),
            to_label: to.to_uppercase(),
            to_config_dir: Some(format!("/h/.claude-{to}")),
            reason: "5-hour limit".into(),
        }
    }

    #[test]
    fn json_shape() {
        let mut r = Record {
            session_id: "f1de9de1-x".into(),
            short_id: "f1de9de1".into(),
            name: Some("n".into()),
            cwd: Some("/w".into()),
            state: State::Waiting,
            limit: Some(Limit {
                text: "You've hit your 5-hour limit".into(),
                since: 10,
                resets_at: None,
                window: "5-hour limit".into(),
            }),
            moves: vec![mv("a", "b"), mv("b", "c"), mv("c", "a"), mv("a", "d")],
            watcher_pid: Some(42),
            ..Default::default()
        };
        r.set_profile(&profile("d", "/h/.claude", true));
        r.fill_previous();
        let v = serde_json::to_value(&r).unwrap();
        for k in [
            "session_id",
            "short_id",
            "name",
            "profile",
            "label",
            "config_dir",
            "cwd",
            "on_limit",
            "state",
            "limit",
            "moves",
            "previous",
            "watcher_pid",
            "watching",
            "updated_at",
        ] {
            assert!(v.get(k).is_some(), "{k}");
        }
        assert_eq!(v["config_dir"], serde_json::Value::Null);
        assert_eq!(v["state"], "waiting");
        assert_eq!(v["on_limit"], "switch");
        assert_eq!(v["limit"]["resets_at"], serde_json::Value::Null);
        assert_eq!(v["limit"]["since"], 10);
        for k in [
            "at",
            "from",
            "from_label",
            "from_config_dir",
            "to",
            "to_label",
            "to_config_dir",
            "reason",
        ] {
            assert!(v["moves"][0].get(k).is_some(), "{k}");
        }
        // Oldest first, no repeats, not the current profile.
        let prev: Vec<&str> = r.previous.iter().map(|p| p.profile.as_str()).collect();
        assert_eq!(prev, ["a", "b", "c"]);
        assert_eq!(v["previous"][0]["config_dir"], "/h/.claude-a");
        assert_eq!(
            v["previous"][0]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["profile", "label", "config_dir"]
        );
        r.set_profile(&profile("a", "/h/.claude-a", false));
        r.fill_previous();
        assert_eq!(r.config_dir.as_deref(), Some("/h/.claude-a"));
        let prev: Vec<&str> = r.previous.iter().map(|p| p.profile.as_str()).collect();
        assert_eq!(prev, ["b", "c"]);
        let back: Record = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn effective_on_limit() {
        use prism_core::model::OnLimit as C;
        assert_eq!(OnLimit::effective(None, C::Auto), OnLimit::Switch);
        assert_eq!(OnLimit::effective(None, C::Ask), OnLimit::Switch);
        assert_eq!(OnLimit::effective(None, C::Wait), OnLimit::Wait);
        assert_eq!(OnLimit::effective(None, C::Stay), OnLimit::Stay);
        assert_eq!(
            OnLimit::effective(Some(LimitAction::Fail), C::Auto),
            OnLimit::Stay
        );
        assert_eq!(
            OnLimit::effective(Some(LimitAction::Mode(C::Ask)), C::Stay),
            OnLimit::Switch
        );
        let run = RunOptions {
            on_limit: Some(LimitAction::Mode(C::Ask)),
            max_wait: Some(60),
            exclude: vec!["x".into()],
            ..Default::default()
        };
        let o = Options::new(&run, Some(Policy::Headroom));
        assert_eq!(o.on_limit.as_deref(), Some("ask"));
        assert_eq!(o.run(), run);
    }

    #[test]
    fn update_find_and_locks() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let sid = "f1de9de1-aaaa";
        let r = update(&paths, sid, |r| {
            r.short_id = "f1de9de1".into();
            r.state = State::Moving;
        })
        .unwrap();
        assert!(r.created_at > 0 && r.updated_at > 0);
        assert_eq!(load(&paths, sid).unwrap().state, State::Moving);
        assert_eq!(find(&paths, "f1de9de1").unwrap().session_id, sid);
        assert_eq!(find(&paths, sid).unwrap().session_id, sid);
        assert!(find(&paths, "00000000").is_none());
        assert_eq!(list(&paths).len(), 1);

        assert!(!watcher_alive(&paths, sid));
        let l = Lock::try_take(&watcher_lock(&paths, sid)).unwrap().unwrap();
        // flock locks belong to the open file: a second open in this
        // process conflicts like another process would.
        assert!(
            Lock::try_take(&watcher_lock(&paths, sid))
                .unwrap()
                .is_none()
        );
        assert!(watcher_alive(&paths, sid));
        assert!(list(&paths)[0].watching);
        drop(l);
        assert!(!watcher_alive(&paths, sid));

        // Nobody watches and the job is gone: ended.
        let jobs = dir.path().join("cfg/jobs/f1de9de1");
        std::fs::create_dir_all(&jobs).unwrap();
        update(&paths, sid, |r| {
            r.state = State::Watching;
            r.config_dir = Some(dir.path().join("cfg").to_string_lossy().into_owned());
        })
        .unwrap();
        assert_eq!(list(&paths)[0].state, State::Watching);
        std::fs::remove_dir_all(&jobs).unwrap();
        assert_eq!(list(&paths)[0].state, State::Ended);
    }
}
