//! Runs, distributions and token totals from the run journal and the
//! transcripts' token records. Pure functions over their inputs.

use std::collections::{BTreeMap, HashMap};

use prism_core::journal::{Event, EventKind, RunMode};
use prism_core::model::Tool;
use serde::Serialize;

/// An exec run (no end event) owns the tokens of its session for at most
/// this long after its start.
const EXEC_WINDOW_SECS: i64 = 24 * 3600;
/// Events listed, newest first.
const MAX_EVENTS: usize = 300;
/// Projects shown by name; the rest fold into "Other".
const MAX_PROJECTS: usize = 8;

/// Token counts of one or more model responses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
}

impl Tokens {
    pub fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.output += o.output;
        self.cache_write += o.cache_write;
        self.cache_read += o.cache_read;
    }

    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }
}

/// One model response from a transcript.
#[derive(Clone, Debug, PartialEq)]
pub struct TokenRec {
    pub ts: i64,
    pub model: String,
    pub tokens: Tokens,
}

/// A stretch of a run on one account.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Segment {
    pub profile: String,
    pub from: i64,
    pub to: i64,
    /// The run waited for this account to reset.
    pub waiting: bool,
}

/// A limit, wait, switch or resume of a run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Mark {
    pub ts: i64,
    pub ev: EventKind,
    pub profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One run rebuilt from its journal events.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Run {
    pub id: String,
    pub tool: Tool,
    pub mode: RunMode,
    pub start: i64,
    /// The end event's time; for a live run now; else an estimate (the
    /// last event or the last token record).
    pub end: i64,
    /// The run wrote an end event.
    pub ended: bool,
    /// The run's process is in the live-session registry.
    pub live: bool,
    /// The first account.
    pub profile: String,
    pub reason: Option<String>,
    pub cwd: Option<String>,
    /// The basename of `cwd`.
    pub project: String,
    pub sessions: Vec<String>,
    pub segments: Vec<Segment>,
    pub marks: Vec<Mark>,
    pub exit: Option<i32>,
    pub tokens: Tokens,
    pub models: Vec<String>,
}

impl Run {
    /// The account active at `ts`.
    fn profile_at(&self, ts: i64) -> &str {
        self.segments
            .iter()
            .rev()
            .find(|s| s.from <= ts)
            .or(self.segments.first())
            .map_or(self.profile.as_str(), |s| s.profile.as_str())
    }

    /// The span whose token records belong to this run; `next` is the
    /// start of the next run of the same session.
    fn token_window(&self, next: Option<i64>, now: i64) -> (i64, i64) {
        let end = if self.ended || self.live {
            self.end
        } else {
            let mut e = (self.start + EXEC_WINDOW_SECS).min(now);
            if let Some(n) = next {
                e = e.min(n);
            }
            e
        };
        (self.start, end)
    }
}

/// The basename of a work dir, for grouping by project.
pub fn project_name(cwd: Option<&str>) -> String {
    cwd.and_then(|c| {
        std::path::Path::new(c.trim_end_matches('/'))
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    })
    .unwrap_or_else(|| "(unknown)".into())
}

/// The pid in a run id `<pid>-<start ts>`.
fn run_pid(id: &str) -> Option<u32> {
    id.split_once('-')?.0.parse().ok()
}

/// Rebuilds the runs from journal events (any order). `live` holds the
/// `(pid, started_at)` of the live-session registry.
pub fn build_runs(events: &[Event], live: &[(u32, i64)], now: i64) -> Vec<Run> {
    let mut order: Vec<&str> = Vec::new();
    let mut groups: HashMap<&str, Vec<&Event>> = HashMap::new();
    for e in events
        .iter()
        .filter(|e| e.ev.is_some() && !e.run.is_empty())
    {
        groups
            .entry(e.run.as_str())
            .or_insert_with(|| {
                order.push(e.run.as_str());
                Vec::new()
            })
            .push(e);
    }
    let mut runs: Vec<Run> = order
        .into_iter()
        .map(|id| {
            let mut evs = groups.remove(id).unwrap_or_default();
            evs.sort_by_key(|e| e.ts);
            build_run(id, &evs, live, now)
        })
        .collect();
    runs.sort_by_key(|r| r.start);
    runs
}

