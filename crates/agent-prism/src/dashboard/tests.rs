use std::collections::HashMap;
use std::path::Path;

use prism_core::journal::{Event, EventKind, RunMode};
use prism_core::model::Tool;
use prism_core::paths::Paths;
use prism_usage::cache::{Sample, UsageCache};

use super::activity::{self, TokenRec, Tokens};
use super::*;

fn ev(ts: i64, run: &str, kind: EventKind, profile: &str) -> Event {
    Event {
        ts,
        run: run.into(),
        ev: Some(kind),
        tool: Tool::Claude,
        mode: RunMode::Tui,
        profile: profile.into(),
        ..Default::default()
    }
}

fn utc(ts: i64) -> (usize, usize, i64) {
    let days = ts.div_euclid(86_400);
    // 1970-01-01 was a Thursday (Monday = 0 → 3).
    let wd = (days + 3).rem_euclid(7) as usize;
    let s = ts.rem_euclid(86_400);
    (wd, (s / 3600) as usize, s % 3600)
}

fn tok(output: u64) -> Tokens {
    Tokens {
        input: 1,
        output,
        cache_write: 0,
        cache_read: 10,
    }
}

/// A run that starts on a, hits a limit, switches to b and ends.
fn switch_run() -> Vec<Event> {
    let mut start = ev(1000, "1-1000", EventKind::Start, "a");
    start.session = Some("s1".into());
    start.cwd = Some("/w/proj/".into());
    start.reason = Some("most 5h headroom".into());
    let mut limit = ev(1600, "1-1000", EventKind::Limit, "a");
    limit.limit = Some("5h".into());
    let mut sw = ev(1606, "1-1000", EventKind::Switch, "b");
    sw.from = Some("a".into());
    sw.reason = Some("limit".into());
    let mut end = ev(2200, "1-1000", EventKind::End, "b");
    end.exit = Some(0);
    end.session = Some("s1".into());
    vec![end, limit, start, sw] // any order
}

#[test]
fn runs_from_events() {
    let runs = activity::build_runs(&switch_run(), &[], 5000);
    assert_eq!(runs.len(), 1);
    let r = &runs[0];
    assert!(r.ended && !r.live);
    assert_eq!((r.start, r.end, r.exit), (1000, 2200, Some(0)));
    assert_eq!(r.project, "proj");
    assert_eq!(r.sessions, vec!["s1".to_string()]);
    let segs: Vec<_> = r
        .segments
        .iter()
        .map(|s| (s.profile.as_str(), s.from, s.to, s.waiting))
        .collect();
    assert_eq!(
        segs,
        vec![("a", 1000, 1606, false), ("b", 1606, 2200, false)]
    );
    let kinds: Vec<_> = r.marks.iter().map(|m| m.ev).collect();
    assert_eq!(kinds, vec![EventKind::Limit, EventKind::Switch]);
}

#[test]
fn wait_live_and_exec_runs() {
    let mut events = vec![
        ev(100, "7-100", EventKind::Start, "a"),
        ev(200, "7-100", EventKind::Wait, "a"),
        ev(500, "7-100", EventKind::Resume, "a"),
    ];
    let mut exec = ev(300, "8-300", EventKind::Start, "b");
    exec.mode = RunMode::Exec;
    events.push(exec);
    // pid 7 is live (started_at within the tolerance), pid 8 is not.
    let runs = activity::build_runs(&events, &[(7, 101)], 900);
    let wait = runs.iter().find(|r| r.id == "7-100").unwrap();
    assert!(wait.live && !wait.ended);
    assert_eq!(wait.end, 900);
    let segs: Vec<_> = wait
        .segments
        .iter()
        .map(|s| (s.from, s.to, s.waiting))
        .collect();
    assert_eq!(
        segs,
        vec![(100, 200, false), (200, 500, true), (500, 900, false)]
    );
    let exec = runs.iter().find(|r| r.id == "8-300").unwrap();
    assert!(!exec.live && !exec.ended);
    assert_eq!(exec.end, 300, "no end and no tokens: no length");
    assert!(exec.segments.is_empty());
}

