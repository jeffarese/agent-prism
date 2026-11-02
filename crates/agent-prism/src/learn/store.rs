//! The learning store `<state>/learn/` (DESIGN §11f): one summary per Claude
//! session, kept after Claude Code deletes the transcript, plus an archive
//! of the accounts' usage samples beyond the 7 days the usage cache keeps.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use prism_core::fsutil::write_atomic;
use prism_core::journal::{self, EventKind, RunMode};
use prism_core::paths::Paths;
use prism_usage::cache::{Sample, UsageCache};
use serde::{Deserialize, Serialize};

use super::scan;

/// The format version of [`SessionStats`]; older summaries are read again
/// from their transcripts while those exist.
pub const VERSION: u32 = 1;
/// Summaries of sessions that ended longer ago are dropped.
const KEEP_SECS: i64 = 400 * 86_400;
/// Archived usage samples: one per account per this many seconds.
const SAMPLE_STEP: i64 = 300;
/// Archived usage samples are kept this long.
const SAMPLE_KEEP_SECS: i64 = 400 * 86_400;

/// Tokens of one kind of model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl Tokens {
    /// Thousands of input-token equivalents: output counts 5×, a cache write
    /// 1.25× and a cache read 0.1× (the API's price ratios). The same for
    /// every model; the raw counts stay in the store.
    pub fn ku(&self) -> f64 {
        (self.input as f64
            + 1.25 * self.cache_write as f64
            + 0.1 * self.cache_read as f64
            + 5.0 * self.output as f64)
            / 1000.0
    }

    pub fn is_empty(&self) -> bool {
        *self == Tokens::default()
    }

    pub fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.cache_write += o.cache_write;
        self.cache_read += o.cache_read;
        self.output += o.output;
    }

    /// The larger count of each kind.
    pub fn max(&self, o: &Tokens) -> Tokens {
        Tokens {
            input: self.input.max(o.input),
            cache_write: self.cache_write.max(o.cache_write),
            cache_read: self.cache_read.max(o.cache_read),
            output: self.output.max(o.output),
        }
    }
}

/// What agent-prism's run journal knows about the session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgpRun {
    /// The journal's run id.
    pub run: String,
    pub mode: RunMode,
    /// `(since, profile id)`: the accounts the run used, in order.
    pub accounts: Vec<(i64, String)>,
    /// Limits it hit.
    pub limits: u32,
}

/// One Claude session, summarized (`<state>/learn/sessions.jsonl`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    pub v: u32,
    pub session: String,
    /// The repository: the main work tree of the session's folder (a
    /// worktree counts as its repository), else the folder.
    pub repo: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// First and last model response (unix seconds).
    pub start: i64,
    pub end: i64,
    /// Time with model responses at most 5 minutes apart, over all agents.
    pub work_secs: i64,
    /// Claude Code's `entrypoint`: `cli` (interactive), `sdk-cli` (`-p`), ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    /// The main conversation's most used effort level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The main conversation's model (most kU).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The first prompt the user typed (first 200 characters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Claude Code's title for the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Tokens by model, all agents included.
    pub tokens: BTreeMap<String, Tokens>,
    /// kU of all agents and of the main conversation alone.
    pub ku: f64,
    pub main_ku: f64,
    /// Plain subagent transcripts, workflow runs and workflow agents.
    pub agents: u32,
    pub workflows: u32,
    pub workflow_agents: u32,
    /// Tool calls of the main conversation by tool name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, u32>,
    /// The most kU in any 30 minutes.
    pub peak30_ku: f64,
    /// `(bucket start, kU)` per 5 minutes, to set against the accounts'
    /// usage samples.
    pub buckets: Vec<(i64, f64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agp: Option<AgpRun>,
    /// Size and newest mtime of the transcripts when read.
    pub src_bytes: u64,
    pub src_mtime: i64,
}

impl SessionStats {
    /// kU per working hour; `None` under 3 working minutes.
    pub fn rate(&self) -> Option<f64> {
        (self.work_secs >= 180 && self.ku > 0.0).then(|| self.ku * 3600.0 / self.work_secs as f64)
    }

    /// The kind of task, from what it did.
    pub fn kind(&self) -> Kind {
        if self.workflows > 0 || self.tools.contains_key("Workflow") {
            Kind::Workflow
        } else if self.agents >= 5 {
            Kind::FanOut
        } else if self.agents > 0 {
            Kind::Agents
        } else {
            Kind::Solo
        }
    }
}

/// Kinds of task, by how much they fan out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Solo,
    Agents,
    FanOut,
    Workflow,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Solo => "solo",
            Kind::Agents => "1-4 agents",
            Kind::FanOut => "5+ agents",
            Kind::Workflow => "workflow",
        }
    }
}

pub fn dir(paths: &Paths) -> PathBuf {
    paths.state_dir.join("learn")
}

