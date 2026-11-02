//! Reads one Claude session's transcripts into [`SessionStats`] (DESIGN
//! §11f): the main `<session>.jsonl` and every agent's transcript under
//! `<session>/subagents/` (workflow agents under `subagents/workflows/<run>/`).
//!
//! The format belongs to Claude Code and is not documented. Lines that don't
//! parse are skipped. Claude Code writes one line per content block of a
//! response and repeats the response's `usage` on each, so usage counts once
//! per message id.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::IgnoredAny;

use super::store::{SessionStats, Tokens, VERSION};

/// Gaps between messages up to this long count as working time.
pub const GAP_SECS: i64 = 300;
/// Length of a usage bucket.
pub const BUCKET_SECS: i64 = 300;
/// The first prompt is kept up to this many characters.
const PROMPT_CHARS: usize = 200;

/// Text that is not a prompt the user typed: local commands, reminders and
/// the caveat Claude Code writes before command output.
const NOT_A_PROMPT: [&str; 8] = [
    "<command-name>",
    "<command-message>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<local-command-caveat>",
    "<system-reminder>",
    "Caveat:",
    "[Request interrupted",
];

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "gitBranch")]
    git_branch: Option<String>,
    #[serde(default)]
    entrypoint: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default, rename = "isMeta")]
    is_meta: Option<bool>,
    #[serde(default, rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(default, rename = "requestId")]
    request_id: Option<String>,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    content: Option<Content>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Block>),
    Other(IgnoredAny),
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

impl Usage {
    fn tokens(&self) -> Tokens {
        Tokens {
            input: self.input_tokens.unwrap_or(0),
            cache_write: self.cache_creation_input_tokens.unwrap_or(0),
            cache_read: self.cache_read_input_tokens.unwrap_or(0),
            output: self.output_tokens.unwrap_or(0),
        }
    }
}

/// One response's usage, counted once.
struct Response {
    ts: i64,
    model: String,
    main: bool,
    tokens: Tokens,
}

/// What the scan collects before [`Acc::finish`].
#[derive(Default)]
struct Acc {
    responses: HashMap<String, Response>,
    /// Responses without an id (counted as they come).
    loose: Vec<Response>,
    times: Vec<i64>,
    effort: HashMap<String, u32>,
    tools: BTreeMap<String, u32>,
    prompt: Option<String>,
    title: Option<String>,
    cwd: Option<String>,
    branch: Option<String>,
    entry: Option<String>,
}

/// The transcript files of a session: the main file, then every agent's.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionFiles {
    pub main: PathBuf,
    /// Plain subagents (`subagents/*.jsonl`).
    pub agents: Vec<PathBuf>,
    /// Workflow agents by workflow run (`subagents/workflows/<run>/*.jsonl`).
    pub workflows: BTreeMap<String, Vec<PathBuf>>,
    /// Total size and newest mtime (unix seconds) of all of them.
    pub bytes: u64,
    pub mtime: i64,
}

/// Lists the files of the session whose main transcript is `main`.
pub fn session_files(main: &Path) -> SessionFiles {
    let mut f = SessionFiles {
        main: main.to_path_buf(),
        ..Default::default()
    };
    add_size(&mut f, main);
    let sub = main.with_extension("").join("subagents");
    for p in jsonl_in(&sub) {
        add_size(&mut f, &p);
        f.agents.push(p);
    }
    if let Ok(rd) = std::fs::read_dir(sub.join("workflows")) {
        for e in rd.flatten() {
            let run = e.file_name().to_string_lossy().into_owned();
            let files = jsonl_in(&e.path());
            for p in &files {
                add_size(&mut f, p);
            }
            if !files.is_empty() {
                f.workflows.insert(run, files);
            }
        }
    }
    f
}

fn jsonl_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl") && p.is_file())
        .collect();
    v.sort();
    v
}

fn add_size(f: &mut SessionFiles, p: &Path) {
    if let Ok(m) = std::fs::metadata(p) {
        f.bytes += m.len();
        let mt = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        f.mtime = f.mtime.max(mt);
    }
}

/// Summarizes a session. `None` when it has no model response.
pub fn summarize(session: &str, files: &SessionFiles) -> Option<SessionStats> {
    let mut acc = Acc::default();
    scan_file(&files.main, true, &mut acc);
    for p in files
        .agents
        .iter()
        .chain(files.workflows.values().flatten())
    {
        scan_file(p, false, &mut acc);
    }
    let mut stats = acc.finish(session)?;
    stats.agents = files.agents.len() as u32;
    stats.workflows = files.workflows.len() as u32;
    stats.workflow_agents = files.workflows.values().map(|v| v.len() as u32).sum();
    stats.src_bytes = files.bytes;
    stats.src_mtime = files.mtime;
    Some(stats)
}

fn scan_file(path: &Path, main: bool, acc: &mut Acc) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&buf);
        let wanted = text.contains("\"type\":\"assistant\"")
            || (main && text.contains("\"type\":\"ai-title\""))
            || (main && acc.prompt.is_none() && text.contains("\"type\":\"user\""));
        if !wanted {
            continue;
        }
        if let Ok(line) = serde_json::from_str::<Line>(&text) {
            acc.add(line, main);
        }
    }
}