#[test]
fn tokens_follow_the_active_account() {
    let mut runs = activity::build_runs(&switch_run(), &[], 5000);
    let recs = HashMap::from([(
        "s1".to_string(),
        vec![
            TokenRec {
                ts: 1100,
                model: "opus".into(),
                tokens: tok(5),
            },
            TokenRec {
                ts: 1700,
                model: "opus".into(),
                tokens: tok(7),
            },
            TokenRec {
                ts: 1800,
                model: "haiku".into(),
                tokens: tok(1),
            },
            // Outside the run: not agent-prism's.
            TokenRec {
                ts: 9000,
                model: "opus".into(),
                tokens: tok(100),
            },
        ],
    )]);
    let got = activity::attribute(&mut runs, &recs, 10_000);
    let by: Vec<_> = got.iter().map(|(_, p, r)| (p.as_str(), r.ts)).collect();
    assert_eq!(by, vec![("a", 1100), ("b", 1700), ("b", 1800)]);
    assert_eq!(runs[0].tokens.output, 13);
    assert_eq!(
        runs[0].models,
        vec!["opus".to_string(), "haiku".to_string()]
    );
}

#[test]
fn exec_runs_end_at_their_last_token() {
    let mut a = ev(100, "8-100", EventKind::Start, "x");
    a.mode = RunMode::Exec;
    a.session = Some("s".into());
    // A later run of the same session takes the later records.
    let mut b = ev(1000, "9-1000", EventKind::Start, "y");
    b.session = Some("s".into());
    let mut b_end = ev(1500, "9-1000", EventKind::End, "y");
    b_end.exit = Some(0);
    let mut runs = activity::build_runs(&[a, b, b_end], &[], 5000);
    let recs = HashMap::from([(
        "s".to_string(),
        vec![
            TokenRec {
                ts: 150,
                model: "m".into(),
                tokens: tok(1),
            },
            TokenRec {
                ts: 400,
                model: "m".into(),
                tokens: tok(1),
            },
            TokenRec {
                ts: 1200,
                model: "m".into(),
                tokens: tok(1),
            },
        ],
    )]);
    let got = activity::attribute(&mut runs, &recs, 5000);
    let owners: Vec<_> = got.iter().map(|(id, _, r)| (id.as_str(), r.ts)).collect();
    assert_eq!(
        owners,
        vec![("8-100", 150), ("8-100", 400), ("9-1000", 1200)]
    );
    let exec = runs.iter().find(|r| r.id == "8-100").unwrap();
    assert_eq!(exec.end, 400);
    assert_eq!(exec.segments.len(), 1);
    assert_eq!((exec.segments[0].from, exec.segments[0].to), (100, 400));
}

#[test]
fn summary_clips_to_the_range() {
    let runs = activity::build_runs(&switch_run(), &[], 5000);
    let recs = HashMap::from([(
        "s1".to_string(),
        vec![
            TokenRec {
                ts: 1100,
                model: "opus".into(),
                tokens: tok(5),
            },
            TokenRec {
                ts: 1700,
                model: "opus".into(),
                tokens: tok(7),
            },
        ],
    )]);
    // The range starts inside the first segment.
    let a = activity::summarize(runs, &recs, 1500, 4000, &utc);
    assert_eq!(a.totals.runs, 1);
    assert_eq!(a.totals.secs, (1606 - 1500) + (2200 - 1606));
    assert_eq!((a.totals.limits, a.totals.switches), (1, 1));
    let acc: Vec<_> = a
        .by_account
        .iter()
        .map(|s| (s.key.as_str(), s.secs, s.runs, s.tokens.output))
        .collect();
    assert_eq!(acc, vec![("b", 594, 1, 7), ("a", 106, 1, 0)]);
    assert_eq!(
        a.totals.tokens.output, 7,
        "the record before the range is out"
    );
    assert_eq!(a.by_project[0].key, "proj");
    assert_eq!(a.by_mode[0].key, "tui");
    assert_eq!(a.events.len(), 2);
    assert_eq!(a.events[0].mark.ev, EventKind::Switch, "newest first");
    // Thursday 00:00 UTC in 1970; every active second lands in hour 0.
    assert_eq!(a.heat[3][0], 700);
    assert_eq!(a.heat.iter().flatten().sum::<i64>(), a.totals.secs);
    let bucketed: u64 = a
        .token_series
        .buckets
        .iter()
        .flat_map(|(_, by)| by.values())
        .sum();
    assert_eq!(bucketed, tok(7).total());
    // Out of the range entirely.
    let none = activity::summarize(Vec::new(), &recs, 10_000, 20_000, &utc);
    assert_eq!(none.totals.runs, 0);
}