fn build_run(id: &str, evs: &[&Event], live: &[(u32, i64)], now: i64) -> Run {
    let first = evs[0];
    let start_ev = evs.iter().find(|e| e.ev == Some(EventKind::Start));
    let start = start_ev.map_or(first.ts, |e| e.ts);
    let mut run = Run {
        id: id.to_string(),
        tool: first.tool,
        mode: first.mode,
        start,
        end: start,
        ended: false,
        live: false,
        profile: start_ev.unwrap_or(&first).profile.clone(),
        reason: start_ev.and_then(|e| e.reason.clone()),
        cwd: evs.iter().find_map(|e| e.cwd.clone()),
        project: String::new(),
        sessions: Vec::new(),
        segments: Vec::new(),
        marks: Vec::new(),
        exit: None,
        tokens: Tokens::default(),
        models: Vec::new(),
    };
    run.project = project_name(run.cwd.as_deref());
    // The account and wait state since `since`.
    let mut cur = (run.profile.clone(), start, false);
    let close = |segs: &mut Vec<Segment>, cur: &(String, i64, bool), ts: i64| {
        if ts > cur.1 && !cur.0.is_empty() {
            segs.push(Segment {
                profile: cur.0.clone(),
                from: cur.1,
                to: ts,
                waiting: cur.2,
            });
        }
    };
    let mut last_ts = start;
    for e in evs {
        last_ts = last_ts.max(e.ts);
        if let Some(s) = &e.session
            && !run.sessions.contains(s)
        {
            run.sessions.push(s.clone());
        }
        let kind = e.ev.unwrap_or(EventKind::Start);
        match kind {
            EventKind::Start => {}
            EventKind::End => {
                run.ended = true;
                run.end = e.ts;
                run.exit = e.exit;
                continue;
            }
            EventKind::Switch | EventKind::Resume | EventKind::Wait => {
                let waiting = kind == EventKind::Wait;
                let profile = if e.profile.is_empty() {
                    cur.0.clone()
                } else {
                    e.profile.clone()
                };
                if profile != cur.0 || waiting != cur.2 {
                    close(&mut run.segments, &cur, e.ts);
                    cur = (profile, e.ts.max(cur.1), waiting);
                }
            }
            EventKind::Limit => {}
        }
        if kind != EventKind::Start {
            run.marks.push(Mark {
                ts: e.ts,
                ev: kind,
                profile: e.profile.clone(),
                from: e.from.clone(),
                limit: e.limit.clone(),
                until: e.until,
                reason: e.reason.clone(),
            });
        }
    }
    if !run.ended {
        let pid = run_pid(id);
        run.live = live
            .iter()
            .any(|(p, at)| Some(*p) == pid && (at - start).abs() <= 120);
        run.end = if run.live { now } else { last_ts };
    }
    close(&mut run.segments, &cur, run.end);
    run
}

/// Gives each token record of a run's sessions to the run (and to the
/// account active at its time), sets the runs' token totals and models,
/// and extends unfinished exec runs to their last record. Returns
/// `(run id, account, record)` for every record that belongs to a run.
pub fn attribute<'a>(
    runs: &mut [Run],
    recs: &'a HashMap<String, Vec<TokenRec>>,
    now: i64,
) -> Vec<(String, String, &'a TokenRec)> {
    // Session → runs that name it, by start.
    let mut by_session: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, r) in runs.iter().enumerate() {
        for s in &r.sessions {
            by_session.entry(s.as_str()).or_default().push(i);
        }
    }
    let mut out = Vec::new();
    for (sid, idx) in &by_session {
        let Some(list) = recs.get(*sid) else {
            continue;
        };
        let windows: Vec<(usize, i64, i64)> = idx
            .iter()
            .enumerate()
            .map(|(k, &i)| {
                let next = idx.get(k + 1).map(|&j| runs[j].start);
                let (a, b) = runs[i].token_window(next, now);
                (i, a, b)
            })
            .collect();
        for rec in list {
            let Some(&(i, _, _)) = windows
                .iter()
                .rev()
                .find(|(_, a, b)| *a <= rec.ts && rec.ts <= *b)
            else {
                continue;
            };
            let profile = runs[i].profile_at(rec.ts).to_string();
            out.push((i, profile, rec));
        }
    }
    for (i, _, rec) in &out {
        let r = &mut runs[*i];
        r.tokens.add(&rec.tokens);
        if !r.models.contains(&rec.model) {
            r.models.push(rec.model.clone());
        }
        if !r.ended && !r.live && rec.ts > r.end {
            r.end = rec.ts;
            match r.segments.last_mut() {
                Some(s) => s.to = rec.ts,
                None => r.segments.push(Segment {
                    profile: r.profile.clone(),
                    from: r.start,
                    to: rec.ts,
                    waiting: false,
                }),
            }
        }
    }
    out.into_iter()
        .map(|(i, p, rec)| (runs[i].id.clone(), p, rec))
        .collect()
}

