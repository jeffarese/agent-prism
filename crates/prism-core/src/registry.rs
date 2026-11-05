//! Live-run registry `<state>/sessions/<pid>.json` (DESIGN §6.7, §9).
//!
//! Every run agent-prism starts holds a record (a claim) on its account
//! while it runs: interactive sessions, print-mode runs (also those that
//! replace themselves with the tool: the pid stays the same across
//! `exec`), supervised print runs, and `--bg` launches and moves. The
//! balancer counts them, so runs that start together spread over the
//! accounts. Picks that write a claim hold the pick lock ([`PickLock`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::fsutil::FileLock;
use crate::journal::RunMode;
use crate::model::Tool;
use crate::paths::Paths;

/// The weight of an interactive session that is not working on a turn.
pub const IDLE_WEIGHT: f64 = 0.25;
/// The weight of an interactive Codex session: agent-prism can't tell
/// whether it works (Codex sends no hooks).
pub const CODEX_TUI_WEIGHT: f64 = 0.5;
/// How long after the last write to a session's subagent or workflow
/// transcripts its background work counts as running. Agents append to
/// their transcripts as they work; a long tool call can pause that.
pub const BACKGROUND_SECS: i64 = 600;

fn yes() -> bool {
    true
}

/// One running agent-prism process and the account it uses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub pid: u32,
    pub profile: String,
    pub session_id: Option<String>,
    pub cwd: PathBuf,
    pub started_at: i64,
    #[serde(default)]
    pub switched_at: Option<i64>,
    /// Claude or Codex.
    #[serde(default)]
    pub tool: Tool,
    /// How the run uses the tool (records from older versions: `tui`).
    #[serde(default)]
    pub mode: RunMode,
    /// Working on a turn now. An idle interactive session counts as
    /// [`IDLE_WEIGHT`] of a run, unless its background work runs (see
    /// [`SessionRecord::background`]).
    #[serde(default = "yes")]
    pub busy: bool,
    /// When the process started (unix seconds), so that a reused pid does
    /// not keep a claim alive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_start: Option<i64>,
    /// How many typical runs this one counts as while it works, learned
    /// from past runs like it (DESIGN §11f); none: one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<f64>,
    /// The session's transcript (Claude's `transcript_path`), whose
    /// sibling folder holds its subagent and workflow transcripts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<PathBuf>,
}

impl SessionRecord {
    /// Subagents or workflows of this session work in the background: a
    /// turn that starts them can end while they run on.
    pub fn background(&self, now: i64) -> bool {
        let since = self.switched_at.unwrap_or(self.started_at);
        self.transcript
            .as_deref()
            .is_some_and(|t| background_active(t, since, now))
    }

    /// Working on a turn or in the background.
    pub fn working(&self, now: i64) -> bool {
        self.busy || self.background(now)
    }

    /// The balancer weight of this run: 1 (or its learned `load`) while it
    /// works, less for an interactive session that waits for its user.
    pub fn weight(&self, now: i64) -> f64 {
        if (self.tool, self.mode) == (Tool::Codex, RunMode::Tui) {
            return CODEX_TUI_WEIGHT;
        }
        if self.working(now) {
            self.load.filter(|w| *w > 0.0).unwrap_or(1.0)
        } else {
            IDLE_WEIGHT
        }
    }
}

/// The folder of a session's subagent and workflow transcripts:
/// `<transcript without .jsonl>/subagents`.
pub fn subagents_dir(transcript: &Path) -> PathBuf {
    transcript.with_extension("").join("subagents")
}

/// The newest write (unix seconds) to a file in the session's subagents
/// folder (`agent-*.jsonl`, `workflows/<run>/agent-*.jsonl`, ...).
pub fn subagent_activity(transcript: &Path) -> Option<i64> {
    newest_mtime(&subagents_dir(transcript), 3)
}

fn newest_mtime(dir: &Path, depth: u32) -> Option<i64> {
    let mut best = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let t = if ft.is_dir() {
            if depth == 0 {
                continue;
            }
            newest_mtime(&e.path(), depth - 1)
        } else {
            e.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| i64::try_from(d.as_secs()).ok())
        };
        best = best.max(t);
    }
    best
}

/// The session's background work runs: a subagent or workflow transcript
/// was written since `since` (work from before a restart died with the
/// old process) and within [`BACKGROUND_SECS`] of `now`.
pub fn background_active(transcript: &Path, since: i64, now: i64) -> bool {
    subagent_activity(transcript).is_some_and(|t| t >= since.max(now - BACKGROUND_SECS))
}

/// The transcript of Claude session `session_id` under a `projects`
/// folder (`<projects>/<project>/<id>.jsonl`), when one exists.
pub fn find_transcript(projects: &Path, session_id: &str) -> Option<PathBuf> {
    let file = format!("{session_id}.jsonl");
    std::fs::read_dir(projects)
        .ok()?
        .flatten()
        .map(|e| e.path().join(&file))
        .find(|p| p.exists())
}

/// The runs on one account.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Load {
    /// How many runs.
    pub runs: u32,
    /// Their summed weight.
    pub weight: f64,
}