#[test]
fn projects_fold_into_other() {
    let events: Vec<Event> = (0..12)
        .map(|i| {
            let mut e = ev(100 + i, &format!("{i}-{}", 100 + i), EventKind::Start, "a");
            e.cwd = Some(format!("/w/p{i}"));
            e
        })
        .collect();
    let runs = activity::build_runs(&events, &[], 1000);
    let a = activity::summarize(runs, &HashMap::new(), 0, 1000, &utc);
    assert_eq!(a.by_project.len(), 8);
    let other = a.by_project.last().unwrap();
    assert_eq!((other.key.as_str(), other.runs), ("Other", 5));
}

#[test]
fn heat_splits_hours() {
    let mut heat = vec![vec![0i64; 24]; 7];
    let start = ev(3000, "1-3000", EventKind::Start, "a");
    let end = {
        let mut e = ev(7200 + 300, "1-3000", EventKind::End, "a");
        e.exit = Some(0);
        e
    };
    let runs = activity::build_runs(&[start, end], &[], 10_000);
    let a = activity::summarize(runs, &HashMap::new(), 0, 10_000, &utc);
    heat[3][0] = 600;
    heat[3][1] = 3600;
    heat[3][2] = 300;
    assert_eq!(a.heat, heat);
}

#[test]
fn history_buckets_keep_peaks() {
    let s = |ts, u5h, u7d| Sample { ts, u5h, u7d };
    let samples = vec![
        s(10, Some(5.0), None),
        s(250, Some(40.0), Some(1.0)),
        s(290, Some(20.0), Some(2.0)),
        s(700, None, Some(3.0)),
        s(5000, Some(1.0), None),
    ];
    let pts = history::bucket(&samples, 0, 1000, 300);
    assert_eq!(
        pts,
        vec![
            history::Point(290, Some(40.0), Some(2.0)),
            history::Point(700, None, Some(3.0)),
        ]
    );
    assert_eq!(history::bucket_secs(86_400), 300);
    assert_eq!(history::bucket_secs(7 * 86_400), 1800);
}

#[test]
fn http_head_and_host() {
    let r = http::parse_head(
        "GET /api/history?range=24h&tool=codex HTTP/1.1\r\nHost: 127.0.0.1:4000\r\nSec-Fetch-Site: same-origin",
    )
    .unwrap();
    assert_eq!(r.method, "GET");
    assert_eq!(r.path, "/api/history");
    assert_eq!(r.query.get("range").map(String::as_str), Some("24h"));
    assert_eq!(r.header("host"), Some("127.0.0.1:4000"));
    assert!(http::parse_head("GET /\r\n").is_err());
    assert!(http::parse_head("GET / SPDY/3").is_err());
    assert!(http::host_ok(Some("127.0.0.1:4000"), 4000));
    assert!(http::host_ok(Some("LOCALHOST:4000"), 4000));
    assert!(!http::host_ok(Some("127.0.0.1:4001"), 4000));
    assert!(!http::host_ok(Some("evil.example:4000"), 4000));
    assert!(!http::host_ok(Some("127.0.0.1"), 4000));
    assert!(!http::host_ok(None, 4000));
    let mut big = b"GET / HTTP/1.1\r\nX: ".to_vec();
    big.extend(std::iter::repeat_n(b'a', 20_000));
    assert!(http::read_request(&mut big.as_slice()).is_err());
}