/// Runs, run time and tokens for one key (account, mode, project, ...).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Share {
    pub key: String,
    pub runs: u32,
    /// Active (not waiting) seconds in the range.
    pub secs: i64,
    pub tokens: Tokens,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Totals {
    pub runs: u32,
    pub live: u32,
    pub secs: i64,
    pub limits: u32,
    pub switches: u32,
    pub waits: u32,
    pub tokens: Tokens,
    /// Runs in the range that have token data.
    pub runs_with_tokens: u32,
}

/// One event for the feed, with its run's context.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FeedItem {
    #[serde(flatten)]
    pub mark: Mark,
    pub run: String,
    pub tool: Tool,
    pub mode: RunMode,
    pub project: String,
}

/// Tokens per account per time bucket.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TokenSeries {
    pub bucket_secs: i64,
    /// `(bucket start, account → total tokens)`, oldest first, every bucket.
    pub buckets: Vec<(i64, BTreeMap<String, u64>)>,
}

/// Everything the activity part of the dashboard shows for one range.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Activity {
    pub since: i64,
    pub now: i64,
    pub totals: Totals,
    pub by_account: Vec<Share>,
    pub by_mode: Vec<Share>,
    pub by_project: Vec<Share>,
    pub by_tool: Vec<Share>,
    pub by_model: Vec<Share>,
    /// Active seconds by local weekday (0 = Monday) and hour.
    pub heat: Vec<Vec<i64>>,
    pub token_series: TokenSeries,
    /// Runs that overlap the range, newest first.
    pub runs: Vec<Run>,
    pub events: Vec<FeedItem>,
}

fn overlap(a: i64, b: i64, since: i64, now: i64) -> i64 {
    (b.min(now) - a.max(since)).max(0)
}

fn mode_key(m: RunMode) -> &'static str {
    match m {
        RunMode::Tui => "tui",
        RunMode::Exec => "exec",
        RunMode::Print => "print",
    }
}

