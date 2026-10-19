//! Live-session registry `<state>/sessions/<pid>.json` (DESIGN §9).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;

/// One running agent-prism process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub pid: u32,
    pub profile: String,
    pub session_id: Option<String>,
    pub cwd: PathBuf,
    pub started_at: i64,
    #[serde(default)]
    pub switched_at: Option<i64>,
}

fn record_path(paths: &Paths, pid: u32) -> PathBuf {
    paths.sessions_dir().join(format!("{pid}.json"))
}

/// Write (or replace) the record for `rec.pid` atomically.
pub fn write_record(paths: &Paths, rec: &SessionRecord) -> Result<()> {
    let data = serde_json::to_vec_pretty(rec).context("serializing session record")?;
    crate::fsutil::write_atomic(&record_path(paths, rec.pid), &data)
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

/// All records of live agent-prism sessions, sorted by pid. Records of dead pids are deleted;
/// unreadable files are skipped.
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
        if pid_alive(rec.pid) {
            out.push(rec);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    out.sort_by_key(|r| r.pid);
    out
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
}