#[test]
fn color_slots_follow_the_account() {
    let p = |id: &str, c: Option<&str>| (id.to_string(), c.map(str::to_string));
    let s = state::color_slots(&[
        p("a", None),
        p("b", Some("cyan")),
        p("c", Some("blue")),
        p("d", None),
    ]);
    // b and c take their config colors; a and d the first free slots.
    assert_eq!(s["b"], Some(2));
    assert_eq!(s["c"], Some(0));
    assert_eq!(s["a"], Some(1));
    assert_eq!(s["d"], Some(3));
    // A taken color falls back to a free slot; past 8 accounts: none.
    let many: Vec<_> = (0..10).map(|i| p(&format!("x{i}"), Some("blue"))).collect();
    let s = state::color_slots(&many);
    assert_eq!(s["x0"], Some(0));
    assert_eq!(s["x1"], Some(1));
    assert_eq!(s["x9"], None);
}

fn write_lines(path: &Path, lines: &[serde_json::Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(path, text).unwrap();
}

fn assistant(ts: &str, id: &str, model: &str, out: u64) -> serde_json::Value {
    serde_json::json!({"type": "assistant", "timestamp": ts, "message": {"id": id, "model": model,
        "usage": {"input_tokens": 2, "output_tokens": out, "cache_creation_input_tokens": 3, "cache_read_input_tokens": 40}}})
}

#[test]
fn scanner_reads_claude_transcripts() {
    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path().join("claude");
    let proj = claude.join("projects").join("-w-my-app");
    let main = proj.join("sid-1.jsonl");
    write_lines(
        &main,
        &[
            serde_json::json!({"type": "user", "timestamp": "2026-10-25T10:00:00Z", "message": {"content": "hi"}}),
            assistant("2026-10-25T10:00:05Z", "m1", "claude-opus-5-5", 10),
            // A streamed chunk of the same message: replaces, never adds.
            assistant("2026-10-25T10:00:06Z", "m1", "claude-opus-5-5", 12),
            assistant("2026-10-25T10:00:09Z", "m2", "<synthetic>", 99),
        ],
    );
    write_lines(
        &proj.join("sid-1").join("subagents").join("agent-x.jsonl"),
        &[assistant(
            "2026-10-25T10:01:00Z",
            "m3",
            "claude-haiku-5-5",
            4,
        )],
    );
    let sc = tokens::Scanner::new(&claude, &dir.path().join("codex"));
    let recs = sc.records(Tool::Claude, "sid-1", Some("/w/my.app"));
    let outs: Vec<_> = recs
        .iter()
        .map(|r| (r.model.as_str(), r.tokens.output))
        .collect();
    assert_eq!(outs, vec![("claude-opus-5-5", 12), ("claude-haiku-5-5", 4)]);
    assert_eq!(recs[0].tokens.cache_read, 40);
    assert_eq!(recs[0].ts, 1792922406);
    // A wrong cwd still finds the file; a bad id finds nothing.
    assert_eq!(
        sc.records(Tool::Claude, "sid-1", Some("/elsewhere")).len(),
        2
    );
    assert!(sc.records(Tool::Claude, "../sid-1", None).is_empty());
    assert!(sc.records(Tool::Claude, "nope", None).is_empty());

    // Appended lines are picked up; a half-written line waits.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&main)
        .unwrap();
    use std::io::Write;
    writeln!(
        f,
        "{}",
        assistant("2026-10-25T10:02:00Z", "m4", "claude-opus-5-5", 1)
    )
    .unwrap();
    write!(f, "{{\"type\":\"assistant\",\"timest").unwrap();
    drop(f);
    let recs = sc.records(Tool::Claude, "sid-1", Some("/w/my.app"));
    assert_eq!(recs.len(), 3);
}

#[test]
fn scanner_reads_codex_rollouts() {
    let dir = tempfile::tempdir().unwrap();
    let codex = dir.path().join("codex");
    let id = "01a16a91-9baf-7b62-aab6-531cf2aea1d2";
    let count = |ts: &str, total: u64, input: u64, cached: u64, out: u64| {
        serde_json::json!({"timestamp": ts, "type": "event_msg", "payload": {"type": "token_count",
            "info": {"total_token_usage": {"total_tokens": total},
                     "last_token_usage": {"input_tokens": input, "cached_input_tokens": cached, "output_tokens": out}}}})
    };
    write_lines(
        &codex
            .join("sessions/2026/10/23")
            .join(format!("rollout-2026-10-23T23-01-29-{id}.jsonl")),
        &[
            serde_json::json!({"timestamp": "2026-10-23T21:01:30Z", "type": "turn_context", "payload": {"model": "gpt-6-astra"}}),
            count("2026-10-23T21:01:40Z", 110, 100, 60, 10),
            // The same total again: not a new response.
            count("2026-10-23T21:01:41Z", 110, 100, 60, 10),
            count("2026-10-23T21:02:00Z", 300, 180, 100, 10),
        ],
    );
    let sc = tokens::Scanner::new(&dir.path().join("claude"), &codex);
    let recs = sc.records(Tool::Codex, id, None);
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].model, "gpt-6-astra");
    assert_eq!(
        recs[0].tokens,
        Tokens {
            input: 40,
            output: 10,
            cache_write: 0,
            cache_read: 60
        }
    );
}