/// `local(ts)` gives `(weekday 0 = Monday, hour, seconds into the hour)`.
pub fn summarize(
    mut runs: Vec<Run>,
    recs: &HashMap<String, Vec<TokenRec>>,
    since: i64,
    now: i64,
    local: &dyn Fn(i64) -> (usize, usize, i64),
) -> Activity {
    let attributed = attribute(&mut runs, recs, now);
    runs.retain(|r| r.end >= since && r.start <= now);
    let mut acc: BTreeMap<String, Share> = BTreeMap::new();
    let mut modes: BTreeMap<String, Share> = BTreeMap::new();
    let mut projects: BTreeMap<String, Share> = BTreeMap::new();
    let mut tools: BTreeMap<String, Share> = BTreeMap::new();
    let mut models: BTreeMap<String, Share> = BTreeMap::new();
    let mut heat = vec![vec![0i64; 24]; 7];
    let mut t = Totals::default();
    let mut feed = Vec::new();
    for r in &runs {
        t.runs += 1;
        t.live += u32::from(r.live);
        let mode = modes.entry(mode_key(r.mode).into()).or_default();
        let proj = projects.entry(r.project.clone()).or_default();
        let tool = tools.entry(r.tool.bin().into()).or_default();
        mode.runs += 1;
        proj.runs += 1;
        tool.runs += 1;
        let mut on: Vec<&str> = Vec::new();
        for s in r.segments.iter().filter(|s| !s.waiting) {
            let secs = overlap(s.from, s.to, since, now);
            if secs == 0 {
                continue;
            }
            t.secs += secs;
            mode.secs += secs;
            proj.secs += secs;
            tool.secs += secs;
            let a = acc.entry(s.profile.clone()).or_default();
            a.secs += secs;
            if !on.contains(&s.profile.as_str()) {
                on.push(&s.profile);
                a.runs += 1;
            }
            add_heat(&mut heat, s.from.max(since), s.to.min(now), local);
        }
        for m in r.marks.iter().filter(|m| m.ts >= since && m.ts <= now) {
            match m.ev {
                EventKind::Limit => t.limits += 1,
                EventKind::Switch => t.switches += 1,
                EventKind::Wait => t.waits += 1,
                _ => {}
            }
            feed.push(FeedItem {
                mark: m.clone(),
                run: r.id.clone(),
                tool: r.tool,
                mode: r.mode,
                project: r.project.clone(),
            });
        }
    }
    let bucket = if now - since <= 2 * 86_400 {
        3600
    } else {
        6 * 3600
    };
    let first = since - since.rem_euclid(bucket);
    let mut buckets: Vec<(i64, BTreeMap<String, u64>)> = (0..)
        .map(|k| first + k * bucket)
        .take_while(|b| *b <= now)
        .map(|b| (b, BTreeMap::new()))
        .collect();
    let mut with_tokens: Vec<&str> = Vec::new();
    let by_id: HashMap<&str, &Run> = runs.iter().map(|r| (r.id.as_str(), r)).collect();
    for (id, profile, rec) in &attributed {
        if rec.ts < since || rec.ts > now {
            continue;
        }
        let Some(r) = by_id.get(id.as_str()) else {
            continue;
        };
        if !with_tokens.contains(&r.id.as_str()) {
            with_tokens.push(&r.id);
        }
        t.tokens.add(&rec.tokens);
        acc.entry(profile.clone())
            .or_default()
            .tokens
            .add(&rec.tokens);
        projects
            .entry(r.project.clone())
            .or_default()
            .tokens
            .add(&rec.tokens);
        tools
            .entry(r.tool.bin().into())
            .or_default()
            .tokens
            .add(&rec.tokens);
        modes
            .entry(mode_key(r.mode).into())
            .or_default()
            .tokens
            .add(&rec.tokens);
        let m = models.entry(rec.model.clone()).or_default();
        m.tokens.add(&rec.tokens);
        let k = ((rec.ts - first) / bucket) as usize;
        if let Some((_, by)) = buckets.get_mut(k) {
            *by.entry(profile.clone()).or_default() += rec.tokens.total();
        }
    }
    t.runs_with_tokens = with_tokens.len() as u32;
    feed.sort_by_key(|f| std::cmp::Reverse(f.mark.ts));
    feed.truncate(MAX_EVENTS);
    runs.reverse();
    Activity {
        since,
        now,
        totals: t,
        by_account: sorted(acc),
        by_mode: sorted(modes),
        by_project: fold_other(sorted(projects), MAX_PROJECTS),
        by_tool: sorted(tools),
        by_model: sorted(models),
        heat,
        token_series: TokenSeries {
            bucket_secs: bucket,
            buckets,
        },
        runs,
        events: feed,
    }
}

fn sorted(m: BTreeMap<String, Share>) -> Vec<Share> {
    let mut v: Vec<Share> = m.into_iter().map(|(key, s)| Share { key, ..s }).collect();
    v.sort_by(|a, b| {
        (b.secs, b.tokens.total(), b.runs)
            .cmp(&(a.secs, a.tokens.total(), a.runs))
            .then(a.key.cmp(&b.key))
    });
    v
}

/// Keeps the first `n - 1` and folds the rest into "Other" when there are
/// more than `n`.
fn fold_other(mut v: Vec<Share>, n: usize) -> Vec<Share> {
    if v.len() <= n {
        return v;
    }
    let rest = v.split_off(n - 1);
    let mut other = Share {
        key: "Other".into(),
        ..Default::default()
    };
    for s in rest {
        other.runs += s.runs;
        other.secs += s.secs;
        other.tokens.add(&s.tokens);
    }
    v.push(other);
    v
}

fn add_heat(heat: &mut [Vec<i64>], from: i64, to: i64, local: &dyn Fn(i64) -> (usize, usize, i64)) {
    let mut t = from;
    while t < to {
        let (wd, h, into) = local(t);
        let step = (3600 - into).clamp(1, 3600).min(to - t);
        if let Some(row) = heat.get_mut(wd)
            && let Some(c) = row.get_mut(h)
        {
            *c += step;
        }
        t += step;
    }
}
