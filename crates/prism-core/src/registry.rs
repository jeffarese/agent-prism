//! Live-run registry `<state>/sessions/<pid>.json` (DESIGN §6.7, §9).
//!
//! Every run agent-prism starts holds a record (a claim) on its account
//! while it runs: interactive sessions, print-mode runs (also those that
//! replace themselves with the tool: the pid stays the same across
//! `exec`), supervised print runs, and `--bg` launches and moves. The
//! balancer counts them, so runs that start together spread over the
//! accounts. Picks that write a claim hold the pick lock ([`PickLock`]).

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::journal::RunMode;
use crate::model::Tool;
use crate::paths::Paths;

/// The weight of an interactive session that is not working on a turn.
pub const IDLE_WEIGHT: f64 = 0.25;
/// The weight of an interactive Codex session: agent-prism can't tell
/// whether it works (Codex sends no hooks).
pub const CODEX_TUI_WEIGHT: f64 = 0.5;

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
    /// [`IDLE_WEIGHT`] of a run.
    #[serde(default = "yes")]
    pub busy: bool,
    /// When the process started (unix seconds), so that a reused pid does
    /// not keep a claim alive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_start: Option<i64>,
}

impl SessionRecord {
    /// The balancer weight of this run: 1 while it works, less for an
    /// interactive session that waits for its user.
    pub fn weight(&self) -> f64 {
        match (self.tool, self.mode, self.busy) {
            (Tool::Codex, RunMode::Tui, _) => CODEX_TUI_WEIGHT,
            (_, _, true) => 1.0,
            (_, _, false) => IDLE_WEIGHT,
        }
    }
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
pub fn loads(records: &[SessionRecord], skip: u32) -> HashMap<String, Load> {
    let mut out: HashMap<String, Load> = HashMap::new();
    for r in records.iter().filter(|r| r.pid != skip) {
        out.entry(r.profile.clone()).or_default().add(r.weight());
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

/// When process `pid` started; not known on this platform.
#[cfg(not(target_os = "macos"))]
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
    _file: File,
}

impl PickLock {
    /// Waits up to `wait` for the lock. `None` on timeout or error (the
    /// caller then picks without it; a launch never fails for this).
    pub fn acquire(paths: &Paths, wait: Duration) -> Option<PickLock> {
        use std::os::fd::AsRawFd;
        let path = paths.state_dir.join("pick.lock");
        if let Err(e) = std::fs::create_dir_all(&paths.state_dir) {
            log::warn!("pick lock: {e}");
            return None;
        }
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                log::warn!("pick lock {}: {e}", path.display());
                return None;
            }
        };
        let deadline = Instant::now() + wait;
        loop {
            // SAFETY: flock on a descriptor we own.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Some(PickLock { _file: file });
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
                log::warn!("pick lock: {err}");
                return None;
            }
            if Instant::now() >= deadline {
                log::warn!("pick lock busy for {wait:?}; picking without it");
                return None;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

#[cfg(test)]
mod tests {
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
        let l = loads(&[rec(1), idle, b, rec(4)], 4);
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
        assert_eq!(codex.weight(), CODEX_TUI_WEIGHT);
        codex.mode = RunMode::Exec;
        assert_eq!(codex.weight(), 1.0);
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
