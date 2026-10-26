//! Run journal `<state>/runs.jsonl` (DESIGN §2): one JSON line per event of
//! a run (start, limit, wait, switch, resume, end), for the dashboard.
//!
//! Appends are one `write` of a whole line in append mode, so concurrent
//! agent-prism processes never interleave inside a line. The file is cut to
//! its newest half when it grows past [`MAX_BYTES`]. Never fails a run:
//! errors are logged and dropped.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::Tool;
use crate::paths::Paths;

/// The journal is cut when it grows past this size.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// The run started on `profile`.
    Start,
    /// `profile` hit a limit (`limit` = 5h | 7d | scoped).
    Limit,
    /// The run waits for `profile` to reset, until `until`.
    Wait,
    /// The run moved from `from` to `profile`.
    Switch,
    /// The run went on on `profile` after a wait.
    Resume,
    /// The run ended with `exit`.
    End,
}

/// How the run uses the tool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// Interactive session in the wrapper (dock).
    #[default]
    Tui,
    /// `agp -p` / `agp codex exec` replaced by the tool (no end event).
    Exec,
    /// `agp -p` / `agp codex exec` with `--on-limit`: agent-prism stays the
    /// parent and handles limits.
    Print,
}

/// One journal line. Optional fields are left out when empty.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Unix seconds.
    pub ts: i64,
    /// Run id: `<pid>-<start ts>`, the same for all events of a run.
    pub run: String,
    pub ev: Option<EventKind>,
    #[serde(default)]
    pub tool: Tool,
    #[serde(default)]
    pub mode: RunMode,
    /// The account the event is about (the new one for a switch).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    /// Switch: the previous account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Limit: `5h`, `7d` or `scoped`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
    /// Wait: when the account resets (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    /// Start / switch: why this account (balancer reason, "pinned", "manual").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Claude session id or Codex thread id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Start: the work dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// End: exit code of the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    /// End: seconds since start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secs: Option<i64>,
}

/// `<state>/runs.jsonl`.
pub fn file(paths: &Paths) -> PathBuf {
    paths.state_dir.join("runs.jsonl")
}

/// The id of a run of this process that started at `started_at`.
pub fn run_id(started_at: i64) -> String {
    format!("{}-{started_at}", std::process::id())
}

/// Appends `ev`; logs and drops errors.
pub fn append(paths: &Paths, ev: &Event) {
    if let Err(e) = append_to(&file(paths), ev) {
        log::warn!("run journal: {e:#}");
    }
}

fn append_to(path: &Path, ev: &Event) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_vec(ev)?;
    line.push(b'\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(&line)?;
    if f.metadata().is_ok_and(|m| m.len() > MAX_BYTES) {
        drop(f);
        trim(path)?;
    }
    Ok(())
}

/// Keeps the newest half of the lines.
fn trim(path: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let keep = &lines[lines.len() / 2..];
    let mut out = keep.join("\n");
    out.push('\n');
    crate::fsutil::write_atomic(path, out.as_bytes())
}

/// Every readable event with `ts >= since`, in file order. Bad lines are
/// skipped.
pub fn read_since(paths: &Paths, since: i64) -> Vec<Event> {
    let Ok(text) = std::fs::read_to_string(file(paths)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .filter(|e| e.ts >= since)
        .collect()
}

/// Writes the events of one run with shared fields filled in.
#[derive(Clone, Debug)]
pub struct Recorder {
    paths: Paths,
    run: String,
    tool: Tool,
    mode: RunMode,
    started_at: i64,
}

impl Recorder {
    /// A recorder for a run of this process that started at `started_at`.
    pub fn new(paths: Paths, tool: Tool, mode: RunMode, started_at: i64) -> Self {
        Self {
            paths,
            run: run_id(started_at),
            tool,
            mode,
            started_at,
        }
    }

    /// An event of `kind` about `profile` at `ts`, for the caller to fill in.
    pub fn event(&self, kind: EventKind, profile: &str, ts: i64) -> Event {
        Event {
            ts,
            run: self.run.clone(),
            ev: Some(kind),
            tool: self.tool,
            mode: self.mode,
            profile: profile.to_string(),
            ..Default::default()
        }
    }

    /// Appends `ev`.
    pub fn write(&self, ev: Event) {
        append(&self.paths, &ev);
    }

    /// Appends an end event with the exit code and the run's length.
    pub fn end(&self, profile: &str, ts: i64, exit: i32, session: Option<String>) {
        let mut ev = self.event(EventKind::End, profile, ts);
        ev.exit = Some(exit);
        ev.secs = Some((ts - self.started_at).max(0));
        ev.session = session;
        self.write(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_read_and_trim() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let rec = Recorder::new(paths.clone(), Tool::Claude, RunMode::Print, 100);
        let mut ev = rec.event(EventKind::Start, "a", 100);
        ev.reason = Some("most 5h headroom".into());
        rec.write(ev);
        rec.end("a", 160, 0, Some("sid".into()));
        let all = read_since(&paths, 0);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].ev, Some(EventKind::Start));
        assert_eq!(all[0].mode, RunMode::Print);
        assert_eq!(all[1].secs, Some(60));
        assert_eq!(all[1].session.as_deref(), Some("sid"));
        assert_eq!(read_since(&paths, 150).len(), 1);
        // Optional fields stay out of the line.
        let text = std::fs::read_to_string(file(&paths)).unwrap();
        assert!(!text.lines().next().unwrap().contains("exit"));

        std::fs::write(file(&paths), "x\n".repeat(10)).unwrap();
        trim(&file(&paths)).unwrap();
        assert_eq!(
            std::fs::read_to_string(file(&paths))
                .unwrap()
                .lines()
                .count(),
            5
        );
    }
}