pub fn sessions_file(paths: &Paths) -> PathBuf {
    dir(paths).join("sessions.jsonl")
}

/// Sessions without a model response, by id: `(bytes, mtime)` of their
/// transcripts when read, so they are not read again until they change.
fn empty_file(paths: &Paths) -> PathBuf {
    dir(paths).join("empty.json")
}

/// Every stored summary. Bad lines are skipped.
pub fn load(paths: &Paths) -> Vec<SessionStats> {
    let Ok(text) = std::fs::read_to_string(sessions_file(paths)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn save(paths: &Paths, all: &[SessionStats]) -> Result<()> {
    let mut out = Vec::new();
    for s in all {
        serde_json::to_writer(&mut out, s)?;
        out.push(b'\n');
    }
    write_atomic(&sessions_file(paths), &out)
}

/// The repository of `cwd`: the folder that holds `.git` (for a worktree,
/// the main work tree its `.git` file points to). A folder that is gone:
/// the part before `/.claude/worktrees/`, else the folder itself.
pub fn repo_root(cwd: &Path) -> PathBuf {
    for dir in cwd.ancestors() {
        let git = dir.join(".git");
        if git.is_dir() {
            return dir.to_path_buf();
        }
        if git.is_file() {
            // `gitdir: /repo/.git/worktrees/<name>`
            let main = std::fs::read_to_string(&git).ok().and_then(|t| {
                let p = PathBuf::from(t.trim().strip_prefix("gitdir:")?.trim());
                let dot_git = p.ancestors().find(|a| a.ends_with(".git"))?;
                Some(dot_git.parent()?.to_path_buf())
            });
            return main.unwrap_or_else(|| dir.to_path_buf());
        }
    }
    let s = cwd.to_string_lossy();
    match s.find("/.claude/worktrees/") {
        Some(i) => PathBuf::from(&s[..i]),
        None => cwd.to_path_buf(),
    }
}

/// What a harvest did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Harvest {
    /// Sessions in the store afterwards.
    pub total: usize,
    /// Sessions read (new or changed).
    pub read: usize,
    /// Of those kept, sessions whose transcripts are gone.
    pub kept_gone: usize,
}

/// Reads new and changed sessions from `projects` dirs, joins the run
/// journal and rewrites the store. `progress` gets the number of sessions
/// to read before reading them.
pub fn harvest(
    paths: &Paths,
    projects: &[PathBuf],
    now: i64,
    progress: &mut dyn FnMut(usize),
) -> Result<Harvest> {
    let mut stored: HashMap<String, SessionStats> = load(paths)
        .into_iter()
        .map(|s| (s.session.clone(), s))
        .collect();
    // Session id → its files; the larger copy when a session shows up twice
    // (a renamed project folder).
    let mut found: HashMap<String, scan::SessionFiles> = HashMap::new();
    for root in projects {
        let Ok(slugs) = std::fs::read_dir(root) else {
            continue;
        };
        for slug in slugs.flatten() {
            let Ok(files) = std::fs::read_dir(slug.path()) else {
                continue;
            };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().is_none_or(|x| x != "jsonl") || !p.is_file() {
                    continue;
                }
                let Some(id) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                    continue;
                };
                let sf = scan::session_files(&p);
                if found.get(&id).is_none_or(|o| sf.bytes > o.bytes) {
                    found.insert(id, sf);
                }
            }
        }
    }
    let old_empty: HashMap<String, (u64, i64)> = std::fs::read_to_string(empty_file(paths))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut empty: BTreeMap<String, (u64, i64)> = BTreeMap::new();
    let todo: Vec<(&String, &scan::SessionFiles)> = found
        .iter()
        .filter(|(id, f)| {
            if old_empty.get(*id) == Some(&(f.bytes, f.mtime)) && !stored.contains_key(*id) {
                empty.insert((*id).clone(), (f.bytes, f.mtime));
                return false;
            }
            stored
                .get(*id)
                .is_none_or(|s| s.v != VERSION || s.src_bytes != f.bytes || s.src_mtime != f.mtime)
        })
        .collect();
    progress(todo.len());
    let mut repos: HashMap<String, String> = HashMap::new();
    let read = todo.len();
    for (id, files) in todo {
        let Some(mut s) = scan::summarize(id, files) else {
            stored.remove(id);
            empty.insert(id.clone(), (files.bytes, files.mtime));
            continue;
        };
        s.repo = repos
            .entry(s.cwd.clone())
            .or_insert_with(|| repo_root(Path::new(&s.cwd)).to_string_lossy().into_owned())
            .clone();
        if let Some(old) = stored.get(id) {
            s.agp = old.agp.clone();
        }
        stored.insert(id.clone(), s);
    }
    join_journal(paths, &mut stored);
    let mut all: Vec<SessionStats> = stored
        .into_values()
        .filter(|s| s.end >= now - KEEP_SECS)
        .collect();
    all.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| a.session.cmp(&b.session))
    });
    save(paths, &all)?;
    write_atomic(&empty_file(paths), &serde_json::to_vec(&empty)?)?;
    let ids: HashSet<&String> = found.keys().collect();
    Ok(Harvest {
        total: all.len(),
        read,
        kept_gone: all.iter().filter(|s| !ids.contains(&s.session)).count(),
    })
}

