//! The `--settings` files of background sessions (DESIGN §11d).
//!
//! Claude Code saves a job's launch flags (`respawnFlags`) and reuses them
//! on `claude respawn` and on a wake. When agent-prism merges settings
//! (overlay, memory dir, the user's `--settings` file) the merge goes to a
//! file per session and profile, and the flags name that file, so it can
//! be written again from the user's file (`agp bg watch`, every resume)
//! and a revived session reads current hooks instead of a frozen copy.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use prism_core::paths::Paths;
use prism_core::profile::Profile;

use super::record;
use crate::launch;

/// `<state>/bg/settings/<key>-<profile id>.json`; `key` is the launch's
/// (`launch.key` in the record).
pub fn file(paths: &Paths, key: &str, profile: &str) -> PathBuf {
    dir(paths).join(format!("{key}-{profile}.json"))
}

fn dir(paths: &Paths) -> PathBuf {
    record::dir(paths).join("settings")
}

fn inline(value: &str) -> bool {
    value.trim_start().starts_with('{')
}

/// A launch key: unique per `agp --bg`, known before claude prints the
/// session id.
pub fn new_key() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:x}{:x}", std::process::id())
}

/// The user's `--settings` value as the record keeps it: a relative path
/// made absolute against the launch folder (the watcher runs elsewhere).
pub fn user_value(value: Option<String>, cwd: &Path) -> Option<String> {
    value.map(|v| {
        if inline(&v) || Path::new(&v).is_absolute() {
            v
        } else {
            cwd.join(&v).to_string_lossy().into_owned()
        }
    })
}

/// Print mode's `--settings` value for `profile` (§7.4).
fn merged(paths: &Paths, profile: &Profile, cwd: &Path, user: Option<&str>) -> Option<String> {
    let overlay = paths.profile_overlay(&profile.id);
    let overlay = overlay.is_file().then_some(overlay);
    crate::session::print_settings(user.map(str::to_string), overlay.as_deref(), |u, o| {
        launch::memory_dir(profile, cwd, &[u, o], |k| std::env::var(k).ok())
    })
}

/// The `--settings` value of a launch or a flags-form resume in `profile`:
/// print mode's, except that settings agent-prism merged (inline JSON it
/// made) go to the session's file for `profile` and the value is that
/// file's path. A user value that is inline JSON, or nothing to merge (the
/// user's file or the overlay alone), stays as print mode passes it.
pub fn value(
    paths: &Paths,
    key: &str,
    profile: &Profile,
    cwd: &Path,
    user: Option<&str>,
) -> Option<String> {
    let v = merged(paths, profile, cwd, user)?;
    if !inline(&v) || user.is_some_and(inline) {
        return Some(v);
    }
    let f = file(paths, key, &profile.id);
    match record::write_atomic(&f, v.as_bytes()) {
        Ok(()) => Some(f.to_string_lossy().into_owned()),
        Err(e) => {
            log::warn!(
                "bg: writing {}: {e:#}; passing the settings inline",
                f.display()
            );
            Some(v)
        }
    }
}

/// Writes the session's file for `profile` again (the user's file read
/// now, the overlay, the memory dir) if that profile's job names it.
/// `Ok(false)`: there is no such file. A user file that can't be read or
/// merged leaves the old file as it is.
pub fn refresh(
    paths: &Paths,
    key: &str,
    profile: &Profile,
    cwd: &Path,
    user: Option<&str>,
) -> Result<bool> {
    let f = file(paths, key, &profile.id);
    if !f.is_file() {
        return Ok(false);
    }
    let text = match merged(paths, profile, cwd, user) {
        Some(v) if inline(&v) => v,
        // Nothing to merge any more: the user's file or the overlay alone.
        Some(path) => std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?,
        None => "{}".to_string(),
    };
    let v: serde_json::Value = serde_json::from_str(&text).context("the settings are not JSON")?;
    anyhow::ensure!(v.is_object(), "the settings are not a JSON object");
    record::write_atomic(&f, text.as_bytes())?;
    Ok(true)
}

/// Removes a launch's files (its record is gone).
pub fn remove(paths: &Paths, key: &str) {
    let Ok(rd) = std::fs::read_dir(dir(paths)) else {
        return;
    };
    let prefix = format!("{key}-");
    for e in rd.flatten() {
        if e.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_core::model::Tool;

    #[test]
    fn files_for_merged_settings_only() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let home = tmp.path().join("home");
        let p = Profile::bare(
            "a",
            Tool::Claude,
            home.join(".claude-a"),
            home.join(".claude"),
        );
        let cwd = tmp.path().join("work");
        let user = tmp.path().join("user.json");
        std::fs::write(&user, r#"{"hooks":{"Stop":"v1"}}"#).unwrap();
        let user = user.to_string_lossy().into_owned();

        // Nothing to merge: the user's file, as print mode passes it.
        assert_eq!(
            value(&paths, "k", &p, &cwd, Some(&user)),
            Some(user.clone())
        );
        assert_eq!(value(&paths, "k", &p, &cwd, None), None);
        assert!(!refresh(&paths, "k", &p, &cwd, Some(&user)).unwrap());

        // With an overlay: a file of the session's, then written again.
        let overlay = paths.profile_overlay("a");
        record::write_atomic(&overlay, br#"{"env":{"X":"1"}}"#).unwrap();
        let f = file(&paths, "k", "a");
        assert_eq!(
            value(&paths, "k", &p, &cwd, Some(&user)),
            Some(f.to_string_lossy().into_owned())
        );
        let read = || -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap()
        };
        assert_eq!(read()["hooks"]["Stop"], "v1");
        assert_eq!(read()["env"]["X"], "1");
        std::fs::write(&user, r#"{"hooks":{"Stop":"v2"}}"#).unwrap();
        assert!(refresh(&paths, "k", &p, &cwd, Some(&user)).unwrap());
        assert_eq!(read()["hooks"]["Stop"], "v2");
        // A broken user file leaves the last good one.
        std::fs::write(&user, "{nope").unwrap();
        assert!(refresh(&paths, "k", &p, &cwd, Some(&user)).is_err());
        assert_eq!(read()["hooks"]["Stop"], "v2");

        // Inline user JSON stays inline (merged).
        let v = value(&paths, "k2", &p, &cwd, Some(r#"{"a":1}"#)).unwrap();
        assert!(v.starts_with('{'), "{v}");
        assert!(!file(&paths, "k2", "a").exists());

        remove(&paths, "k");
        assert!(!f.exists());
    }

    #[test]
    fn user_values() {
        let cwd = Path::new("/w");
        assert_eq!(
            user_value(Some("s.json".into()), cwd).as_deref(),
            Some("/w/s.json")
        );
        assert_eq!(
            user_value(Some("/x.json".into()), cwd).as_deref(),
            Some("/x.json")
        );
        assert_eq!(user_value(Some(" {}".into()), cwd).as_deref(), Some(" {}"));
        assert_eq!(user_value(None, cwd), None);
    }
}
