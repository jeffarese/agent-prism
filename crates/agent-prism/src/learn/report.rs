//! What `agp learn` prints: a repository's burn by kind of task and by
//! effort, its latest sessions, or one line per repository (`--all`).

use std::collections::{BTreeMap, HashMap};

use prism_usage::balance::fmt_duration;
use serde::Serialize;

use super::store::{Kind, SessionStats};

/// Burn of a group of sessions, in kU per working hour.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Group {
    pub name: String,
    pub sessions: usize,
    pub p50: f64,
    pub p75: f64,
    pub p90: f64,
    /// `p50` over the median of all sessions in every repository.
    pub vs_typical: f64,
    pub work_p50_secs: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Latest {
    pub session: String,
    pub start: i64,
    pub kind: Kind,
    pub effort: Option<String>,
    pub work_secs: i64,
    /// kU per working hour; `None` under 3 working minutes.
    pub rate: Option<f64>,
    /// Labels of the accounts agent-prism ran it on, in order.
    pub accounts: Vec<String>,
    pub limits: u32,
    pub title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RepoReport {
    pub repo: String,
    pub sessions: usize,
    /// Sessions with at least 3 working minutes (the ones in the groups).
    pub rated: usize,
    pub first: Option<i64>,
    pub last: Option<i64>,
    /// Median kU per working hour over all repositories.
    pub typical: Option<f64>,
    pub kinds: Vec<Group>,
    pub efforts: Vec<Group>,
    pub latest: Vec<Latest>,
    /// How many typical runs a new run here counts as in a pick (with the
    /// prompt given, if any), and why. `None`: too little data (1).
    pub new_run: Option<NewRun>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NewRun {
    pub weight: f64,
    pub why: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RepoLine {
    pub repo: String,
    pub sessions: usize,
    pub rated: usize,
    pub p50: Option<f64>,
    pub p90: Option<f64>,
    pub vs_typical: Option<f64>,
    pub last: i64,
}

/// The value at quantile `q` of sorted `v` (nearest rank).
fn quantile(v: &[f64], q: f64) -> f64 {
    let i = ((q * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1;
    v[i]
}

fn sorted(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(f64::total_cmp);
    v
}

fn round(x: f64, digits: i32) -> f64 {
    let f = 10f64.powi(digits);
    (x * f).round() / f
}

/// The median burn over all sessions.
pub fn typical(all: &[SessionStats]) -> Option<f64> {
    let v = sorted(all.iter().filter_map(SessionStats::rate).collect());
    (!v.is_empty()).then(|| round(quantile(&v, 0.5), 0))
}

fn group(name: String, ss: &[&SessionStats], typical: Option<f64>) -> Option<Group> {
    let rates = sorted(ss.iter().filter_map(|s| s.rate()).collect());
    if rates.is_empty() {
        return None;
    }
    let mut work: Vec<i64> = ss.iter().map(|s| s.work_secs).collect();
    work.sort_unstable();
    let p50 = quantile(&rates, 0.5);
    Some(Group {
        name,
        sessions: rates.len(),
        p50: round(p50, 0),
        p75: round(quantile(&rates, 0.75), 0),
        p90: round(quantile(&rates, 0.9), 0),
        vs_typical: round(typical.map_or(1.0, |t| p50 / t), 2),
        work_p50_secs: work[work.len().div_ceil(2) - 1],
    })
}

/// The report for `repo`; `labels` maps profile ids to account labels.
pub fn repo_report(
    all: &[SessionStats],
    repo: &str,
    labels: &HashMap<String, String>,
    latest: usize,
) -> RepoReport {
    let typical = typical(all);
    let mine: Vec<&SessionStats> = all.iter().filter(|s| s.repo == repo).collect();
    let rated: Vec<&SessionStats> = mine
        .iter()
        .copied()
        .filter(|s| s.rate().is_some())
        .collect();
    let mut by_kind: BTreeMap<Kind, Vec<&SessionStats>> = BTreeMap::new();
    let mut by_effort: BTreeMap<String, Vec<&SessionStats>> = BTreeMap::new();
    for s in &rated {
        by_kind.entry(s.kind()).or_default().push(s);
        let e = s.effort.clone().unwrap_or_else(|| "unknown".into());
        by_effort.entry(e).or_default().push(s);
    }
    let kinds = by_kind
        .into_iter()
        .filter_map(|(k, ss)| group(k.label().into(), &ss, typical))
        .collect();
    let rank = |e: &str| {
        ["low", "medium", "high", "xhigh", "max"]
            .iter()
            .position(|x| *x == e)
            .unwrap_or(9)
    };
    let mut efforts: Vec<Group> = by_effort
        .into_iter()
        .filter_map(|(e, ss)| group(e, &ss, typical))
        .collect();
    efforts.sort_by_key(|g| rank(&g.name));
    let mut recent = mine.clone();
    recent.sort_by_key(|s| std::cmp::Reverse(s.start));
    let latest = recent
        .into_iter()
        .take(latest)
        .map(|s| Latest {
            session: s.session.clone(),
            start: s.start,
            kind: s.kind(),
            effort: s.effort.clone(),
            work_secs: s.work_secs,
            rate: s.rate().map(|r| round(r, 0)),
            accounts: s
                .agp
                .iter()
                .flat_map(|a| &a.accounts)
                .map(|(_, id)| labels.get(id).cloned().unwrap_or_else(|| id.clone()))
                .collect(),
            limits: s.agp.as_ref().map_or(0, |a| a.limits),
            title: s.title.clone().or_else(|| s.prompt.clone()),
        })
        .collect();
    RepoReport {
        repo: repo.to_string(),
        sessions: mine.len(),
        rated: rated.len(),
        first: mine.iter().map(|s| s.start).min(),
        last: mine.iter().map(|s| s.start).max(),
        typical,
        kinds,
        efforts,
        latest,
        new_run: None,
    }
}

/// One line per repository, most sessions first.
pub fn repo_lines(all: &[SessionStats]) -> Vec<RepoLine> {
    let typical = typical(all);
    let mut by_repo: BTreeMap<&str, Vec<&SessionStats>> = BTreeMap::new();
    for s in all {
        by_repo.entry(s.repo.as_str()).or_default().push(s);
    }
    let mut lines: Vec<RepoLine> = by_repo
        .into_iter()
        .map(|(repo, ss)| {
            let g = group(String::new(), &ss, typical);
            RepoLine {
                repo: repo.to_string(),
                sessions: ss.len(),
                rated: g.as_ref().map_or(0, |g| g.sessions),
                p50: g.as_ref().map(|g| g.p50),
                p90: g.as_ref().map(|g| g.p90),
                vs_typical: g.as_ref().map(|g| g.vs_typical),
                last: ss.iter().map(|s| s.start).max().unwrap_or(0),
            }
        })
        .collect();
    lines.sort_by(|a, b| {
        b.sessions
            .cmp(&a.sessions)
            .then_with(|| a.repo.cmp(&b.repo))
    });
    lines
}

/// 812, 4.5k, 13k.
pub fn fmt_ku(x: f64) -> String {
    if x < 1000.0 {
        format!("{x:.0}")
    } else if x < 10_000.0 {
        format!("{:.1}k", x / 1000.0)
    } else {
        format!("{:.0}k", x / 1000.0)
    }
}

fn fmt_day(ts: i64) -> String {
    jiff::Timestamp::from_second(ts)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

fn fmt_when(ts: i64) -> String {
    jiff::Timestamp::from_second(ts)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// `path` with the home folder as `~`.
fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && path.starts_with(&h) => format!("~{}", &path[h.len()..]),
        _ => path.to_string(),
    }
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

const UNITS: &str = "\
Burn is kU per working hour. 1 kU = 1000 input-token equivalents: output
counts 5x, a cache write 1.25x, a cache read 0.1x. Working time is when
the session or one of its agents answered at most 5 minutes apart.
\"x typical\" compares with the median session over all repositories.";

/// The text of a repository report.
pub fn repo_text(r: &RepoReport) -> String {
    let mut out = String::new();
    let name = r.repo.rsplit('/').next().unwrap_or(&r.repo);
    if r.sessions == 0 {
        out.push_str(&format!(
            "No sessions learned for {} yet.\n`agp learn --all` lists the repositories agp knows.\n",
            r.repo
        ));
        return out;
    }
    out.push_str(&format!("{name} ({})\n", tilde(&r.repo)));
    let what = match r.new_run.as_ref().and_then(|n| n.prompt.as_ref()) {
        Some(_) => "A new run with this prompt",
        None => "A new run here",
    };
    match &r.new_run {
        Some(n) if n.weight > 1.0 => out.push_str(&format!(
            "{what} counts as {:.1} runs in agp's picks ({}).\n",
            n.weight, n.why
        )),
        Some(n) => out.push_str(&format!("{what} counts as 1 run ({}).\n", n.why)),
        None => out.push_str(&format!(
            "{what} counts as 1 run (too few past runs to tell).\n"
        )),
    }
    out.push_str(&format!(
        "{} sessions from {} to {}, {} with 3+ working minutes. Typical over all repositories: {}.\n\n",
        r.sessions,
        r.first.map(fmt_day).unwrap_or_default(),
        r.last.map(fmt_day).unwrap_or_default(),
        r.rated,
        r.typical.map(fmt_ku).unwrap_or_else(|| "-".into()),
    ));
    for (title, groups) in [("KIND", &r.kinds), ("EFFORT", &r.efforts)] {
        if groups.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "{title:<12} {:>8} {:>7} {:>7} {:>7} {:>10} {:>12}\n",
            "SESSIONS", "MEDIAN", "P75", "P90", "X TYPICAL", "WORK MEDIAN"
        ));
        for g in groups.iter() {
            out.push_str(&format!(
                "{:<12} {:>8} {:>7} {:>7} {:>7} {:>9.1}x {:>12}\n",
                g.name,
                g.sessions,
                fmt_ku(g.p50),
                fmt_ku(g.p75),
                fmt_ku(g.p90),
                g.vs_typical,
                fmt_duration(g.work_p50_secs),
            ));
        }
        out.push('\n');
    }
    if !r.latest.is_empty() {
        out.push_str(&format!(
            "{:<11}  {:<10} {:<6} {:>6} {:>6}  {:<20} {}\n",
            "LATEST", "KIND", "EFFORT", "WORK", "BURN", "ACCOUNTS", "TITLE"
        ));
        for l in &r.latest {
            let mut acc = l.accounts.join("→");
            if l.limits > 0 {
                acc.push_str(&format!(
                    " ({} limit{})",
                    l.limits,
                    if l.limits > 1 { "s" } else { "" }
                ));
            }
            out.push_str(&format!(
                "{:<11}  {:<10} {:<6} {:>6} {:>6}  {:<20} {}\n",
                fmt_when(l.start),
                l.kind.label(),
                l.effort.as_deref().unwrap_or("-"),
                fmt_duration(l.work_secs),
                l.rate.map(fmt_ku).unwrap_or_else(|| "-".into()),
                cut(if acc.is_empty() { "-" } else { &acc }, 20),
                cut(l.title.as_deref().unwrap_or(""), 50),
            ));
        }
        out.push('\n');
    }
    out.push_str(UNITS);
    out.push('\n');
    out
}

/// The text of `--all`.
pub fn all_text(lines: &[RepoLine]) -> String {
    if lines.is_empty() {
        return "No sessions learned yet.\n".into();
    }
    let mut out = format!(
        "{:<44} {:>8} {:>7} {:>7} {:>10}  {}\n",
        "REPOSITORY", "SESSIONS", "MEDIAN", "P90", "X TYPICAL", "LAST"
    );
    for l in lines {
        out.push_str(&format!(
            "{:<44} {:>8} {:>7} {:>7} {:>10}  {}\n",
            cut(&tilde(&l.repo), 44),
            l.sessions,
            l.p50.map(fmt_ku).unwrap_or_else(|| "-".into()),
            l.p90.map(fmt_ku).unwrap_or_else(|| "-".into()),
            l.vs_typical
                .map(|v| format!("{v:.1}x"))
                .unwrap_or_else(|| "-".into()),
            fmt_day(l.last),
        ));
    }
    out.push('\n');
    out.push_str(UNITS);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::store::AgpRun;

    fn s(id: &str, repo: &str, ku: f64, work: i64, agents: u32, effort: &str) -> SessionStats {
        SessionStats {
            session: id.into(),
            repo: repo.into(),
            start: 1_793_600_000 + ku as i64,
            ku,
            work_secs: work,
            agents,
            effort: Some(effort.into()),
            ..Default::default()
        }
    }

    #[test]
    fn groups_by_kind_and_effort() {
        let mut all = vec![
            s("a", "/r", 1000.0, 3600, 0, "medium"),
            s("b", "/r", 2000.0, 3600, 0, "high"),
            s("c", "/r", 6000.0, 3600, 6, "xhigh"),
            s("d", "/other", 1000.0, 3600, 0, "medium"),
            // Under 3 working minutes: listed, not in the groups.
            s("e", "/r", 50.0, 60, 0, "medium"),
        ];
        all[2].agp = Some(AgpRun {
            run: "1-1".into(),
            accounts: vec![(1, "pb2".into()), (2, "pb4".into())],
            limits: 1,
            ..Default::default()
        });
        let labels: HashMap<String, String> = [("pb2".to_string(), "PB2".to_string())].into();
        let r = repo_report(&all, "/r", &labels, 10);
        assert_eq!((r.sessions, r.rated), (4, 3));
        // Median of 1000, 1000, 2000, 6000 (nearest rank).
        assert_eq!(r.typical, Some(1000.0));
        let names: Vec<&str> = r.kinds.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["solo", "5+ agents"]);
        assert_eq!(r.kinds[1].p50, 6000.0);
        assert_eq!(r.kinds[1].vs_typical, 6.0);
        let efforts: Vec<&str> = r.efforts.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(efforts, vec!["medium", "high", "xhigh"]);
        let c = r.latest.iter().find(|l| l.session == "c").unwrap();
        assert_eq!(c.accounts, vec!["PB2", "pb4"]);
        assert_eq!(r.latest[0].session, "c");
        let text = repo_text(&r);
        assert!(text.contains("PB2→pb4 (1 limit)"), "{text}");
        assert!(text.contains("5+ agents"));
        let lines = repo_lines(&all);
        assert_eq!(lines[0].repo, "/r");
        assert_eq!(lines[0].sessions, 4);
    }

    #[test]
    fn units() {
        assert_eq!(fmt_ku(812.4), "812");
        assert_eq!(fmt_ku(4546.0), "4.5k");
        assert_eq!(fmt_ku(13_199.0), "13k");
        assert_eq!(quantile(&[1.0, 2.0, 3.0, 4.0], 0.9), 4.0);
        assert_eq!(quantile(&[1.0], 0.5), 1.0);
    }

    #[test]
    fn empty_repo() {
        let r = repo_report(&[], "/x", &HashMap::new(), 5);
        assert!(repo_text(&r).starts_with("No sessions learned for /x"));
    }
}