/// Fills in `agp` from the run journal for the sessions it names. Older
/// journal lines are trimmed away; what the store has stays then.
fn join_journal(paths: &Paths, stored: &mut HashMap<String, SessionStats>) {
    let mut runs: BTreeMap<String, Vec<journal::Event>> = BTreeMap::new();
    for ev in journal::read_since(paths, 0) {
        runs.entry(ev.run.clone()).or_default().push(ev);
    }
    for (run, evs) in runs {
        let Some(session) = evs.iter().rev().find_map(|e| e.session.clone()) else {
            continue;
        };
        let Some(s) = stored.get_mut(&session) else {
            continue;
        };
        let accounts = evs
            .iter()
            .filter(|e| {
                matches!(
                    e.ev,
                    Some(EventKind::Start | EventKind::Switch | EventKind::Resume)
                ) && !e.profile.is_empty()
            })
            .map(|e| (e.ts, e.profile.clone()))
            .fold(Vec::<(i64, String)>::new(), |mut v, (ts, p)| {
                if v.last().is_none_or(|l| l.1 != p) {
                    v.push((ts, p));
                }
                v
            });
        let limits = evs
            .iter()
            .filter(|e| e.ev == Some(EventKind::Limit))
            .count() as u32;
        let agp = AgpRun {
            run,
            mode: evs[0].mode,
            accounts,
            limits,
        };
        // A session resumed by a later run: keep the run that started it,
        // add the later run's accounts and limits.
        match &mut s.agp {
            Some(old) if old.run != agp.run && old.accounts.first() < agp.accounts.first() => {
                for a in agp.accounts {
                    if !old.accounts.contains(&a) {
                        old.accounts.push(a);
                    }
                }
                old.limits = old.limits.max(agp.limits);
            }
            _ => s.agp = Some(agp),
        }
    }
}

fn samples_file(paths: &Paths, id: &str) -> PathBuf {
    dir(paths).join("samples").join(format!("{id}.jsonl"))
}

/// Appends the usage samples of `ids` that the archive lacks, one per
/// [`SAMPLE_STEP`] (the last of each step that has ended), and drops
/// archived samples older than [`SAMPLE_KEEP_SECS`].
pub fn archive_samples(paths: &Paths, cache: &UsageCache, ids: &[String], now: i64) {
    for id in ids {
        if let Err(e) = archive_one(paths, cache, id, now) {
            log::warn!("learn: archiving usage samples of {id}: {e:#}");
        }
    }
}

fn archive_one(paths: &Paths, cache: &UsageCache, id: &str, now: i64) -> Result<()> {
    let path = samples_file(paths, id);
    let old: Vec<Sample> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let last = old.last().map_or(i64::MIN, |s| s.ts);
    let open_step = now - now.rem_euclid(SAMPLE_STEP);
    let mut steps: BTreeMap<i64, Sample> = BTreeMap::new();
    for s in cache.samples(id, last.saturating_add(1)) {
        let step = s.ts - s.ts.rem_euclid(SAMPLE_STEP);
        if step < open_step {
            steps.insert(step, s);
        }
    }
    let keep_from = now - SAMPLE_KEEP_SECS;
    if steps.is_empty() && old.first().is_none_or(|s| s.ts >= keep_from) {
        return Ok(());
    }
    let mut out = Vec::new();
    for s in old
        .iter()
        .filter(|s| s.ts >= keep_from)
        .chain(steps.values())
    {
        serde_json::to_writer(&mut out, s)?;
        out.push(b'\n');
    }
    write_atomic(&path, &out)
}

/// The accounts' plans at the last harvest (`<state>/learn/accounts.json`),
/// so kU can later be set against each plan's usage.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    pub capacity: f64,
    pub updated: i64,
}

pub fn accounts_file(paths: &Paths) -> PathBuf {
    dir(paths).join("accounts.json")
}

