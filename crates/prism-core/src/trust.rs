//! Folder trust across Claude accounts (DESIGN §4.7).
//!
//! Claude keeps "trust this folder" per account, in `.claude.json` at
//! `projects["<abs path>"].hasTrustDialogAccepted`. Before Claude starts on
//! an account, [`sync`] adds the trust of every folder that another Claude
//! account trusts. It only adds trust; it never removes it.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::model::Tool;
use crate::paths::Paths;
use crate::profile::Profile;

/// The per-folder flag Claude sets when the trust dialog is accepted.
pub const TRUST_KEY: &str = "hasTrustDialogAccepted";

/// How long [`sync`] waits for Claude's lock on the file before it skips.
const LOCK_WAIT: Duration = Duration::from_secs(1);

/// A lock older than this is stale (proper-lockfile's default, which Claude
/// uses).
const LOCK_STALE: Duration = Duration::from_secs(10);

/// A new project entry: the default entry Claude itself writes when trust is
/// accepted in a folder it has no entry for, with trust on.
fn new_entry() -> Value {
    serde_json::json!({
        "allowedTools": [],
        "mcpContextUris": [],
        "mcpServers": {},
        "enabledMcpjsonServers": [],
        "disabledMcpjsonServers": [],
        TRUST_KEY: true,
        "hasClaudeMdExternalIncludesApproved": false,
        "hasClaudeMdExternalIncludesWarningShown": false,
    })
}

/// The folders a parsed `.claude.json` trusts.
pub fn trusted(v: &Value) -> BTreeSet<String> {
    v.get("projects")
        .and_then(Value::as_object)
        .map(|p| {
            p.iter()
                .filter(|(_, e)| e.get(TRUST_KEY) == Some(&Value::Bool(true)))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Trust `folders` in a parsed `.claude.json` and return how many changed.
/// A folder with an entry only gets the flag; a folder without one gets
/// Claude's default entry. New entries go at the end, like Claude adds them.
pub fn add_trust(v: &mut Value, folders: &BTreeSet<String>) -> usize {
    let Some(root) = v.as_object_mut() else {
        return 0;
    };
    let projects = root
        .entry("projects")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(projects) = projects.as_object_mut() else {
        return 0;
    };
    let mut changed = 0;
    for f in folders {
        match projects.get_mut(f) {
            None => {
                projects.insert(f.clone(), new_entry());
                changed += 1;
            }
            Some(Value::Object(e)) if e.get(TRUST_KEY) != Some(&Value::Bool(true)) => {
                e.insert(TRUST_KEY.into(), Value::Bool(true));
                changed += 1;
            }
            // Trusted already, or not an object: leave it.
            Some(_) => {}
        }
    }
    changed
}

/// Parse a `.claude.json`. Missing: `None`. Unreadable or not JSON: `None`
/// and a warning.
fn read(path: &Path) -> Option<Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            log::warn!("trust sync: cannot read {}: {e}; skipped", path.display());
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!(
                "trust sync: {} is not valid JSON ({e}); skipped",
                path.display()
            );
            None
        }
    }
}

/// The folders that the Claude accounts in `profiles` trust, not counting
/// the file `skip`.
fn union(paths: &Paths, profiles: &[Profile], skip: &Path) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut all = BTreeSet::new();
    for p in profiles.iter().filter(|p| p.tool == Tool::Claude) {
        let file = p.claude_json_path(paths);
        if file == skip || !seen.insert(file.clone()) {
            continue;
        }
        if let Some(v) = read(&file) {
            all.extend(trusted(&v));
        }
    }
    all
}

/// How many folders `target` trusts, and how many that other accounts in
/// `profiles` trust it does not (for `doctor`). `None` when its
/// `.claude.json` is missing or unreadable.
pub fn status(paths: &Paths, target: &Profile, profiles: &[Profile]) -> Option<(usize, usize)> {
    let file = target.claude_json_path(paths);
    let own = trusted(&read(&file)?);
    let missing = union(paths, profiles, &file).difference(&own).count();
    Some((own.len(), missing))
}

/// Give the Claude account `target` the trust of every folder that another
/// Claude account in `profiles` trusts, and return how many folders it
/// added. Never fails: a problem is logged and the file stays as it is. A
/// missing `.claude.json` (a new login) is left to Claude.
pub fn sync(paths: &Paths, target: &Profile, profiles: &[Profile]) -> usize {
    if target.tool != Tool::Claude {
        return 0;
    }
    match try_sync(paths, target, profiles) {
        Ok(n) => n,
        Err(e) => {
            log::warn!("trust sync for {}: {e:#}", target.id);
            0
        }
    }
}

