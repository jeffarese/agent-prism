//! Reads the end of a Claude transcript (`<session>.jsonl`) to decide
//! whether a session resumed after a limit switch needs a first prompt
//! (DESIGN §9).
//!
//! `busy` only sees turns that the user starts (`UserPromptSubmit`). Turns
//! that a teammate message, a task notification or a `/goal` Stop hook
//! starts fire no hook, and a resumed Claude waits for input. The
//! transcript shows both cases. Its format belongs to Claude Code and is
//! not documented, so anything unreadable counts as "no".

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use serde_json::Value;

/// Read at most this much from the end of the file. Claude Code writes the
/// goal again at each compaction, so the latest one is near the end.
const MAX_SCAN: u64 = 64 * 1024 * 1024;
const CHUNK: usize = 1024 * 1024;

/// Text that local slash commands (`/effort`, `/goal`, ...) write as user
/// entries; they do not start a turn.
const LOCAL_COMMAND: [&str; 5] = [
    "<command-name>",
    "<command-message>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<local-command-caveat>",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tail {
    /// A `/goal` is set and not met. Claude Code restores it on resume, but
    /// its Stop hook only acts when a turn ends.
    pub goal: bool,
    /// The last turn has no final reply: a prompt, teammate message, task
    /// notification or tool result is unanswered, or a tool was running.
    pub open_turn: bool,
}

/// What the end of the transcript at `path` shows.
pub fn inspect(path: &Path) -> Tail {
    inspect_within(path, MAX_SCAN)
}

fn inspect_within(path: &Path, max: u64) -> Tail {
    let mut tail = Tail::default();
    let Ok(lines) = RevLines::open(path, max) else {
        return tail;
    };
    let (mut goal_known, mut turn_known) = (false, false);
    for line in lines {
        let Ok(line) = std::str::from_utf8(&line) else {
            continue;
        };
        let goal_line = !goal_known && line.contains("\"goal_status\"");
        let turn_line = !turn_known
            && (line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\""));
        if !goal_line && !turn_line {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if goal_line && let Some(active) = goal_entry(&v) {
            tail.goal = active;
            goal_known = true;
        }
        if turn_line && let Some(open) = turn_entry(&v) {
            tail.open_turn = open;
            turn_known = true;
        }
        if goal_known && turn_known {
            break;
        }
    }
    tail
}

/// `Some(active)` for a `goal_status` attachment. Same rule as Claude
/// Code's restore on resume: the latest one wins, and a goal that is met
/// or failed is over.
fn goal_entry(v: &Value) -> Option<bool> {
    if v.get("type")?.as_str()? != "attachment" {
        return None;
    }
    let a = v.get("attachment")?;
    if a.get("type")?.as_str()? != "goal_status" {
        return None;
    }
    let flag = |k: &str| a.get(k).and_then(Value::as_bool).unwrap_or(false);
    let condition = a
        .get("condition")
        .and_then(Value::as_str)
        .is_some_and(|c| !c.is_empty());
    Some(condition && !flag("met") && !flag("failed"))
}

/// `Some(open)` for an entry that decides whether the last turn is open;
/// `None` for entries that do not (local commands, synthetic replies,
/// compaction summaries, sidechains).
fn turn_entry(v: &Value) -> Option<bool> {
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    if flag("isSidechain") || flag("isCompactSummary") || flag("isVisibleInTranscriptOnly") {
        return None;
    }
    let msg = v.get("message")?;
    match v.get("type")?.as_str()? {
        "assistant" => {
            // "No response requested." on resume, API errors: no reply.
            if msg.get("model").and_then(Value::as_str) == Some("<synthetic>") {
                return None;
            }
            match msg.get("stop_reason").and_then(Value::as_str) {
                None | Some("tool_use") => Some(true),
                Some(_) => Some(false),
            }
        }
        "user" => {
            let texts = texts(msg.get("content")?);
            if texts
                .iter()
                .any(|t| t.starts_with("[Request interrupted by user"))
            {
                return Some(false);
            }
            if !texts.is_empty()
                && texts
                    .iter()
                    .all(|t| LOCAL_COMMAND.iter().any(|p| t.starts_with(p)))
            {
                return None;
            }
            Some(true)
        }
        _ => None,
    }
}

/// The text of a message's content: a string, or the text blocks.
fn texts(content: &Value) -> Vec<&str> {
    match content {
        Value::String(s) => vec![s.trim_start()],
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .map(str::trim_start)
            .collect(),
        _ => Vec::new(),
    }
}

/// The lines of a file from the last to the first, within `max` bytes of
/// the end. A line cut by the limit is not returned.
struct RevLines {
    file: File,
    /// File offset of `buf[0]`.
    pos: u64,
    floor: u64,
    buf: Vec<u8>,
}

impl RevLines {
    fn open(path: &Path, max: u64) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            pos: len,
            floor: len.saturating_sub(max),
            buf: Vec::new(),
        })
    }
}

