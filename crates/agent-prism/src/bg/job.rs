//! Claude Code's background jobs as agent-prism sees them (DESIGN §11d):
//! `<config dir>/jobs/<short>/state.json`, the launch output, the list from
//! `claude agents --json --all`, a limit's text, and the resume lines.

use std::path::{Path, PathBuf};

use prism_core::model::LimitKind;
use serde_json::Value;

/// What `needs` starts with when a rate limit blocked the job (Claude Code
/// 2.1.316: `"rate_limit": {state: "blocked", needs: "rate limited — wait
/// and retry"}`).
const RATE_LIMITED: &str = "rate limited";

/// The fields of `state.json` agent-prism uses. Parsed by hand so that a
/// field of an unexpected type drops only that field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobState {
    /// `working`, `blocked`, `done`, …
    pub state: String,
    pub detail: Option<String>,
    pub needs: Option<String>,
    pub session_id: Option<String>,
    /// The folder the session was launched in (not its worktree).
    pub cwd: Option<String>,
    pub name: Option<String>,
    /// The launch flags Claude saved (`--name x --settings … …`).
    pub respawn_flags: Vec<String>,
    /// ISO time of the last change.
    pub updated_at: Option<String>,
    pub created_at: Option<String>,
}

impl JobState {
    pub fn parse(text: &str) -> Option<JobState> {
        let v: Value = serde_json::from_str(text).ok()?;
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(JobState {
            state: s("state").unwrap_or_default(),
            detail: s("detail"),
            needs: s("needs"),
            session_id: s("sessionId"),
            cwd: s("cwd"),
            name: s("name"),
            respawn_flags: v
                .get("respawnFlags")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            updated_at: s("updatedAt"),
            created_at: s("createdAt"),
        })
    }

    /// Blocked by a rate limit. The process stays alive and idle, and
    /// Claude Code does not retry by itself.
    pub fn rate_limited(&self) -> bool {
        self.state == "blocked"
            && self
                .needs
                .as_deref()
                .is_some_and(|n| n.trim_start().starts_with(RATE_LIMITED))
    }

    /// What identifies one limit: each is handled once.
    pub fn limit_key(&self) -> String {
        self.updated_at.clone().unwrap_or_default()
    }

    /// The limit as Claude Code words it, e.g. "You've hit your 5-hour
    /// limit · resets 6:10am (Europe/Madrid)".
    pub fn limit_text(&self) -> String {
        self.detail
            .clone()
            .or_else(|| self.needs.clone())
            .unwrap_or_default()
    }
}

/// `<config dir>/jobs/<short>`.
pub fn job_dir(config_dir: &Path, short: &str) -> PathBuf {
    config_dir.join("jobs").join(short)
}

/// `<config dir>/jobs/<short>/state.json`.
pub fn state_file(config_dir: &Path, short: &str) -> PathBuf {
    job_dir(config_dir, short).join("state.json")
}

/// When the job's state file last changed.
pub fn state_mtime(config_dir: &Path, short: &str) -> Option<std::time::SystemTime> {
    std::fs::metadata(state_file(config_dir, short))
        .and_then(|m| m.modified())
        .ok()
}

/// The job's state, if its file is there and parses.
pub fn read_state(config_dir: &Path, short: &str) -> Option<JobState> {
    let text = std::fs::read_to_string(state_file(config_dir, short)).ok()?;
    JobState::parse(&text)
}

/// The short id of a session id: its first 8 characters. A short id stays
/// as it is.
pub fn short_of(id: &str) -> String {
    id.chars().take(8).collect()
}

/// A plausible session or short id (it becomes a file name).
pub fn valid_id(id: &str) -> bool {
    (4..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `s` without CSI, OSC and other escape sequences and without `\r`.
pub fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.next() {
                // CSI: parameters and intermediates up to a final byte.
                Some('[') => {
                    for c in it.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC, DCS, APC, PM, SOS: up to BEL or ESC \.
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(c) = it.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && it.peek() == Some(&'\\') {
                            it.next();
                            break;
                        }
                    }
                }
                // A two-byte sequence (charset selection takes one more).
                Some('(' | ')' | '*' | '+') => {
                    it.next();
                }
                _ => {}
            },
            '\r' | '\x07' => {}
            c => out.push(c),
        }
    }
    out
}

/// What a `claude --bg` line printed: `backgrounded · <short> · <name>`, or
/// for a wake `woke session <short> with its saved options`.
#[derive(Clone, Debug, PartialEq)]
pub struct Launched {
    pub short: String,
    pub name: Option<String>,
}