fn parse_ts(s: &str) -> Option<i64> {
    s.parse::<jiff::Timestamp>().ok().map(|t| t.as_second())
}

impl Acc {
    fn add(&mut self, line: Line, main: bool) {
        if main {
            if self.cwd.is_none() {
                self.cwd = line.cwd.clone().filter(|c| !c.is_empty());
            }
            if self.branch.is_none() {
                self.branch = line.git_branch.clone().filter(|b| !b.is_empty());
            }
        }
        match line.kind.as_str() {
            "ai-title" if main => {
                if let Some(t) = line.ai_title.filter(|t| !t.trim().is_empty()) {
                    self.title = Some(t.trim().to_string());
                }
            }
            "user" if main && self.prompt.is_none() && line.is_meta != Some(true) => {
                let text = match line.message.and_then(|m| m.content) {
                    Some(Content::Text(t)) => Some(t),
                    Some(Content::Blocks(b)) => b
                        .into_iter()
                        .find(|b| b.kind == "text")
                        .and_then(|b| b.text),
                    _ => None,
                };
                self.prompt = text.and_then(|t| prompt_text(&t));
            }
            "assistant" => self.add_response(line, main),
            _ => {}
        }
    }

    fn add_response(&mut self, line: Line, main: bool) {
        let Some(ts) = line.timestamp.as_deref().and_then(parse_ts) else {
            return;
        };
        let Some(msg) = line.message else {
            return;
        };
        self.times.push(ts);
        if main {
            if self.entry.is_none() {
                self.entry = line.entrypoint.clone();
            }
            if let Some(e) = line.effort.filter(|e| !e.is_empty()) {
                *self.effort.entry(e).or_default() += 1;
            }
            if let Some(Content::Blocks(blocks)) = &msg.content {
                for b in blocks.iter().filter(|b| b.kind == "tool_use") {
                    if let Some(n) = &b.name {
                        *self.tools.entry(n.clone()).or_default() += 1;
                    }
                }
            }
        }
        let model = msg.model.unwrap_or_default();
        let Some(usage) = msg.usage else {
            return;
        };
        let tokens = usage.tokens();
        if model == "<synthetic>" || tokens.is_empty() {
            return;
        }
        let r = Response {
            ts,
            model,
            main,
            tokens,
        };
        match msg.id.or(line.request_id) {
            Some(id) => {
                self.responses
                    .entry(id)
                    .and_modify(|old| old.tokens = old.tokens.max(&r.tokens))
                    .or_insert(r);
            }
            None => self.loose.push(r),
        }
    }

    fn finish(mut self, session: &str) -> Option<SessionStats> {
        let responses: Vec<Response> = self.responses.into_values().chain(self.loose).collect();
        if responses.is_empty() {
            return None;
        }
        self.times.sort_unstable();
        let work_secs = self
            .times
            .windows(2)
            .map(|w| w[1] - w[0])
            .filter(|g| *g <= GAP_SECS)
            .sum();
        let mut tokens: BTreeMap<String, Tokens> = BTreeMap::new();
        let mut buckets: BTreeMap<i64, f64> = BTreeMap::new();
        let mut main_model: HashMap<String, f64> = HashMap::new();
        let (mut ku, mut main_ku) = (0.0, 0.0);
        for r in &responses {
            let k = r.tokens.ku();
            ku += k;
            tokens.entry(r.model.clone()).or_default().add(&r.tokens);
            *buckets
                .entry(r.ts - r.ts.rem_euclid(BUCKET_SECS))
                .or_default() += k;
            if r.main {
                main_ku += k;
                *main_model.entry(r.model.clone()).or_default() += k;
            }
        }
        let peak30_ku = peak(&buckets, 30 * 60);
        let effort = self
            .effort
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
            .map(|(e, _)| e);
        let model = main_model
            .into_iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(m, _)| m);
        Some(SessionStats {
            v: VERSION,
            session: session.to_string(),
            repo: String::new(),
            cwd: self.cwd.unwrap_or_default(),
            branch: self.branch,
            start: *self.times.first()?,
            end: *self.times.last()?,
            work_secs,
            entry: self.entry,
            effort,
            model,
            prompt: self.prompt,
            title: self.title,
            tokens,
            ku: round1(ku),
            main_ku: round1(main_ku),
            agents: 0,
            workflows: 0,
            workflow_agents: 0,
            tools: self.tools,
            peak30_ku: round1(peak30_ku),
            buckets: buckets.into_iter().map(|(t, k)| (t, round1(k))).collect(),
            agp: None,
            src_bytes: 0,
            src_mtime: 0,
        })
    }
}