impl Iterator for RevLines {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            while self.buf.last() == Some(&b'\n') {
                self.buf.pop();
            }
            if let Some(i) = self.buf.iter().rposition(|&b| b == b'\n') {
                return Some(self.buf.split_off(i + 1));
            }
            if self.pos <= self.floor {
                // At the start of the file the rest is the first line; at
                // the limit it is a cut line.
                return (self.pos == 0 && !self.buf.is_empty())
                    .then(|| std::mem::take(&mut self.buf));
            }
            // Grow the read with the buffer so a long line costs O(n).
            let want = CHUNK.max(self.buf.len()) as u64;
            let n = want.min(self.pos - self.floor);
            let start = self.pos - n;
            let mut chunk = vec![0u8; n as usize];
            self.file.read_exact_at(&mut chunk, start).ok()?;
            chunk.extend_from_slice(&self.buf);
            self.buf = chunk;
            self.pos = start;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(lines: &[Value]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut text = String::new();
        for l in lines {
            text.push_str(&l.to_string());
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    fn user(content: Value) -> Value {
        json!({"type":"user","isSidechain":false,"message":{"role":"user","content":content}})
    }

    fn meta(text: &str) -> Value {
        json!({"type":"user","isMeta":true,"message":{"role":"user","content":text}})
    }

    fn assistant(stop: &str) -> Value {
        json!({"type":"assistant","message":{"model":"claude-opus-5-5","stop_reason":stop,
            "content":[{"type":"text","text":"done"}]}})
    }

    fn synthetic() -> Value {
        json!({"type":"assistant","message":{"model":"<synthetic>","stop_reason":"stop_sequence",
            "content":[{"type":"text","text":"No response requested."}]}})
    }

    fn goal(met: bool) -> Value {
        json!({"type":"attachment","attachment":{"type":"goal_status","met":met,
            "sentinel":true,"condition":"ship it"}})
    }

    fn other() -> Value {
        json!({"type":"system","subtype":"stop_hook_summary","content":"x"})
    }

    fn tail(lines: &[Value]) -> Tail {
        let (_d, p) = write(lines);
        inspect(&p)
    }

    #[test]
    fn finished_turn_without_goal() {
        let t = tail(&[user(json!("fix it")), assistant("end_turn"), other()]);
        assert_eq!(t, Tail::default());
    }

    /// The incident: a teammate report arrived, the swap killed the turn
    /// before any reply, and a `/goal` was active.
    #[test]
    fn teammate_message_unanswered_with_goal() {
        let t = tail(&[
            goal(false),
            user(json!("go")),
            assistant("end_turn"),
            other(),
            user(json!(
                "Another Claude session sent a message:\n<teammate-message>done</teammate-message>"
            )),
            json!({"type":"queue-operation","operation":"enqueue"}),
        ]);
        assert_eq!(
            t,
            Tail {
                goal: true,
                open_turn: true
            }
        );
    }

    #[test]
    fn met_goal_is_over() {
        let t = tail(&[goal(false), goal(true), assistant("end_turn")]);
        assert!(!t.goal);
        let failed = json!({"type":"attachment","attachment":{"type":"goal_status",
            "met":false,"failed":true,"condition":"x"}});
        assert!(!tail(&[goal(false), failed]).goal);
    }

    #[test]
    fn running_tool_and_tool_result_are_open() {
        assert!(tail(&[assistant("tool_use")]).open_turn);
        let result = user(json!([{"type":"tool_result","tool_use_id":"t","content":"ok"}]));
        assert!(tail(&[assistant("tool_use"), result]).open_turn);
    }

    #[test]
    fn synthetic_reply_is_skipped() {
        // A resume left the teammate message unanswered.
        let t = tail(&[assistant("end_turn"), user(json!("report")), synthetic()]);
        assert!(t.open_turn);
        let t = tail(&[user(json!("hi")), assistant("end_turn"), synthetic()]);
        assert!(!t.open_turn);
    }

    #[test]
    fn local_commands_do_not_open_a_turn() {
        let t = tail(&[
            assistant("end_turn"),
            meta("<local-command-caveat>Caveat: ...</local-command-caveat>"),
            user(json!(
                "<command-name>/effort</command-name>\n<command-message>effort</command-message>"
            )),
            user(json!(
                "<local-command-stdout>Set effort</local-command-stdout>"
            )),
        ]);
        assert!(!t.open_turn);
        // `/goal` adds a meta prompt that starts a turn.
        let t = tail(&[
            assistant("end_turn"),
            user(json!("<command-name>/goal</command-name>")),
            meta("A session-scoped Stop hook is now active with condition: \"x\"."),
        ]);
        assert!(t.open_turn);
    }

    #[test]
    fn user_interrupt_closes_the_turn() {
        let esc = user(json!([{"type":"text","text":"[Request interrupted by user]"}]));
        assert!(!tail(&[assistant("tool_use"), esc]).open_turn);
    }

    #[test]
    fn missing_file_and_garbage() {
        assert_eq!(inspect(Path::new("/nonexistent/x.jsonl")), Tail::default());
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("g.jsonl");
        std::fs::write(&p, "not json\n{\"type\":\"user\"\n").unwrap();
        assert_eq!(inspect(&p), Tail::default());
    }

    #[test]
    fn long_lines_and_scan_limit() {
        let big = "x".repeat(3 * CHUNK);
        let (_d, p) = write(&[
            goal(false),
            json!({"type":"api-request-blob","blob":big}),
            user(json!("go")),
        ]);
        assert_eq!(
            inspect(&p),
            Tail {
                goal: true,
                open_turn: true
            }
        );
        // The goal lies beyond the limit: not seen; the last line still is.
        let t = inspect_within(&p, 1024);
        assert!(!t.goal);
        assert!(t.open_turn);
    }

    #[test]
    fn rev_lines_order_and_partial_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("l");
        std::fs::write(&p, "one\ntwo\n\nthree").unwrap();
        let all: Vec<String> = RevLines::open(&p, u64::MAX)
            .unwrap()
            .map(|l| String::from_utf8(l).unwrap())
            .collect();
        assert_eq!(all, ["three", "two", "one"]);
        // A limit inside "two" drops the cut line and everything before it.
        let cut: Vec<String> = RevLines::open(&p, 8)
            .unwrap()
            .map(|l| String::from_utf8(l).unwrap())
            .collect();
        assert_eq!(cut, ["three"]);
    }
}