impl Load {
    /// Adds one run of `weight`.
    pub fn add(&mut self, weight: f64) {
        self.runs += 1;
        self.weight += weight;
    }
}

/// The runs per profile id in `records`, without the process `skip`.
pub fn loads(records: &[SessionRecord], skip: u32, now: i64) -> HashMap<String, Load> {
    let mut out: HashMap<String, Load> = HashMap::new();
    for r in records.iter().filter(|r| r.pid != skip) {
        out.entry(r.profile.clone()).or_default().add(r.weight(now));
    }
    out
}

fn record_path(paths: &Paths, pid: u32) -> PathBuf {
    paths.sessions_dir().join(format!("{pid}.json"))
}

/// Write (or replace) the record for `rec.pid` atomically.
pub fn write_record(paths: &Paths, rec: &SessionRecord) -> Result<()> {
    let data = serde_json::to_vec_pretty(rec).context("serializing session record")?;
    crate::fsutil::write_atomic(&record_path(paths, rec.pid), &data)
}

/// The record of `pid`, if there is one.
pub fn read_record(paths: &Paths, pid: u32) -> Option<SessionRecord> {
    let b = std::fs::read(record_path(paths, pid)).ok()?;
    serde_json::from_slice(&b).ok()
}

/// Remove the record for `pid`, ignoring errors.
pub fn remove_record(paths: &Paths, pid: u32) {
    let _ = std::fs::remove_file(record_path(paths, pid));
}