pub fn load_accounts(paths: &Paths) -> BTreeMap<String, AccountInfo> {
    std::fs::read_to_string(accounts_file(paths))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Merges `fresh` into the stored accounts (accounts not read now keep
/// their last entry).
pub fn save_accounts(paths: &Paths, fresh: BTreeMap<String, AccountInfo>) {
    let mut all = load_accounts(paths);
    all.extend(fresh);
    let res = serde_json::to_vec_pretty(&all)
        .map_err(anyhow::Error::from)
        .and_then(|b| write_atomic(&accounts_file(paths), &b));
    if let Err(e) = res {
        log::warn!("learn: saving accounts: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, ts: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","cwd":"/gone/app/.claude/worktrees/x","message":{{"id":"{id}","model":"m","usage":{{"input_tokens":1000,"output_tokens":0}}}}}}"#
        )
    }

    #[test]
    fn repo_of_worktrees_and_plain_folders() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("app");
        std::fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        assert_eq!(repo_root(&repo.join("src/deep")), repo);
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        assert_eq!(repo_root(&wt), repo);
        assert_eq!(
            repo_root(Path::new("/gone/app/.claude/worktrees/x/sub")),
            PathBuf::from("/gone/app")
        );
        assert_eq!(repo_root(Path::new("/gone/b")), PathBuf::from("/gone/b"));
    }

    #[test]
    fn harvest_is_incremental_and_keeps_gone_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(&dir.path().join("home"));
        let projects = dir.path().join("projects");
        let slug = projects.join("-gone-app");
        std::fs::create_dir_all(&slug).unwrap();
        let a = slug.join("a.jsonl");
        std::fs::write(&a, line("m1", "2026-11-02T09:00:00Z") + "\n").unwrap();
        std::fs::write(
            slug.join("b.jsonl"),
            line("m2", "2026-11-02T09:00:00Z") + "\n",
        )
        .unwrap();
        // The same session in a second (renamed) folder, smaller: ignored.
        let other = projects.join("-old-name");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("a.jsonl"), "{}\n").unwrap();
        // A session without a model response: read once, not stored.
        std::fs::write(slug.join("c.jsonl"), "{\"type\":\"user\"}\n").unwrap();
        let now = 1_793_600_000;
        let mut seen = Vec::new();
        let h = harvest(&paths, std::slice::from_ref(&projects), now, &mut |n| {
            seen.push(n)
        })
        .unwrap();
        assert_eq!((h.total, h.read, h.kept_gone), (2, 3, 0));
        let all = load(&paths);
        assert_eq!(all[0].repo, "/gone/app");
        assert_eq!(all[0].ku, 1.0);
        // Nothing changed: nothing is read again.
        let h = harvest(&paths, std::slice::from_ref(&projects), now, &mut |n| {
            seen.push(n)
        })
        .unwrap();
        assert_eq!(h.read, 0);
        // Claude Code deleted a transcript: its summary stays.
        std::fs::remove_file(&a).unwrap();
        std::fs::remove_file(other.join("a.jsonl")).unwrap();
        let h = harvest(&paths, &[projects], now, &mut |n| seen.push(n)).unwrap();
        assert_eq!((h.total, h.read, h.kept_gone), (2, 0, 1));
        assert_eq!(seen, vec![3, 0, 0]);
    }

    #[test]
    fn journal_runs_join_their_session() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let rec = journal::Recorder::new(paths.clone(), Default::default(), RunMode::Print, 100);
        rec.write(rec.event(EventKind::Start, "pb2", 100));
        let mut lim = rec.event(EventKind::Limit, "pb2", 200);
        lim.limit = Some("5h".into());
        rec.write(lim);
        let mut sw = rec.event(EventKind::Switch, "pb4", 210);
        sw.from = Some("pb2".into());
        rec.write(sw);
        rec.end("pb4", 300, 0, Some("s1".into()));
        let mut stored: HashMap<String, SessionStats> = [(
            "s1".to_string(),
            SessionStats {
                session: "s1".into(),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        join_journal(&paths, &mut stored);
        let agp = stored["s1"].agp.clone().unwrap();
        assert_eq!(agp.mode, RunMode::Print);
        assert_eq!(
            agp.accounts,
            vec![(100, "pb2".to_string()), (210, "pb4".to_string())]
        );
        assert_eq!(agp.limits, 1);
    }

    #[test]
    fn samples_archive_one_per_step() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let cache = UsageCache::new(paths.usage_dir());
        let s = |ts, u| Sample {
            ts,
            u5h: Some(u),
            u7d: Some(1.0),
        };
        for x in [s(600, 1.0), s(700, 2.0), s(950, 3.0), s(1210, 4.0)] {
            cache.append_sample("a", &x).unwrap();
        }
        // At 1250 the step [1200, 1500) is still open.
        archive_samples(&paths, &cache, &["a".to_string()], 1250);
        let read = || -> Vec<Sample> {
            std::fs::read_to_string(samples_file(&paths, "a"))
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        };
        assert_eq!(read(), vec![s(700, 2.0), s(950, 3.0)]);
        archive_samples(&paths, &cache, &["a".to_string()], 1600);
        assert_eq!(read(), vec![s(700, 2.0), s(950, 3.0), s(1210, 4.0)]);
    }
}