pub fn parse_launch_output(text: &str) -> Option<Launched> {
    for line in strip_escapes(text).lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("backgrounded") {
            let mut parts = rest.split('·').map(str::trim).skip(1);
            let short = parts.next().filter(|s| valid_id(s))?.to_string();
            let name: Vec<&str> = parts.collect();
            let name = Some(name.join(" · ")).filter(|n| !n.is_empty());
            return Some(Launched { short, name });
        }
        if let Some(i) = line.find("woke session ") {
            let short = line[i + "woke session ".len()..]
                .split_whitespace()
                .next()
                .filter(|s| valid_id(s))?;
            return Some(Launched {
                short: short.to_string(),
                name: None,
            });
        }
    }
    None
}

/// One session from `claude agents --json --all`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Agent {
    pub id: String,
    pub session_id: Option<String>,
    pub name: Option<String>,
    pub state: Option<String>,
    /// Absent when the process is gone.
    pub pid: Option<u32>,
}

impl Agent {
    fn from_value(v: &Value) -> Option<Agent> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Agent {
            id: s("id")?,
            session_id: s("sessionId"),
            name: s("name"),
            state: s("state"),
            pid: v
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|p| u32::try_from(p).ok())
                .filter(|p| *p > 0),
        })
    }

    /// The session `sid` (short id `short`).
    pub fn is(&self, sid: &str, short: &str) -> bool {
        self.session_id.as_deref() == Some(sid) || self.id == short
    }
}

/// The sessions in `claude agents --json --all` output: a JSON array, an
/// object holding one, or one object per line.
pub fn parse_agents(text: &str) -> Vec<Agent> {
    let text = strip_escapes(text);
    let list = |v: &Value| -> Vec<Agent> {
        let arr = match v {
            Value::Array(a) => Some(a),
            Value::Object(o) => o.values().find_map(Value::as_array),
            _ => None,
        };
        arr.map(|a| a.iter().filter_map(Agent::from_value).collect())
            .unwrap_or_default()
    };
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        return match &v {
            Value::Object(_) if v.get("id").is_some() => {
                Agent::from_value(&v).into_iter().collect()
            }
            _ => list(&v),
        };
    }
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter_map(|v| Agent::from_value(&v))
        .collect()
}

/// The args of a resume: with `flags` the form for a profile that has no
/// job for the session (`--bg --resume <id> <flags…> <prompt>`), without
/// them the wake of the job a profile already has (`--bg --resume <id>
/// <prompt>`). Flags in a profile that has the job would start a copy.
pub fn resume_args(session_id: &str, flags: Option<&[String]>, prompt: &str) -> Vec<String> {
    let mut a = vec!["--bg".into(), "--resume".into(), session_id.to_string()];
    if let Some(f) = flags {
        a.extend(strip_bg(f));
    }
    a.push(prompt.to_string());
    a
}

/// `flags` without `--bg`, `--resume <id>`, `--continue` and `--session-id
/// <id>`: the resume adds its own.
fn strip_bg(flags: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = flags.iter();
    while let Some(f) = it.next() {
        match f.as_str() {
            "--bg" | "-c" | "--continue" | "--fork-session" => {}
            "-r" | "--resume" | "--session-id" => {
                it.next();
            }
            _ if f.starts_with("--resume=") || f.starts_with("--session-id=") => {}
            _ => out.push(f.clone()),
        }
    }
    out
}

/// `flags` with every `--settings <v>` removed, and `settings` added.
pub fn replace_settings(flags: &[String], settings: Option<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = flags.iter();
    while let Some(f) = it.next() {
        if f == "--settings" {
            it.next();
        } else if !f.starts_with("--settings=") {
            out.push(f.clone());
        }
    }
    if let Some(s) = settings {
        out.extend(["--settings".to_string(), s]);
    }
    out
}

/// Unix seconds of an ISO-8601 time.
pub fn iso_to_unix(s: &str) -> Option<i64> {
    s.parse::<jiff::Timestamp>().ok().map(|t| t.as_second())
}

/// The window and the reset time in a limit's text, e.g. "You've hit your
/// 5-hour limit · resets 6:10am (Europe/Madrid)". `since` is when the limit
/// hit; the reset is the next such time after it.
pub fn parse_limit(text: &str, since: i64) -> (Option<LimitKind>, Option<i64>) {
    let low = text.to_lowercase();
    let kind = if low.contains("5-hour") || low.contains("5 hour") || low.contains("session limit")
    {
        Some(LimitKind::FiveHour)
    } else if low.contains("weekly") || low.contains("7-day") {
        Some(LimitKind::SevenDay)
    } else {
        None
    };
    (kind, parse_reset(text, since))
}