/// The largest kU sum over `span` seconds of buckets.
fn peak(buckets: &BTreeMap<i64, f64>, span: i64) -> f64 {
    let v: Vec<(i64, f64)> = buckets.iter().map(|(t, k)| (*t, *k)).collect();
    let (mut best, mut sum, mut j) = (0.0_f64, 0.0, 0);
    for i in 0..v.len() {
        sum += v[i].1;
        while v[j].0 <= v[i].0 - span {
            sum -= v[j].1;
            j += 1;
        }
        best = best.max(sum);
    }
    best
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// The prompt to keep: the first line of real text, whitespace collapsed,
/// at most [`PROMPT_CHARS`] characters. `None` for command output and
/// reminders.
fn prompt_text(t: &str) -> Option<String> {
    let t = t.trim();
    if t.is_empty() || NOT_A_PROMPT.iter().any(|p| t.starts_with(p)) {
        return None;
    }
    let one: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(one.chars().take(PROMPT_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(id: &str, ts: &str, out: u64, block: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","cwd":"/r","gitBranch":"main","entrypoint":"cli","effort":"xhigh","requestId":"q{id}","message":{{"id":"{id}","model":"claude-opus-5-5","usage":{{"input_tokens":1000,"cache_creation_input_tokens":0,"cache_read_input_tokens":10000,"output_tokens":{out}}},"content":[{block}]}}}}"#
        )
    }

    fn write(path: &Path, lines: &[String]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn counts_each_response_once_and_reads_agents() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("s1.jsonl");
        let tool = r#"{"type":"tool_use","name":"Workflow","input":{"script":"x"}}"#;
        write(
            &main,
            &[
                r#"{"type":"user","isMeta":true,"message":{"content":"Caveat: ignore"}}"#.into(),
                r#"{"type":"user","message":{"content":"<command-name>/effort</command-name>"}}"#
                    .into(),
                r#"{"type":"user","message":{"content":[{"type":"text","text":"  fix the\n bug  "}]}}"#
                    .into(),
                // Two lines of one response repeat its usage.
                assistant("m1", "2026-11-02T09:00:00Z", 200, r#"{"type":"thinking"}"#),
                assistant("m1", "2026-11-02T09:00:01Z", 200, tool),
                // 10 minutes later: a gap that is not working time.
                assistant("m2", "2026-11-02T09:10:01Z", 100, r#"{"type":"text","text":"ok"}"#),
                r#"{"type":"ai-title","aiTitle":"Fix the bug"}"#.into(),
                "not json".into(),
            ],
        );
        let sub = dir.path().join("s1/subagents");
        write(
            &sub.join("agent-a.jsonl"),
            &[assistant(
                "a1",
                "2026-11-02T09:01:00Z",
                0,
                r#"{"type":"text"}"#,
            )],
        );
        write(
            &sub.join("workflows/wf_1/agent-b.jsonl"),
            &[assistant(
                "b1",
                "2026-11-02T09:02:00Z",
                0,
                r#"{"type":"text"}"#,
            )],
        );
        write(
            &sub.join("workflows/wf_1/agent-c.jsonl"),
            &[assistant(
                "c1",
                "2026-11-02T09:03:00Z",
                0,
                r#"{"type":"text"}"#,
            )],
        );
        let files = session_files(&main);
        assert_eq!(files.agents.len(), 1);
        assert_eq!(files.workflows["wf_1"].len(), 2);
        let s = summarize("s1", &files).unwrap();
        // 5 responses: 1000 input + 10000 cache read (= 2 kU) each, plus output x5.
        assert_eq!(s.ku, 5.0 * 2.0 + (200.0 + 100.0) * 5.0 / 1000.0);
        assert_eq!(s.main_ku, 2.0 * 2.0 + 1.5);
        assert_eq!(s.tokens["claude-opus-5-5"].output, 300);
        assert_eq!(s.prompt.as_deref(), Some("fix the bug"));
        assert_eq!(s.title.as_deref(), Some("Fix the bug"));
        assert_eq!(s.effort.as_deref(), Some("xhigh"));
        assert_eq!(s.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(s.entry.as_deref(), Some("cli"));
        assert_eq!((s.cwd.as_str(), s.branch.as_deref()), ("/r", Some("main")));
        assert_eq!(s.tools.get("Workflow"), Some(&1));
        assert_eq!((s.agents, s.workflows, s.workflow_agents), (1, 1, 2));
        // 09:00:00 → 09:03:00 counts; the 7-minute gap to 09:10:01 doesn't.
        assert_eq!(s.work_secs, 180);
        assert_eq!(s.buckets.len(), 2);
        assert_eq!(s.peak30_ku, s.ku);
        assert!(s.src_bytes > 0 && s.src_mtime > 0);
    }

    #[test]
    fn no_responses_no_stats() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("s.jsonl");
        write(
            &main,
            &[r#"{"type":"user","message":{"content":"hi"}}"#.into()],
        );
        assert!(summarize("s", &session_files(&main)).is_none());
    }

    #[test]
    fn peak_window() {
        let b: BTreeMap<i64, f64> = [(0, 1.0), (300, 2.0), (1800, 4.0), (2100, 1.0)]
            .into_iter()
            .collect();
        // [300, 2100) holds 2 + 4 and [1800, 3600) 4 + 1; [0, 1800) only 3.
        assert_eq!(peak(&b, 1800), 6.0);
    }
}