/// A whole dashboard over a temp state: one Claude account with fake
/// credentials in a file store, usage, samples and a journal.
fn fixture(root: &Path) -> Dash {
    let paths = Paths::under(root);
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(
        paths.config_file(),
        "[[profile]]\nid = \"work\"\nlabel = \"Work\"\ndir = \"~/.claude-work\"\n",
    )
    .unwrap();
    let dir = paths.home.join(".claude-work");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".claude.json"),
        r#"{"oauthAccount":{"displayName":"W","emailAddress":"w@example.test"}}"#,
    )
    .unwrap();
    let service = prism_core::profile::keychain_service_for(&dir, false);
    let secret = serde_json::json!({"claudeAiOauth": {"accessToken": "SECRET-ACCESS",
        "refreshToken": "SECRET-REFRESH", "expiresAt": 4_102_444_800_000i64,
        "subscriptionType": "max", "rateLimitTier": "default_claude_max_20x"}});
    let store_file = root.join("secrets.json");
    std::fs::write(
        &store_file,
        serde_json::json!({ service: secret.to_string() }).to_string(),
    )
    .unwrap();
    let now = launch::unix_now();
    let cache = UsageCache::new(paths.usage_dir());
    cache
        .store(
            "work",
            &prism_usage::cache::CachedUsage {
                snapshot: Some(prism_core::model::UsageSnapshot {
                    fetched_at: now,
                    five_hour: Some(prism_core::model::Window {
                        utilization: 40.0,
                        resets_at: Some(now + 3600),
                        inferred: false,
                    }),
                    seven_day: None,
                    scoped: Vec::new(),
                    extra_usage_enabled: false,
                    limit_reached: false,
                }),
                last_attempt: now,
                ..Default::default()
            },
        )
        .unwrap();
    cache
        .append_sample(
            "work",
            &Sample {
                ts: now - 60,
                u5h: Some(39.0),
                u7d: None,
            },
        )
        .unwrap();
    let mut start = ev(now - 600, "1-1", EventKind::Start, "work");
    start.cwd = Some("/w/app".into());
    journal::append(&paths, &start);
    let mut end = ev(now - 60, "1-1", EventKind::End, "work");
    end.exit = Some(0);
    journal::append(&paths, &end);

    let ctx = Arc::new(AppCtx {
        store: Arc::new(crate::tokens::FileStore::new(store_file)),
        cache,
        paths,
    });
    let config = ctx.load_config().unwrap();
    let profiles = config.profiles(&ctx.paths);
    Dash {
        tokens: Arc::new(KeychainTokens::new(ctx.store.clone(), profiles.clone())),
        scanner: tokens::Scanner::new(&root.join("home/.claude"), &root.join("home/.codex")),
        ctx,
        port: 4000,
        config: Mutex::new((None, config, profiles)),
        conns: AtomicUsize::new(0),
        stop: Arc::new(AtomicBool::new(false)),
    }
}

