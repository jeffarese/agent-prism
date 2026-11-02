//! Learning from past sessions (DESIGN §11f): `agp learn` and the detached
//! `agp _learn`.
//!
//! A harvest reads the Claude transcripts that are new or changed since the
//! last one, joins agent-prism's run journal and keeps one summary per
//! session in `<state>/learn/sessions.jsonl`, also after Claude Code deletes
//! the transcript (after 30 days by default). It also archives the accounts'
//! usage samples (the usage cache keeps 7 days) and their plans. Launches
//! start a harvest in the background at most every [`EVERY_SECS`]. Nothing
//! leaves the machine, and the balancer does not use any of it yet.

pub mod report;
mod scan;
pub mod store;

use std::collections::{BTreeMap, HashMap};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use prism_core::fsutil::{FileLock, write_atomic};
use prism_core::model::Tool;
use prism_core::paths::Paths;
use prism_core::profile::Profile;
use serde::{Deserialize, Serialize};

use crate::app::AppCtx;
use crate::launch::{self, unix_now};
use crate::tokens::KeychainTokens;

/// A launch starts a background harvest when the last one started this
/// long ago.
pub const EVERY_SECS: i64 = 6 * 3600;
/// `agp learn` waits this long for a running background harvest.
const WAIT: Duration = Duration::from_secs(120);
/// Rows of the latest sessions in a repository report.
const LATEST: usize = 12;

/// `agp learn` options.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LearnOpts {
    /// The folder whose repository to show (default: the current one).
    pub path: Option<String>,
    /// One line per repository.
    pub all: bool,
    pub json: bool,
}

/// `<state>/learn/state.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct State {
    /// When the last harvest started and ended (unix seconds).
    last_start: i64,
    last_done: i64,
    /// Sessions it read, and in the store after it.
    read: usize,
    total: usize,
}

fn state_file(paths: &Paths) -> PathBuf {
    store::dir(paths).join("state.json")
}

fn lock_file(paths: &Paths) -> PathBuf {
    store::dir(paths).join("harvest.lock")
}