fn try_sync(paths: &Paths, target: &Profile, profiles: &[Profile]) -> Result<usize> {
    let file = target.claude_json_path(paths);
    let all = union(paths, profiles, &file);
    if all.is_empty() {
        return Ok(0);
    }
    // A first look without the lock: most launches have nothing to add.
    let Some(v) = read(&file) else {
        return Ok(0);
    };
    let missing: BTreeSet<String> = all.difference(&trusted(&v)).cloned().collect();
    if missing.is_empty() {
        return Ok(0);
    }
    // Claude takes this lock for its own writes and re-reads the file under
    // it, so neither side loses the other's change.
    let _lock = ConfigLock::acquire(&file)?;
    let Some(mut v) = read(&file) else {
        return Ok(0);
    };
    let added = add_trust(&mut v, &missing);
    if added == 0 {
        return Ok(0);
    }
    // Claude writes `JSON.stringify(config, null, 2)`; serde_json's pretty
    // form with preserve_order and float_roundtrip gives the same bytes.
    let text = serde_json::to_string_pretty(&v).context("serializing")?;
    // Write through a symlinked `.claude.json`, not over the link.
    let dest = std::fs::canonicalize(&file).unwrap_or(file);
    crate::fsutil::write_atomic(&dest, text.as_bytes())?;
    log::info!(
        "trust sync: {} now trusts {added} more folder(s)",
        target.id
    );
    Ok(added)
}

/// Claude's lock on its config file: the dir `<file>.lock` (proper-lockfile).
/// Removed on drop.
struct ConfigLock(PathBuf);

impl ConfigLock {
    fn acquire(file: &Path) -> Result<ConfigLock> {
        let mut name = OsString::from(file.as_os_str());
        name.push(".lock");
        let lock = PathBuf::from(name);
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match std::fs::create_dir(&lock) {
                Ok(()) => return Ok(ConfigLock(lock)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // A lock left by a crashed process: take it over, as
                    // proper-lockfile does.
                    if is_stale(&lock) && std::fs::remove_dir(&lock).is_ok() {
                        continue;
                    }
                    anyhow::ensure!(
                        Instant::now() < deadline,
                        "{} is held (Claude is writing the file); skipped",
                        lock.display()
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("creating {}", lock.display()));
                }
            }
        }
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn is_stale(lock: &Path) -> bool {
    std::fs::metadata(lock)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > LOCK_STALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    fn set(dirs: &[(&str, bool)], root: &Path) -> (Paths, Vec<Profile>) {
        let paths = Paths::under(root);
        let common = paths.home.join(".claude");
        let profiles = dirs
            .iter()
            .map(|(id, default)| {
                let dir = if *default {
                    common.clone()
                } else {
                    paths.home.join(format!(".claude-{id}"))
                };
                fs::create_dir_all(&dir).unwrap();
                Profile::bare(id, Tool::Claude, dir, common.clone())
            })
            .collect();
        (paths, profiles)
    }

    fn write(p: &Profile, paths: &Paths, v: &Value) {
        fs::write(
            p.claude_json_path(paths),
            serde_json::to_string_pretty(v).unwrap(),
        )
        .unwrap();
    }

    fn load(p: &Profile, paths: &Paths) -> Value {
        serde_json::from_str(&fs::read_to_string(p.claude_json_path(paths)).unwrap()).unwrap()
    }

    fn folders(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn add_trust_sets_flag_or_adds_default_entry() {
        let mut v = serde_json::json!({
            "numStartups": 3,
            "projects": {
                "/a": { "allowedTools": ["x"], TRUST_KEY: false, "lastCost": 1.5 },
                "/b": { TRUST_KEY: true },
                "/odd": 7
            },
            "oauthAccount": {}
        });
        let n = add_trust(&mut v, &folders(&["/a", "/b", "/c", "/odd"]));
        assert_eq!(n, 2);
        let p = v["projects"].as_object().unwrap();
        // Order kept, the new folder at the end.
        assert_eq!(p.keys().collect::<Vec<_>>(), ["/a", "/b", "/odd", "/c"]);
        assert_eq!(
            p["/a"],
            serde_json::json!({ "allowedTools": ["x"], TRUST_KEY: true, "lastCost": 1.5 })
        );
        assert_eq!(
            p["/a"].as_object().unwrap().keys().collect::<Vec<_>>(),
            ["allowedTools", TRUST_KEY, "lastCost"]
        );
        assert_eq!(p["/c"], new_entry());
        assert_eq!(p["/odd"], 7);
        let top: Vec<_> = v.as_object().unwrap().keys().collect();
        assert_eq!(top, ["numStartups", "projects", "oauthAccount"]);
        assert_eq!(trusted(&v), folders(&["/a", "/b", "/c"]));
    }

    #[test]
    fn add_trust_without_projects() {
        let mut v = serde_json::json!({ "numStartups": 1 });
        assert_eq!(add_trust(&mut v, &folders(&["/a"])), 1);
        assert_eq!(v["projects"]["/a"][TRUST_KEY], true);
        let mut v = serde_json::json!([1]);
        assert_eq!(add_trust(&mut v, &folders(&["/a"])), 0);
    }

    #[test]
    fn sync_takes_the_union_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, ps) = set(&[("main", true), ("a", false), ("b", false)], dir.path());
        let (main, a, b) = (&ps[0], &ps[1], &ps[2]);
        assert_eq!(
            main.claude_json_path(&paths),
            paths.home.join(".claude.json")
        );
        write(
            main,
            &paths,
            &serde_json::json!({ "projects": {
                "/w/one": { "allowedTools": ["Bash"], TRUST_KEY: true },
                "/w/no": { TRUST_KEY: false }
            }}),
        );
        write(
            b,
            &paths,
            &serde_json::json!({ "projects": { "/w/two": { TRUST_KEY: true } } }),
        );
        // Unsorted keys and floats that a lossy parser would change.
        let text = r#"{
  "zeta": 1,
  "cost": 0.00045799999999999997,
  "projects": {
    "/w/one": {
      "lastCost": 2.6180120000000002,
      "hasTrustDialogAccepted": false
    },
    "/w/own": {
      "hasTrustDialogAccepted": true
    }
  },
  "alpha": "é ☃"
}"#;
        let a_file = a.claude_json_path(&paths);
        fs::write(&a_file, text).unwrap();

