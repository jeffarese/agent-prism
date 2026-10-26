//! Token records from Claude transcripts and Codex rollout files, for the
//! sessions that the run journal names only. Files are parsed
//! incrementally (they only grow) and cached by path.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use prism_core::model::Tool;
use serde::Deserialize;
use serde_json::Value;

use super::activity::{TokenRec, Tokens};

/// How long a lookup of a session's files is reused.
const RESOLVE_TTL: Duration = Duration::from_secs(60);
/// How long the index of Codex rollout files is reused.
const CODEX_INDEX_TTL: Duration = Duration::from_secs(300);
/// A file read stops after this many new bytes per scan.
const MAX_READ: u64 = 256 * 1024 * 1024;

/// What one file has given so far.
#[derive(Default)]
struct FileScan {
    len: u64,
    mtime: Option<SystemTime>,
    /// Bytes parsed (up to the last full line).
    offset: u64,
    /// Claude: message id → index in `recs` (streamed chunks repeat it).
    ids: HashMap<String, usize>,
    recs: Vec<TokenRec>,
    /// Codex: the last `total_token_usage.total_tokens` and model seen.
    codex_total: u64,
    codex_model: String,
}

/// A session's files, by tool and session id, with when they were found.
type Resolved = HashMap<(Tool, String), (Instant, Vec<PathBuf>)>;

/// Finds and parses the token records of sessions.
pub struct Scanner {
    /// `<claude common home>/projects`.
    claude_projects: PathBuf,
    /// `<codex common home>/sessions`.
    codex_sessions: PathBuf,
    files: Mutex<HashMap<PathBuf, FileScan>>,
    resolved: Mutex<Resolved>,
    codex_index: Mutex<Option<(Instant, HashMap<String, PathBuf>)>>,
}

impl Scanner {
    pub fn new(claude_home: &Path, codex_home: &Path) -> Self {
        Self {
            claude_projects: claude_home.join("projects"),
            codex_sessions: codex_home.join("sessions"),
            files: Mutex::new(HashMap::new()),
            resolved: Mutex::new(HashMap::new()),
            codex_index: Mutex::new(None),
        }
    }

    /// Every token record of `session` (a Claude session id or a Codex
    /// thread id), oldest first.
    pub fn records(&self, tool: Tool, session: &str, cwd: Option<&str>) -> Vec<TokenRec> {
        let mut out = Vec::new();
        for path in self.resolve(tool, session, cwd) {
            out.extend(self.scan(tool, &path));
        }
        out.sort_by_key(|r| r.ts);
        out
    }

    fn resolve(&self, tool: Tool, session: &str, cwd: Option<&str>) -> Vec<PathBuf> {
        // A session id names a file: never let it walk out of the dir.
        if session.is_empty()
            || !session
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Vec::new();
        }
        let key = (tool, session.to_string());
        if let Some((at, v)) = lock(&self.resolved).get(&key)
            && at.elapsed() < RESOLVE_TTL
        {
            return v.clone();
        }
        let found = match tool {
            Tool::Claude => self.claude_files(session, cwd),
            Tool::Codex => self.codex_file(session).into_iter().collect(),
        };
        lock(&self.resolved).insert(key, (Instant::now(), found.clone()));
        found
    }

    /// `projects/<slug>/<sid>.jsonl` plus its subagent transcripts. The
    /// slug is the work dir with every non-alphanumeric char as `-`; when
    /// that dir has no such file, every project dir is checked.
    fn claude_files(&self, sid: &str, cwd: Option<&str>) -> Vec<PathBuf> {
        let name = format!("{sid}.jsonl");
        let mut dir = cwd
            .map(|c| {
                let slug: String = c
                    .chars()
                    .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
                    .collect();
                self.claude_projects.join(slug)
            })
            .filter(|d| d.join(&name).is_file());
        if dir.is_none() {
            dir = std::fs::read_dir(&self.claude_projects)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .find(|d| d.join(&name).is_file());
        }
        let Some(dir) = dir else {
            return Vec::new();
        };
        let mut out = vec![dir.join(&name)];
        if let Ok(rd) = std::fs::read_dir(dir.join(sid).join("subagents")) {
            let mut subs: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
                .collect();
            subs.sort();
            out.extend(subs);
        }
        out
    }

    /// `sessions/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`, from an index
    /// of every rollout file (rebuilt at most every 5 min).
    fn codex_file(&self, id: &str) -> Option<PathBuf> {
        let mut idx = lock(&self.codex_index);
        let stale = idx
            .as_ref()
            .is_none_or(|(at, m)| at.elapsed() >= CODEX_INDEX_TTL && !m.contains_key(id));
        if stale {
            *idx = Some((Instant::now(), index_rollouts(&self.codex_sessions)));
        }
        idx.as_ref().and_then(|(_, m)| m.get(id).cloned())
    }

    /// The records of one file, reading only what was appended since the
    /// last scan.
    fn scan(&self, tool: Tool, path: &Path) -> Vec<TokenRec> {
        let Ok(meta) = std::fs::metadata(path) else {
            return Vec::new();
        };
        let (len, mtime) = (meta.len(), meta.modified().ok());
        let mut files = lock(&self.files);
        let fs = files.entry(path.to_path_buf()).or_default();
        if len < fs.offset || (len == fs.len && mtime != fs.mtime && len == fs.offset) {
            // Replaced or rewritten: start over.
            *fs = FileScan::default();
        }
        if len > fs.offset
            && let Ok(mut f) = File::open(path)
            && f.seek(SeekFrom::Start(fs.offset)).is_ok()
        {
            let mut buf = Vec::new();
            let _ = f.take(MAX_READ).read_to_end(&mut buf);
            let end = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            for line in buf[..end].split(|b| *b == b'\n') {
                match tool {
                    Tool::Claude => claude_line(fs, line),
                    Tool::Codex => codex_line(fs, line),
                }
            }
            fs.offset += end as u64;
        }
        fs.len = len;
        fs.mtime = mtime;
        fs.recs.clone()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn index_rollouts(root: &Path) -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    let dirs = |p: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect()
    };
    for y in dirs(root) {
        for m in dirs(&y) {
            for d in dirs(&m) {
                for f in dirs(&d) {
                    if let Some(id) = rollout_thread_id(&f) {
                        out.insert(id, f);
                    }
                }
            }
        }
    }
    out
}