fn load_state(paths: &Paths) -> State {
    std::fs::read_to_string(state_file(paths))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_state(paths: &Paths, s: &State) {
    let res = serde_json::to_vec_pretty(s)
        .map_err(anyhow::Error::from)
        .and_then(|b| write_atomic(&state_file(paths), &b));
    if let Err(e) = res {
        log::warn!("learn: saving state: {e:#}");
    }
}

/// Background harvests are off: `AGP_NO_LEARN`, or `AGP_NO_POLL` (tests).
fn disabled() -> bool {
    launch::polling_disabled() || prism_core::env::var_os("AGP_NO_LEARN").is_some_and(|v| v != "0")
}

/// At a launch: starts `agp _learn` detached when the last harvest started
/// [`EVERY_SECS`] ago or more and none runs.
pub fn maybe_start(paths: &Paths, now: i64) {
    // In unit tests `current_exe` is the test binary.
    if cfg!(test) || disabled() {
        return;
    }
    let Ok(Some(lock)) = FileLock::acquire(&lock_file(paths), Duration::ZERO) else {
        return;
    };
    let mut st = load_state(paths);
    if !due(&st, now) {
        return;
    }
    st.last_start = now;
    save_state(paths, &st);
    drop(lock);
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("_learn");
    match crate::bg::spawn_detached(cmd) {
        Ok(pid) => log::info!("learn: harvest started (pid {pid})"),
        Err(e) => log::warn!("learn: starting the harvest: {e:#}"),
    }
}

/// A background harvest is due: the last one started [`EVERY_SECS`] ago or
/// more (or never).
fn due(st: &State, now: i64) -> bool {
    now - st.last_start >= EVERY_SECS
}

/// `agp _learn` (internal, detached): one harvest, unless one runs.
pub fn run_background(ctx: &AppCtx) -> Result<i32> {
    let Some(_lock) = FileLock::acquire(&lock_file(&ctx.paths), Duration::ZERO)? else {
        log::info!("learn: another harvest runs");
        return Ok(0);
    };
    harvest_now(ctx, &mut |_| {})?;
    Ok(0)
}

/// Every `projects` folder that holds Claude transcripts: the common home's
/// and each Claude profile's own (once each).
fn projects_dirs(paths: &Paths, profiles: &[Profile]) -> Vec<PathBuf> {
    let mut dirs = vec![paths.home.join(".claude").join("projects")];
    for p in profiles.iter().filter(|p| p.tool == Tool::Claude) {
        dirs.push(p.common_home.join("projects"));
        dirs.push(p.dir.join("projects"));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for d in dirs {
        if let Ok(c) = d.canonicalize()
            && c.is_dir()
            && !out.contains(&c)
        {
            out.push(c);
        }
    }
    out
}

fn harvest_now(ctx: &AppCtx, progress: &mut dyn FnMut(usize)) -> Result<store::Harvest> {
    let config = ctx.load_config()?;
    let profiles = ctx.profiles(&config);
    let started = unix_now();
    let h = store::harvest(
        &ctx.paths,
        &projects_dirs(&ctx.paths, &profiles),
        started,
        progress,
    )?;
    let claude: Vec<Profile> = profiles
        .iter()
        .filter(|p| p.tool == Tool::Claude)
        .cloned()
        .collect();
    let ids: Vec<String> = claude.iter().map(|p| p.id.clone()).collect();
    store::archive_samples(&ctx.paths, &ctx.cache, &ids, unix_now());
    store::save_accounts(&ctx.paths, account_infos(ctx, &claude));
    let mut st = load_state(&ctx.paths);
    st.last_start = st.last_start.max(started);
    st.last_done = unix_now();
    st.read = h.read;
    st.total = h.total;
    save_state(&ctx.paths, &st);
    log::info!(
        "learn: {} sessions stored ({} read, {} whose transcripts are gone) in {}s",
        h.total,
        h.read,
        h.kept_gone,
        st.last_done - started
    );
    Ok(h)
}

/// The plans of the accounts whose credentials can be read now.
fn account_infos(ctx: &AppCtx, profiles: &[Profile]) -> BTreeMap<String, store::AccountInfo> {
    let tokens = KeychainTokens::new(ctx.store.clone(), profiles.to_vec());
    let now = unix_now();
    profiles
        .iter()
        .filter_map(|p| {
            let c = tokens.credentials(&p.id).ok().flatten()?;
            let mut id = prism_core::identity::read_identity(p, &ctx.paths);
            id.tool = p.tool;
            id.subscription = c.subscription_type.clone();
            id.rate_limit_tier = c.rate_limit_tier.clone();
            Some((
                p.id.clone(),
                store::AccountInfo {
                    label: p.label.clone(),
                    subscription: c.subscription_type,
                    tier: c.rate_limit_tier,
                    capacity: id.capacity(),
                    updated: now,
                },
            ))
        })
        .collect()
}

/// `agp learn [<path>] [--all] [--json]`: brings the store up to date, then
/// shows the repository of `path` (or every repository).
pub fn run(ctx: &AppCtx, opts: LearnOpts) -> Result<i32> {
    let tell = !opts.json && std::io::stderr().is_terminal();
    match FileLock::acquire(&lock_file(&ctx.paths), WAIT)? {
        Some(_lock) => {
            harvest_now(ctx, &mut |n| {
                if tell && n > 20 {
                    eprintln!("agp: reading {n} new or changed sessions…");
                }
            })?;
        }
        None => {
            if tell {
                eprintln!("agp: a harvest is still running; showing what is stored");
            }
        }
    }
    let all = store::load(&ctx.paths);
    let config = ctx.load_config()?;
    let labels: HashMap<String, String> = ctx
        .profiles(&config)
        .into_iter()
        .map(|p| (p.id, p.label))
        .collect();
    if opts.all {
        let lines = report::repo_lines(&all);
        if opts.json {
            println!("{}", serde_json::to_string_pretty(&lines)?);
        } else {
            print!("{}", report::all_text(&lines));
        }
        return Ok(0);
    }
    let path = match &opts.path {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir()?,
    };
    let path = path.canonicalize().unwrap_or(path);
    let repo = store::repo_root(&path).to_string_lossy().into_owned();
    let r = report::repo_report(&all, &repo, &labels, LATEST);
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        print!("{}", report::repo_text(&r));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_harvest_every_six_hours() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        assert!(due(&load_state(&paths), 1_000_000));
        save_state(
            &paths,
            &State {
                last_start: 1_000_000,
                ..Default::default()
            },
        );
        let st = load_state(&paths);
        assert!(!due(&st, 1_000_000 + EVERY_SECS - 1));
        assert!(due(&st, 1_000_000 + EVERY_SECS));
    }

    #[test]
    fn projects_dirs_once_each() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let projects = paths.home.join(".claude/projects");
        std::fs::create_dir_all(&projects).unwrap();
        assert_eq!(
            projects_dirs(&paths, &[]),
            vec![projects.canonicalize().unwrap()]
        );
    }
}