        assert_eq!(sync(&paths, a, &ps), 2);
        let after = fs::read_to_string(&a_file).unwrap();
        let want = text
            .replacen(
                "\"hasTrustDialogAccepted\": false",
                "\"hasTrustDialogAccepted\": true",
                1,
            )
            .replacen(
                "      \"hasTrustDialogAccepted\": true\n    }\n  },",
                &format!(
                    "      \"hasTrustDialogAccepted\": true\n    }},\n    \"/w/two\": {}\n  }},",
                    serde_json::to_string_pretty(&new_entry())
                        .unwrap()
                        .replace('\n', "\n    ")
                ),
                1,
            );
        assert_eq!(after, want);
        // Never removes trust; the sources are untouched.
        assert!(trusted(&load(a, &paths)).contains("/w/own"));
        assert_eq!(trusted(&load(main, &paths)), folders(&["/w/one"]));
        assert!(!a_file.with_extension("json.lock").exists());

        // Nothing left to add: no write.
        let mtime = fs::metadata(&a_file).unwrap().modified().unwrap();
        assert_eq!(sync(&paths, a, &ps), 0);
        assert_eq!(fs::metadata(&a_file).unwrap().modified().unwrap(), mtime);
        assert_eq!(status(&paths, a, &ps), Some((3, 0)));
        // The default install gets the rest.
        assert_eq!(status(&paths, main, &ps), Some((1, 2)));
        assert_eq!(sync(&paths, main, &ps), 2);
        assert_eq!(
            trusted(&load(main, &paths)),
            folders(&["/w/one", "/w/two", "/w/own"])
        );
    }

    #[test]
    fn bad_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, ps) = set(&[("a", false), ("bad", false), ("none", false)], dir.path());
        let (a, bad, none) = (&ps[0], &ps[1], &ps[2]);
        write(
            a,
            &paths,
            &serde_json::json!({ "projects": { "/x": { TRUST_KEY: true } } }),
        );
        fs::write(bad.claude_json_path(&paths), "{ not json").unwrap();
        // A broken source is ignored; a broken target is left as it is.
        assert_eq!(sync(&paths, bad, &ps), 0);
        assert_eq!(
            fs::read_to_string(bad.claude_json_path(&paths)).unwrap(),
            "{ not json"
        );
        assert_eq!(status(&paths, bad, &ps), None);
        // A missing target is left to Claude.
        assert_eq!(sync(&paths, none, &ps), 0);
        assert!(!none.claude_json_path(&paths).exists());
        assert_eq!(sync(&paths, a, &ps), 0);
        // Codex accounts are never touched.
        let mut codex = a.clone();
        codex.tool = Tool::Codex;
        assert_eq!(sync(&paths, &codex, &ps), 0);
    }

    #[test]
    fn waits_for_claude_lock_and_takes_a_stale_one() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, ps) = set(&[("a", false), ("b", false)], dir.path());
        write(
            &ps[0],
            &paths,
            &serde_json::json!({ "projects": { "/x": { TRUST_KEY: true } } }),
        );
        write(&ps[1], &paths, &serde_json::json!({ "projects": {} }));
        let lock = ps[1].dir.join(".claude.json.lock");
        fs::create_dir(&lock).unwrap();
        assert_eq!(sync(&paths, &ps[1], &ps), 0);
        assert!(lock.is_dir(), "a live lock is not ours to remove");
        assert!(trusted(&load(&ps[1], &paths)).is_empty());

        let old = std::time::SystemTime::now() - Duration::from_secs(60);
        fs::File::open(&lock).unwrap().set_modified(old).unwrap();
        assert_eq!(sync(&paths, &ps[1], &ps), 1);
        assert!(!lock.exists());
    }

    #[test]
    fn writes_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, ps) = set(&[("a", false), ("b", false)], dir.path());
        write(
            &ps[0],
            &paths,
            &serde_json::json!({ "projects": { "/x": { TRUST_KEY: true } } }),
        );
        let real = dir.path().join("real.json");
        fs::write(&real, "{}").unwrap();
        let link = ps[1].claude_json_path(&paths);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(sync(&paths, &ps[1], &ps), 1);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(fs::read_to_string(&real).unwrap().contains("\"/x\""));
    }
}
