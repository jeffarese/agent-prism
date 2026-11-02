//! How many typical runs a new run counts as (DESIGN §11f), from the store:
//! past runs in the same repository with a similar first prompt, else the
//! repository's runs. The balancer uses it for the run's pick and claim.

use std::collections::HashSet;
use std::path::Path;

use prism_core::paths::Paths;
use serde::Deserialize;

use super::store;

/// The learned weight of a run, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Estimate {
    /// Typical runs this one counts as, from 1 to [`MAX_WEIGHT`].
    pub weight: f64,
    /// For the pick reason and the log, e.g. "14 similar runs in fathom".
    pub why: String,
}

/// A run never counts as more than this many typical runs.
pub const MAX_WEIGHT: f64 = 4.0;
/// Prompts at least this similar (shared words over all words) match.
const SIMILAR: f64 = 0.5;
/// Similar runs needed, and the most used (most similar, then newest).
const MIN_SIMILAR: usize = 3;
const MAX_SIMILAR: usize = 20;
/// Runs in the repository needed without similar ones, and the newest used.
const MIN_REPO: usize = 5;
const MAX_REPO: usize = 50;
/// Only runs that started this recently count.
const RECENT_SECS: i64 = 90 * 86_400;

/// The fields of a stored summary the estimate reads.
#[derive(Deserialize)]
struct Past {
    repo: String,
    #[serde(default)]
    prompt: Option<String>,
    start: i64,
    work_secs: i64,
    ku: f64,
}

impl Past {
    fn rate(&self) -> Option<f64> {
        (self.work_secs >= 180 && self.ku > 0.0).then(|| self.ku * 3600.0 / self.work_secs as f64)
    }
}

/// Lowercase words of two or more letters or digits in the first 200
/// characters (the length of a stored prompt).
fn words(text: &str) -> HashSet<String> {
    let head: String = text.chars().take(200).collect();
    head.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect()
}

fn similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

fn quantile(mut v: Vec<f64>, q: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let i = ((q * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1;
    Some(v[i])
}

/// The weight of a run in `cwd` whose prompt is `prompt` (none for a
/// session started without one). `None` when the store knows too little.
pub fn estimate(paths: &Paths, cwd: &Path, prompt: Option<&str>, now: i64) -> Option<Estimate> {
    let text = std::fs::read_to_string(store::sessions_file(paths)).ok()?;
    let past: Vec<Past> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let repo = store::repo_root(cwd).to_string_lossy().into_owned();
    estimate_from(&past, &repo, prompt, now)
}

fn estimate_from(past: &[Past], repo: &str, prompt: Option<&str>, now: i64) -> Option<Estimate> {
    let typical = quantile(past.iter().filter_map(Past::rate).collect(), 0.5)?;
    let name = repo.rsplit('/').next().unwrap_or(repo);
    let mut mine: Vec<(&Past, f64)> = past
        .iter()
        .filter(|p| p.repo == repo && p.start >= now - RECENT_SECS)
        .filter_map(|p| Some((p, p.rate()?)))
        .collect();
    mine.sort_by_key(|(p, _)| std::cmp::Reverse(p.start));
    let want = prompt.map(words).filter(|w| !w.is_empty());
    let similar: Vec<(f64, f64)> = match &want {
        Some(w) => {
            let mut v: Vec<(f64, f64)> = mine
                .iter()
                .filter_map(|(p, r)| {
                    let s = similarity(w, &words(p.prompt.as_deref()?));
                    (s >= SIMILAR).then_some((s, *r))
                })
                .collect();
            // Most similar first; the sort is stable, so newest among equals.
            v.sort_by(|a, b| b.0.total_cmp(&a.0));
            v.truncate(MAX_SIMILAR);
            v
        }
        None => Vec::new(),
    };
    let (rate, why) = if similar.len() >= MIN_SIMILAR {
        let n = similar.len();
        let rate = quantile(similar.into_iter().map(|(_, r)| r).collect(), 0.75)?;
        (rate, format!("{n} similar runs in {name}"))
    } else if mine.len() >= MIN_REPO {
        let rates: Vec<f64> = mine.iter().take(MAX_REPO).map(|(_, r)| *r).collect();
        let n = rates.len();
        (quantile(rates, 0.5)?, format!("{n} recent runs in {name}"))
    } else {
        return None;
    };
    let weight = ((rate / typical).clamp(1.0, MAX_WEIGHT) * 10.0).round() / 10.0;
    Some(Estimate { weight, why })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn past(repo: &str, prompt: &str, ku_per_h: f64) -> Past {
        Past {
            repo: repo.into(),
            prompt: Some(prompt.into()),
            start: NOW - 3600,
            work_secs: 3600,
            ku: ku_per_h,
        }
    }

    fn lanes() -> Vec<Past> {
        let mut v = Vec::new();
        for i in 0..6 {
            v.push(past("/w/other", &format!("small fix {i}"), 5000.0));
        }
        for lane in ["core", "combat", "world", "base"] {
            v.push(past(
                "/w/fathom",
                &format!("You are a lane runner for the FATHOM v3 orchestration. Lane: {lane}."),
                12_000.0,
            ));
        }
        v.push(past(
            "/w/fathom",
            "can we make the statusline mobile friendly",
            4000.0,
        ));
        v
    }

    #[test]
    fn similar_prompts_weigh_like_their_runs() {
        let v = lanes();
        // Typical: the median of 6 × 5000, 4 × 12000 and 4000 is 5000.
        let e = estimate_from(
            &v,
            "/w/fathom",
            Some("You are a lane runner for the FATHOM v3 orchestration. Lane: ui."),
            NOW,
        )
        .unwrap();
        assert_eq!(e.weight, 2.4);
        assert_eq!(e.why, "4 similar runs in fathom");
    }

    #[test]
    fn otherwise_the_repository_median() {
        let v = lanes();
        // No similar prompt: the median of fathom's 5 runs (12000).
        let e = estimate_from(&v, "/w/fathom", Some("rename a variable"), NOW).unwrap();
        assert_eq!((e.weight, e.why.as_str()), (2.4, "5 recent runs in fathom"));
        let e = estimate_from(&v, "/w/fathom", None, NOW).unwrap();
        assert_eq!(e.weight, 2.4);
        // Lighter than typical still counts as one run.
        let e = estimate_from(&v, "/w/other", None, NOW).unwrap();
        assert_eq!(e.weight, 1.0);
    }

    #[test]
    fn too_little_data_or_too_old() {
        let v = lanes();
        assert!(estimate_from(&v, "/w/new", Some("anything"), NOW).is_none());
        assert!(estimate_from(&v, "/w/fathom", None, NOW + RECENT_SECS + 3600).is_none());
        assert!(estimate_from(&[], "/w/fathom", None, NOW).is_none());
    }

    #[test]
    fn capped() {
        let mut v = lanes();
        for p in v.iter_mut().filter(|p| p.repo == "/w/fathom") {
            p.ku = 80_000.0;
        }
        let e = estimate_from(&v, "/w/fathom", None, NOW).unwrap();
        assert_eq!(e.weight, MAX_WEIGHT);
    }
}