/// True unless the process certainly does not exist.
pub(crate) fn pid_alive(pid: u32) -> bool {
    let Ok(p) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if p <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 only checks for existence/permission.
    let r = unsafe { libc::kill(p, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// When process `pid` started (unix seconds); `None` when it can't be read.
#[cfg(target_os = "macos")]
pub fn process_start(pid: u32) -> Option<i64> {
    let p = libc::c_int::try_from(pid).ok()?;
    // SAFETY: proc_pidinfo fills at most `size` bytes of `info`, a plain C
    // struct for which all-zero bytes are a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            p,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (n == size).then_some(info.pbi_start_tvsec as i64)
}

/// When process `pid` started (unix seconds), from `/proc`; `None` when it
/// can't be read.
#[cfg(target_os = "linux")]
pub fn process_start(pid: u32) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let ticks = start_ticks(&stat)?;
    // SAFETY: sysconf only reads a configuration value.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    let btime: i64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    Some(btime + ticks / hz as i64)
}

/// Field 22 of `/proc/<pid>/stat` (start time in clock ticks after boot).
/// The command name (field 2) may hold spaces and parentheses, so count
/// from the last `)`.
#[cfg(any(target_os = "linux", test))]
fn start_ticks(stat: &str) -> Option<i64> {
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// When process `pid` started; not known on this platform.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn process_start(_pid: u32) -> Option<i64> {
    None
}

/// The record's process runs: its pid exists and, when both are known,
/// started when the record says.
fn record_alive(rec: &SessionRecord) -> bool {
    if !pid_alive(rec.pid) {
        return false;
    }
    match (rec.pid_start, process_start(rec.pid)) {
        (Some(want), Some(got)) => (want - got).abs() <= 2,
        _ => true,
    }
}

/// All records of live runs, sorted by pid. Records of dead processes are
/// deleted; unreadable files are skipped.
pub fn list_live(paths: &Paths) -> Vec<SessionRecord> {
    let Ok(rd) = std::fs::read_dir(paths.sessions_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Some(rec) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<SessionRecord>(&b).ok())
        else {
            continue;
        };
        if record_alive(&rec) {
            out.push(rec);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    out.sort_by_key(|r| r.pid);
    out
}

/// The exclusive pick lock `<state>/pick.lock`, held from reading the
/// claims to writing a new one, so that runs that start together see each
/// other. Released on drop (and by the kernel when the process exits or
/// execs: the file is close-on-exec).
#[derive(Debug)]
pub struct PickLock {
    _lock: FileLock,
}

impl PickLock {
    /// Waits up to `wait` for the lock. `None` on timeout or error (the
    /// caller then picks without it; a launch never fails for this).
    pub fn acquire(paths: &Paths, wait: Duration) -> Option<PickLock> {
        let path = paths.state_dir.join("pick.lock");
        match FileLock::acquire(&path, wait) {
            Ok(Some(l)) => Some(PickLock { _lock: l }),
            Ok(None) => {
                log::warn!("pick lock busy for {wait:?}; picking without it");
                None
            }
            Err(e) => {
                log::warn!("pick lock {}: {e}", path.display());
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn proc_stat_start_ticks() {
        let stat = "4242 (we(ir d)) S 1 4242 4242 0 -1 4194304 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";
        assert_eq!(super::start_ticks(stat), Some(987654));
        assert_eq!(super::start_ticks("1 (x) S"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn own_start_is_known() {
        let s = super::process_start(std::process::id()).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(s <= now + 2 && s > now - 3600);
    }

    use super::*;

    fn rec(pid: u32) -> SessionRecord {
        SessionRecord {
            pid,
            profile: "a".into(),
            session_id: None,
            cwd: "/tmp".into(),
            started_at: 1,
            switched_at: None,
            tool: Tool::Claude,
            mode: RunMode::Tui,
            busy: true,
            pid_start: None,
            load: None,
            transcript: None,
        }
    }

    #[test]
    fn prunes_dead() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let me = std::process::id();
        // A child that has exited and been reaped is dead.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        write_record(&paths, &rec(me)).unwrap();
        write_record(&paths, &rec(dead)).unwrap();
        let live = list_live(&paths);
        assert_eq!(live, vec![rec(me)]);
        assert!(!record_path(&paths, dead).exists());
        remove_record(&paths, me);
        assert!(list_live(&paths).is_empty());
    }

    #[test]
    fn old_records_and_reused_pids() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let me = std::process::id();
        // A record from an older version (no tool, mode, busy, pid_start).
        std::fs::create_dir_all(paths.sessions_dir()).unwrap();
        std::fs::write(
            record_path(&paths, me),
            format!(r#"{{"pid":{me},"profile":"a","session_id":null,"cwd":"/","started_at":1}}"#),
        )
        .unwrap();
        let live = list_live(&paths);
        assert_eq!(live.len(), 1);
        assert_eq!((live[0].mode, live[0].busy), (RunMode::Tui, true));
        // This pid with another start time: a reused pid, not our run.
        if let Some(start) = process_start(me) {
            let mut r = rec(me);
            r.pid_start = Some(start - 3600);
            write_record(&paths, &r).unwrap();
            assert!(list_live(&paths).is_empty());
            r.pid_start = Some(start);
            write_record(&paths, &r).unwrap();
            assert_eq!(list_live(&paths), vec![r]);
        }
    }

    #[test]
    fn loads_weigh_idle_sessions_less() {
        let mut idle = rec(2);
        idle.busy = false;
        let mut b = rec(3);
        b.profile = "b".into();
        let l = loads(&[rec(1), idle, b, rec(4)], 4, 0);
        assert_eq!(
            l["a"],
            Load {
                runs: 2,
                weight: 1.0 + IDLE_WEIGHT
            }
        );
        assert_eq!(l["b"].runs, 1);
        let mut codex = rec(5);
        codex.tool = Tool::Codex;
        assert_eq!(codex.weight(0), CODEX_TUI_WEIGHT);
        codex.mode = RunMode::Exec;
        assert_eq!(codex.weight(0), 1.0);
        // A learned load counts while the run works; idle stays a quarter.
        let mut heavy = rec(6);
        heavy.load = Some(1.9);
        assert_eq!(heavy.weight(0), 1.9);
        heavy.busy = false;
        assert_eq!(heavy.weight(0), IDLE_WEIGHT);
        // Records of older versions have no load.
        let old: SessionRecord = serde_json::from_str(
            r#"{"pid":1,"profile":"a","session_id":null,"cwd":"/","started_at":1}"#,
        )
        .unwrap();
        assert_eq!(old.load, None);
        assert_eq!(old.weight(0), 1.0);
    }

    fn touch(path: &Path, at: i64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let f = std::fs::File::create(path).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(at as u64))
            .unwrap();
    }

    #[test]
    fn background_work_keeps_the_weight() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("proj/s1.jsonl");
        touch(&transcript, 1_000);
        let now = 10_000;
        let mut lane = rec(7);
        lane.busy = false;
        lane.load = Some(1.9);
        lane.started_at = 5_000;
        lane.transcript = Some(transcript.clone());
        // No subagents yet: an idle session.
        assert!(!lane.background(now));
        assert_eq!(lane.weight(now), IDLE_WEIGHT);
        // A workflow agent wrote a minute ago: the turn ended, the work goes on.
        let agent = tmp
            .path()
            .join("proj/s1/subagents/workflows/wf_1/agent-a.jsonl");
        touch(&agent, now - 60);
        assert_eq!(subagent_activity(&transcript), Some(now - 60));
        assert!(lane.background(now) && lane.working(now));
        assert_eq!(lane.weight(now), 1.9);
        assert_eq!(loads(&[lane.clone()], 0, now)["a"].weight, 1.9);
        // Quiet for longer than BACKGROUND_SECS: done.
        assert!(!lane.background(now + BACKGROUND_SECS + 61));
        // Work from before a move died with the old process.
        lane.switched_at = Some(now - 30);
        assert!(!lane.background(now));
        // A plain subagent counts too.
        touch(
            &tmp.path().join("proj/s1/subagents/agent-b.jsonl"),
            now - 10,
        );
        assert!(lane.background(now));
        assert_eq!(find_transcript(tmp.path(), "s1"), Some(transcript));
        assert_eq!(find_transcript(tmp.path(), "s2"), None);
    }

    #[test]
    fn pick_lock_is_exclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let a = PickLock::acquire(&paths, Duration::from_millis(50)).unwrap();
        // flock is per open file: a second open in this process waits too.
        assert!(PickLock::acquire(&paths, Duration::from_millis(60)).is_none());
        drop(a);
        assert!(PickLock::acquire(&paths, Duration::from_millis(50)).is_some());
    }
}