/// "resets 6:10am (Europe/Madrid)", "resets 6am (UTC)", "resets Nov 3,
/// 6am (Europe/Madrid)" → the first such time after `since`.
fn parse_reset(text: &str, since: i64) -> Option<i64> {
    let at = text.find("resets ")? + "resets ".len();
    let rest = &text[at..];
    let open = rest.find('(')?;
    let close = rest[open..].find(')')? + open;
    let tz = jiff::tz::TimeZone::get(rest[open + 1..close].trim()).ok()?;
    let when = rest[..open].trim().trim_end_matches(',');
    let mut words: Vec<&str> = when
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty() && *w != "at")
        .collect();
    let (hour, minute) = parse_clock(words.pop()?)?;
    let base = jiff::Timestamp::from_second(since)
        .ok()?
        .to_zoned(tz.clone());
    let date = match words.as_slice() {
        [] => base.date(),
        [month, day] => {
            let m = month_number(month)?;
            let d: i8 = day.parse().ok()?;
            jiff::civil::Date::new(base.year(), m, d).ok()?
        }
        _ => return None,
    };
    let at = |d: jiff::civil::Date| -> Option<i64> {
        let t = d.at(hour, minute, 0, 0).to_zoned(tz.clone()).ok()?;
        Some(t.timestamp().as_second())
    };
    let mut t = at(date)?;
    if t <= since {
        // A time alone is the next one; a date in the past is next year's.
        let next = if words.is_empty() {
            date.tomorrow().ok()?
        } else {
            date.checked_add(jiff::Span::new().years(1)).ok()?
        };
        t = at(next)?;
    }
    Some(t)
}

/// "6:10am" → (6, 10); "12pm" → (12, 0); "18:30" → (18, 30).
fn parse_clock(w: &str) -> Option<(i8, i8)> {
    let w = w.to_lowercase();
    let (num, pm) = if let Some(n) = w.strip_suffix("am") {
        (n, Some(false))
    } else if let Some(n) = w.strip_suffix("pm") {
        (n, Some(true))
    } else {
        (w.as_str(), None)
    };
    let (h, m) = match num.split_once(':') {
        Some((h, m)) => (h.parse::<i8>().ok()?, m.parse::<i8>().ok()?),
        None => (num.parse::<i8>().ok()?, 0),
    };
    let h = match pm {
        Some(true) if h < 12 => h + 12,
        Some(false) if h == 12 => 0,
        _ => h,
    };
    ((0..24).contains(&h) && (0..60).contains(&m)).then_some((h, m))
}