/// The thread id at the end of `rollout-<time>-<uuid>.jsonl`.
fn rollout_thread_id(p: &Path) -> Option<String> {
    let name = p.file_name()?.to_str()?;
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    (id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')).then(|| id.into())
}

fn parse_ts(s: &str) -> Option<i64> {
    s.parse::<jiff::Timestamp>().ok().map(|t| t.as_second())
}

#[derive(Deserialize)]
struct ClaudeLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    message: Option<ClaudeMsg>,
}

#[derive(Deserialize)]
struct ClaudeMsg {
    id: Option<String>,
    model: Option<String>,
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

/// An assistant line with usage. Streamed chunks of one message repeat its
/// id; the last one wins.
fn claude_line(fs: &mut FileScan, line: &[u8]) {
    // Cheap filter before parsing: most lines are not responses.
    if !contains(line, b"\"assistant\"") || !contains(line, b"\"usage\"") {
        return;
    }
    let Ok(l) = serde_json::from_slice::<ClaudeLine>(line) else {
        return;
    };
    let (Some("assistant"), Some(ts), Some(msg)) = (l.kind.as_deref(), l.timestamp, l.message)
    else {
        return;
    };
    let (Some(u), Some(ts)) = (msg.usage, parse_ts(&ts)) else {
        return;
    };
    let model = msg.model.unwrap_or_default();
    if model.is_empty() || model == "<synthetic>" {
        return;
    }
    let rec = TokenRec {
        ts,
        model,
        tokens: Tokens {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_write: u.cache_creation_input_tokens,
            cache_read: u.cache_read_input_tokens,
        },
    };
    match msg.id {
        Some(id) => match fs.ids.get(&id) {
            Some(&i) => fs.recs[i] = rec,
            None => {
                fs.ids.insert(id, fs.recs.len());
                fs.recs.push(rec);
            }
        },
        None => fs.recs.push(rec),
    }
}

/// `turn_context` sets the model; a `token_count` event whose running
/// total grew adds its `last_token_usage`.
fn codex_line(fs: &mut FileScan, line: &[u8]) {
    let ctx = contains(line, b"\"turn_context\"");
    if !ctx && !contains(line, b"\"token_count\"") {
        return;
    }
    let Ok(v) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    let p = &v["payload"];
    if v["type"] == "turn_context" {
        if let Some(m) = p["model"].as_str() {
            fs.codex_model = m.to_string();
        }
        return;
    }
    if p["type"] != "token_count" {
        return;
    }
    let info = &p["info"];
    let total = info["total_token_usage"]["total_tokens"]
        .as_u64()
        .unwrap_or(0);
    if total <= fs.codex_total {
        return;
    }
    fs.codex_total = total;
    let Some(ts) = v["timestamp"].as_str().and_then(parse_ts) else {
        return;
    };
    let last = &info["last_token_usage"];
    let n = |k: &str| last[k].as_u64().unwrap_or(0);
    let cached = n("cached_input_tokens");
    fs.recs.push(TokenRec {
        ts,
        model: if fs.codex_model.is_empty() {
            "codex".into()
        } else {
            fs.codex_model.clone()
        },
        tokens: Tokens {
            input: n("input_tokens").saturating_sub(cached),
            output: n("output_tokens"),
            cache_write: n("cache_write_input_tokens"),
            cache_read: cached,
        },
    });
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}