fn get(d: &Dash, target: &str, extra: &str) -> Response {
    let head = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:4000{extra}");
    route(d, &http::parse_head(&head).unwrap())
}

#[test]
fn routes_and_guards() {
    let dir = tempfile::tempdir().unwrap();
    let d = fixture(dir.path());
    let page = get(&d, "/", "");
    assert_eq!(page.status, 200);
    let mut out = Vec::new();
    page.write_to(&mut out).unwrap();
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("Content-Security-Policy: default-src 'none'"));
    assert!(text.contains("<script src=\"/app.js\""));
    assert_eq!(get(&d, "/app.js", "").status, 200);
    assert_eq!(get(&d, "/nope", "").status, 404);
    assert_eq!(get(&d, "/../etc/passwd", "").status, 404);
    // DNS rebinding, other methods, cross-site requests.
    let other = route(
        &d,
        &http::parse_head("GET / HTTP/1.1\r\nHost: attacker.example:4000").unwrap(),
    );
    assert_eq!(other.status, 403);
    let post = route(
        &d,
        &http::parse_head("POST /api/state HTTP/1.1\r\nHost: localhost:4000").unwrap(),
    );
    assert_eq!(post.status, 405);
    assert_eq!(
        get(&d, "/api/state", "\r\nSec-Fetch-Site: cross-site").status,
        403
    );
    assert_eq!(
        get(&d, "/api/state", "\r\nSec-Fetch-Site: same-origin").status,
        200
    );
}

#[test]
fn api_state_history_activity() {
    let dir = tempfile::tempdir().unwrap();
    let d = fixture(dir.path());
    let st = get(&d, "/api/state", "");
    let body = String::from_utf8(st.body.clone()).unwrap();
    assert!(!body.contains("SECRET"), "no token may reach the page");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let a = &v["accounts"][0];
    assert_eq!(a["id"], "work");
    assert_eq!(a["email"], "w@example.test");
    assert_eq!(a["tier"], "default_claude_max_20x");
    assert_eq!(a["token"]["state"], "valid");
    assert_eq!(a["five_hour"]["util"], 40.0);
    assert_eq!(a["slot"], 0);
    assert_eq!(v["picks"][0]["chosen"], "work");
    assert_eq!(v["journal"]["exists"], true);

    let h: serde_json::Value =
        serde_json::from_slice(&get(&d, "/api/history?range=24h", "").body).unwrap();
    assert_eq!(h["bucket_secs"], 300);
    assert_eq!(h["series"][0]["points"].as_array().unwrap().len(), 1);

    let a: serde_json::Value =
        serde_json::from_slice(&get(&d, "/api/activity?range=7d", "").body).unwrap();
    assert_eq!(a["totals"]["runs"], 1);
    assert_eq!(a["totals"]["secs"], 540);
    assert_eq!(a["by_project"][0]["key"], "app");
    let codex: serde_json::Value =
        serde_json::from_slice(&get(&d, "/api/activity?tool=codex", "").body).unwrap();
    assert_eq!(codex["totals"]["runs"], 0);
}