fn month_number(m: &str) -> Option<i8> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let m = m.to_lowercase();
    let i = MONTHS.iter().position(|x| m.starts_with(x))?;
    Some(i as i8 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: &str = "You've hit your 5-hour limit · resets 6:10am (Europe/Madrid)";

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn sample(state: &str, needs: Option<&str>) -> String {
        serde_json::json!({
            "state": state,
            "detail": LIMIT,
            "needs": needs,
            "sessionId": "f1de9de1-2222-4333-8444-555566667777",
            "cwd": "/w/repo",
            "worktreePath": "/w/repo/.claude/worktrees/x",
            "respawnFlags": ["--name", "x", "--effort", "high", "--settings", "/t/s.json"],
            "providerEnv": {},
            "updatedAt": "2026-11-01T04:12:30.123Z",
            "createdAt": "2026-11-01T01:00:00.000Z"
        })
        .to_string()
    }

    #[test]
    fn limit_detection() {
        let needs = format!("rate limited — wait and retry · {LIMIT}");
        let j = JobState::parse(&sample("blocked", Some(&needs))).unwrap();
        assert!(j.rate_limited());
        assert_eq!(j.limit_text(), LIMIT);
        assert_eq!(j.limit_key(), "2026-11-01T04:12:30.123Z");
        assert_eq!(j.cwd.as_deref(), Some("/w/repo"));
        assert_eq!(j.respawn_flags.len(), 6);
        assert_eq!(
            j.session_id.as_deref(),
            Some("f1de9de1-2222-4333-8444-555566667777")
        );
        // Blocked on something else, or working.
        let j = JobState::parse(&sample("blocked", Some("permission to run Bash"))).unwrap();
        assert!(!j.rate_limited());
        let j = JobState::parse(&sample("working", Some(&needs))).unwrap();
        assert!(!j.rate_limited());
        let j = JobState::parse(&sample("blocked", None)).unwrap();
        assert!(!j.rate_limited());
        assert!(JobState::parse("not json").is_none());
        // A field of another type drops only that field.
        let j = JobState::parse(r#"{"state":"done","respawnFlags":"x","needs":3}"#).unwrap();
        assert_eq!(j.state, "done");
        assert!(j.respawn_flags.is_empty() && j.needs.is_none());
    }

    #[test]
    fn limit_text() {
        // 2026-11-01 03:00 UTC = 04:00 in Madrid (CET).
        let since = iso_to_unix("2026-11-01T03:00:00Z").unwrap();
        let (kind, reset) = parse_limit(LIMIT, since);
        assert_eq!(kind, Some(LimitKind::FiveHour));
        assert_eq!(reset, iso_to_unix("2026-11-01T05:10:00Z"));
        // Past that time of day: the next day.
        let late = iso_to_unix("2026-11-01T06:00:00Z").unwrap();
        assert_eq!(
            parse_limit(LIMIT, late).1,
            iso_to_unix("2026-11-02T05:10:00Z")
        );
        let weekly = "You've hit your weekly limit · resets Nov 3, 6pm (UTC)";
        let (kind, reset) = parse_limit(weekly, since);
        assert_eq!(kind, Some(LimitKind::SevenDay));
        assert_eq!(reset, iso_to_unix("2026-11-03T18:00:00Z"));
        assert_eq!(
            parse_limit("resets 12am (UTC)", since).1,
            iso_to_unix("2026-11-02T00:00:00Z")
        );
        assert_eq!(parse_limit("Opus limit reached", since), (None, None));
        assert_eq!(parse_limit("resets soon (Nowhere/Zone)", since).1, None);
    }

    #[test]
    fn launch_output() {
        let out = "\x1b[2K\x1b[1mbackgrounded\x1b[0m · \x1b]8;;x\x07f1de9de1\x1b]8;;\x07 · fix the bug\r\n\
                   note: the session runs in the background\n  claude agents   list sessions\n";
        assert_eq!(
            parse_launch_output(out),
            Some(Launched {
                short: "f1de9de1".into(),
                name: Some("fix the bug".into())
            })
        );
        assert_eq!(
            parse_launch_output("\x1b[32mwoke session ab12cd34\x1b[0m with its saved options\n"),
            Some(Launched {
                short: "ab12cd34".into(),
                name: None
            })
        );
        assert_eq!(
            parse_launch_output("backgrounded · 0123abcd\n")
                .unwrap()
                .name,
            None
        );
        assert_eq!(parse_launch_output("error: no such flag\n"), None);
        assert_eq!(parse_launch_output("backgrounded · ../x · n"), None);
        assert_eq!(strip_escapes("a\x1b[1;31mb\x1b(Bc\x1b]0;t\x1b\\d"), "abcd");
    }

    #[test]
    fn agents_list() {
        let arr = r#"[{"id":"f1de9de1","sessionId":"f1de9de1-x","name":"n","cwd":"/w","kind":"bg","state":"blocked","status":"idle","pid":4242},
                      {"id":"0000aaaa","sessionId":"0000aaaa-y","state":"done","status":"idle"}]"#;
        let a = parse_agents(arr);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].pid, Some(4242));
        assert!(a[0].is("f1de9de1-x", "zzzz"));
        assert!(a[1].is("other", "0000aaaa"));
        assert_eq!(a[1].pid, None);
        assert_eq!(parse_agents(&format!(r#"{{"agents":{arr}}}"#)).len(), 2);
        let lines = "{\"id\":\"a1b2c3d4\",\"pid\":7}\n{\"id\":\"b1b2c3d4\"}\n";
        assert_eq!(parse_agents(lines)[0].pid, Some(7));
        assert!(parse_agents("No sessions").is_empty());
    }

    #[test]
    fn resume_forms() {
        let flags = v(&[
            "--name",
            "x",
            "--bg",
            "--resume",
            "old",
            "--settings",
            "/t/s.json",
        ]);
        // A profile without the job: the saved flags go along.
        assert_eq!(
            resume_args("sid", Some(&flags), "go on"),
            v(&[
                "--bg",
                "--resume",
                "sid",
                "--name",
                "x",
                "--settings",
                "/t/s.json",
                "go on"
            ])
        );
        // A profile with the job: no flags, or Claude starts a copy.
        assert_eq!(
            resume_args("sid", None, "go on"),
            v(&["--bg", "--resume", "sid", "go on"])
        );
        assert_eq!(
            replace_settings(&flags, Some("{}".into())),
            v(&["--name", "x", "--bg", "--resume", "old", "--settings", "{}"])
        );
        assert_eq!(
            replace_settings(&v(&["--settings=/a", "-x"]), None),
            v(&["-x"])
        );
    }

    #[test]
    fn ids() {
        assert_eq!(short_of("f1de9de1-2222-4333"), "f1de9de1");
        assert_eq!(short_of("f1de9de1"), "f1de9de1");
        assert!(valid_id("f1de9de1"));
        assert!(!valid_id("../etc"));
        assert!(!valid_id("abc"));
    }
}
