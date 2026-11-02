//! `settings.json` across Claude accounts (DESIGN §4.2).
//!
//! An account's `settings.json` is a symlink to the common one, so a
//! setting changed in one account (`/effort`, `/model`, `/config`, a
//! plugin) holds in all of them. Some of Claude's writes replace the link
//! with a file of the account's own (Claude Code 2.1.x stages them in
//! `<config dir>/.cc-writes` and renames over the link); from then on the
//! accounts drift apart.
//!
//! Before Claude starts, [`sync`] folds such a file back into the common
//! one and links it again. The merge is three-way, against the common file
//! as it was at the last sync that found the account linked (its base): a
//! value only the account changed takes the account's, a value only common
//! changed keeps common's, and a value both changed takes the newer file's.
//! An account file with no base (it drifted before agp kept one) is left
//! alone and reported: there is no telling which side changed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::Config;
use crate::fsutil::{FileLock, write_atomic};
use crate::model::Tool;
use crate::paths::Paths;
use crate::profile::Profile;
use crate::share::{ShareClass, classify};

pub const SETTINGS: &str = "settings.json";

/// How long [`sync`] waits for another agp's sync before it skips.
const LOCK_WAIT: Duration = Duration::from_secs(1);

/// What a [`sync`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Accounts whose file went into common, with the paths it changed
    /// there (`modelSettings/claude-opus-5-5/effortLevel`).
    pub merged: Vec<(String, Vec<String>)>,
    /// Accounts whose file had no base: left as they are.
    pub drifted: Vec<String>,
}

/// The bases, in `<state>/settings-base.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Bases {
    /// The common file they belong to.
    common: PathBuf,
    /// Account id → common `settings.json` at the last sync that found the
    /// account linked.
    profiles: BTreeMap<String, Value>,
}

fn bases_path(paths: &Paths) -> PathBuf {
    paths.state_dir.join("settings-base.json")
}

/// The bases kept for `common`, if any.
fn load_bases(paths: &Paths, common: &Path) -> BTreeMap<String, Value> {
    std::fs::read(bases_path(paths))
        .ok()
        .and_then(|b| serde_json::from_slice::<Bases>(&b).ok())
        .filter(|b| b.common == common)
        .map(|b| b.profiles)
        .unwrap_or_default()
}

/// The JSON object in `path`; `None` (logged unless missing) otherwise.
fn read_object(path: &Path) -> Option<Value> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            log::warn!("settings sync: cannot read {}: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) if v.is_object() => Some(v),
        _ => {
            log::warn!("settings sync: {} is not a JSON object", path.display());
            None
        }
    }
}

fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// How an account's `settings.json` stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// A link to the common file.
    Linked,
    /// A file of its own (`differs`: the top-level keys that differ from
    /// common; `base`: [`sync`] can merge it).
    Own { differs: Vec<String>, base: bool },
    /// Missing, a link elsewhere, or unreadable: not agp's to manage.
    Other,
}

/// The state of `profile`'s `settings.json` (for `doctor`).
pub fn state(paths: &Paths, profile: &Profile) -> State {
    if profile.is_default_install || profile.dir == profile.common_home {
        // Its settings.json is the common one.
        return State::Linked;
    }
    let common = profile.common_home.join(SETTINGS);
    let file = profile.dir.join(SETTINGS);
    let Ok(meta) = std::fs::symlink_metadata(&file) else {
        return State::Other;
    };
    if meta.file_type().is_symlink() {
        return if same_file(&file, &common) {
            State::Linked
        } else {
            State::Other
        };
    }
    let (Some(own), Some(c)) = (read_object(&file), read_object(&common)) else {
        return State::Other;
    };
    let (own, c) = (own.as_object().unwrap(), c.as_object().unwrap());
    let differs = own
        .keys()
        .chain(c.keys().filter(|k| !own.contains_key(*k)))
        .filter(|k| own.get(*k) != c.get(*k))
        .cloned()
        .collect();
    let base = load_bases(paths, &common).contains_key(&profile.id);
    State::Own { differs, base }
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// Before Claude starts: [`sync_profiles`] for the Claude accounts of
/// `config`, unless `share.sync_settings` is off or the share map keeps
/// `settings.json` per account (`share.extra_local`).
pub fn sync(paths: &Paths, config: &Config) -> Report {
    if !config.share.sync_settings || !shared(config) {
        return Report::default();
    }
    let common = config.common_home_for(paths, Tool::Claude);
    sync_profiles(paths, &common, &config.profiles_for(paths, Tool::Claude))
}

/// Whether the share map shares `settings.json` between Claude accounts.
pub fn shared(config: &Config) -> bool {
    classify(SETTINGS, &config.share_rules(Tool::Claude)) == ShareClass::Shared
}

/// Folds the `settings.json` files that Claude accounts in `profiles` own
/// into `common` (`~/.claude`) and links them again; records the bases.
/// Errors are logged: a launch never fails for this.
pub fn sync_profiles(paths: &Paths, common: &Path, profiles: &[Profile]) -> Report {
    let lock = paths.state_dir.join("settings-sync.lock");
    match FileLock::acquire(&lock, LOCK_WAIT) {
        Ok(Some(_l)) => match sync_locked(paths, common, profiles) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("settings sync: {e:#}");
                Report::default()
            }
        },
        Ok(None) => {
            log::info!("settings sync: another agp is syncing; skipped");
            Report::default()
        }
        Err(e) => {
            log::warn!("settings sync: lock {}: {e}", lock.display());
            Report::default()
        }
    }
}

fn sync_locked(paths: &Paths, common: &Path, profiles: &[Profile]) -> Result<Report> {
    let mut report = Report::default();
    let file = common.join(SETTINGS);
    let Some(start) = read_object(&file) else {
        return Ok(report);
    };
    let old = load_bases(paths, &file);
    let mut linked = Vec::new();
    let mut own = Vec::new();
    // Missing, or a link elsewhere: not agp's, and a base would go stale.
    let mut other = Vec::new();
    for p in profiles {
        if p.tool != Tool::Claude || p.is_default_install || p.dir == common {
            continue;
        }
        let f = p.dir.join(SETTINGS);
        match std::fs::symlink_metadata(&f) {
            Ok(m) if m.file_type().is_symlink() && same_file(&f, &file) => linked.push(p),
            Ok(m) if m.is_file() => own.push((p, f.clone(), mtime(&f))),
            _ => other.push(p),
        }
    }
    // The oldest first, so that of two accounts that changed the same
    // value the newer one wins.
    own.sort_by_key(|(_, _, t)| *t);
    let mut c = start.clone();
    let mut c_time = mtime(&file);
    // (account, its file, the content read): to back up and link.
    let mut fold: Vec<(&Profile, PathBuf, Value)> = Vec::new();
    for (p, f, t) in own {
        let Some(mine) = read_object(&f) else {
            continue;
        };
        if mine != c {
            let Some(base) = old.get(&p.id) else {
                log::warn!(
                    "settings sync: {} has its own settings.json from before agp kept a base; \
                     left alone (agp doctor)",
                    p.id
                );
                report.drifted.push(p.id.clone());
                continue;
            };
            let merged = merge3(Some(base), Some(&mine), Some(&c), t >= c_time)
                .unwrap_or_else(|| Value::Object(Map::new()));
            let mut changed = Vec::new();
            diff_paths(Some(&c), Some(&merged), "", &mut changed);
            if !changed.is_empty() {
                c_time = c_time.max(t);
            }
            report.merged.push((p.id.clone(), changed));
            c = merged;
        }
        fold.push((p, f, mine));
    }
    // The files replaced: the common one before a merge and each account's.
    let backups = paths.state_dir.join("settings-sync");
    let stamp = jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S").to_string();
    if c != start {
        // Claude may have written common meanwhile: try again next launch.
        if read_object(&file).as_ref() != Some(&start) {
            log::info!(
                "settings sync: {} changed meanwhile; skipped",
                file.display()
            );
            return Ok(Report::default());
        }
        let backup = backups.join(format!("{stamp}-common.json"));
        std::fs::create_dir_all(&backups)
            .and_then(|_| std::fs::copy(&file, &backup))
            .with_context(|| format!("backing up {}", file.display()))?;
        write_settings(&file, &c)?;
    }
    for (p, f, mine) in fold {
        // A newer write by Claude in that account: the next sync takes it.
        if read_object(&f).as_ref() != Some(&mine) {
            continue;
        }
        let backup = backups.join(format!("{stamp}-{}.json", p.id));
        let relinked = std::fs::create_dir_all(&backups)
            .and_then(|_| std::fs::copy(&f, &backup))
            .with_context(|| format!("backing up {}", f.display()))
            .and_then(|_| relink(&f, &file));
        match relinked {
            Ok(()) => {
                log::info!(
                    "settings sync: {} linked again (its file: {})",
                    p.id,
                    backup.display()
                );
                linked.push(p);
            }
            Err(e) => log::warn!("settings sync: {}: {e:#}", p.id),
        }
    }
    for (id, changed) in &report.merged {
        if !changed.is_empty() {
            log::info!("settings sync: from {id}: {}", changed.join(", "));
        }
    }
    // Accounts this sync did not look at (`--accounts`) or left as they
    // were keep their base.
    let mut profiles = old;
    for p in other {
        profiles.remove(&p.id);
    }
    for p in linked {
        profiles.insert(p.id.clone(), c.clone());
    }
    let bases = Bases {
        common: file,
        profiles,
    };
    write_atomic(&bases_path(paths), &serde_json::to_vec(&bases)?)?;
    Ok(report)
}

/// Writes `v` to `file` (through a symlink) the way Claude does: two-space
/// JSON and a newline, with the file's mode kept.
fn write_settings(file: &Path, v: &Value) -> Result<()> {
    let real = file
        .canonicalize()
        .with_context(|| format!("resolving {}", file.display()))?;
    let perms = std::fs::metadata(&real)?.permissions();
    let mut text = serde_json::to_string_pretty(v)?;
    text.push('\n');
    write_atomic(&real, text.as_bytes())?;
    std::fs::set_permissions(&real, perms)?;
    Ok(())
}

/// Replaces the file `f` with a link to `target`, in one rename.
fn relink(f: &Path, target: &Path) -> Result<()> {
    let tmp = f.with_file_name(".settings.json.agp-link");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)
        .with_context(|| format!("linking {}", tmp.display()))?;
    std::fs::rename(&tmp, f).with_context(|| format!("replacing {}", f.display()))?;
    Ok(())
}

/// The three-way merge of a JSON value: `None` is absent. Objects merge
/// key by key; anything else changed on both sides takes `ours` when
/// `prefer_ours`, else `theirs`.
fn merge3(
    base: Option<&Value>,
    ours: Option<&Value>,
    theirs: Option<&Value>,
    prefer_ours: bool,
) -> Option<Value> {
    if ours == theirs || ours == base {
        return theirs.cloned();
    }
    if theirs == base {
        return ours.cloned();
    }
    if let (Some(Value::Object(o)), Some(Value::Object(t))) = (ours, theirs) {
        let b = base.and_then(Value::as_object);
        let mut out = Map::new();
        for k in t.keys().chain(o.keys().filter(|k| !t.contains_key(*k))) {
            if let Some(v) = merge3(b.and_then(|b| b.get(k)), o.get(k), t.get(k), prefer_ours) {
                out.insert(k.clone(), v);
            }
        }
        return Some(Value::Object(out));
    }
    if prefer_ours {
        ours.cloned()
    } else {
        theirs.cloned()
    }
}

/// The paths (`a/b/c`) where `a` and `b` differ, objects compared key by key.
fn diff_paths(a: Option<&Value>, b: Option<&Value>, at: &str, out: &mut Vec<String>) {
    if a == b {
        return;
    }
    if let (Some(Value::Object(x)), Some(Value::Object(y))) = (a, b) {
        for k in x.keys().chain(y.keys().filter(|k| !x.contains_key(*k))) {
            let p = if at.is_empty() {
                k.clone()
            } else {
                format!("{at}/{k}")
            };
            diff_paths(x.get(k), y.get(k), &p, out);
        }
        return;
    }
    out.push(at.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn merge(base: Value, ours: Value, theirs: Value, prefer_ours: bool) -> Value {
        merge3(Some(&base), Some(&ours), Some(&theirs), prefer_ours).unwrap()
    }

    #[test]
    fn three_way() {
        let base = json!({"a": 1, "m": {"x": "low", "y": "low"}, "gone": true});
        // The account changed m.x and dropped `gone`; common changed m.y
        // and added b.
        let ours = json!({"a": 1, "m": {"x": "high", "y": "low"}});
        let theirs = json!({"a": 1, "m": {"x": "low", "y": "max"}, "gone": true, "b": 2});
        assert_eq!(
            merge(base.clone(), ours.clone(), theirs.clone(), false),
            json!({"a": 1, "m": {"x": "high", "y": "max"}, "b": 2})
        );
        // Both changed one value: the newer side wins.
        let theirs = json!({"a": 1, "m": {"x": "medium", "y": "low"}, "gone": true});
        assert_eq!(
            merge(base.clone(), ours.clone(), theirs.clone(), true)["m"]["x"],
            "high"
        );
        assert_eq!(merge(base, ours, theirs, false)["m"]["x"], "medium");
    }

    fn profile(root: &Path, id: &str) -> Profile {
        Profile::bare(id, Tool::Claude, root.join(id), root.join("common"))
    }

    struct Setup {
        _dir: tempfile::TempDir,
        paths: Paths,
        common: PathBuf,
        a: Profile,
        b: Profile,
    }

    fn setup(settings: Value) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let paths = Paths::under(&root);
        let common = root.join("common");
        std::fs::create_dir_all(&common).unwrap();
        write_settings_new(&common.join(SETTINGS), &settings);
        let (a, b) = (profile(&root, "a"), profile(&root, "b"));
        for p in [&a, &b] {
            std::fs::create_dir_all(&p.dir).unwrap();
            std::os::unix::fs::symlink(common.join(SETTINGS), p.dir.join(SETTINGS)).unwrap();
        }
        Setup {
            _dir: dir,
            paths,
            common,
            a,
            b,
        }
    }

    fn write_settings_new(f: &Path, v: &Value) {
        std::fs::write(f, serde_json::to_string_pretty(v).unwrap() + "\n").unwrap();
    }

    fn read(f: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(f).unwrap()).unwrap()
    }

    /// What Claude's staged write does: a new file over the link.
    fn break_link(p: &Profile, v: &Value) {
        let f = p.dir.join(SETTINGS);
        std::fs::remove_file(&f).unwrap();
        write_settings_new(&f, v);
    }

    fn is_link(p: &Profile) -> bool {
        std::fs::symlink_metadata(p.dir.join(SETTINGS))
            .unwrap()
            .file_type()
            .is_symlink()
    }

    #[test]
    fn folds_a_drifted_account_back() {
        let s =
            setup(json!({"theme": "dark", "modelSettings": {"opus": {"effortLevel": "medium"}}}));
        let all = [s.a.clone(), s.b.clone()];
        // A first sync records the bases.
        assert_eq!(sync_profiles(&s.paths, &s.common, &all), Report::default());
        assert_eq!(state(&s.paths, &s.a), State::Linked);
        // `/effort high` in account a breaks its link; then common changes
        // the theme.
        break_link(
            &s.a,
            &json!({"theme": "dark", "modelSettings": {"opus": {"effortLevel": "high"}}}),
        );
        assert_eq!(
            state(&s.paths, &s.a),
            State::Own {
                differs: vec!["modelSettings".into()],
                base: true
            }
        );
        let c = s.common.join(SETTINGS);
        write_settings_new(
            &c,
            &json!({"theme": "light", "modelSettings": {"opus": {"effortLevel": "medium"}}}),
        );
        let r = sync_profiles(&s.paths, &s.common, &all);
        assert_eq!(
            r.merged,
            vec![(
                "a".to_string(),
                vec!["modelSettings/opus/effortLevel".to_string()]
            )]
        );
        assert_eq!(
            read(&c),
            json!({"theme": "light", "modelSettings": {"opus": {"effortLevel": "high"}}})
        );
        assert!(is_link(&s.a) && is_link(&s.b));
        assert_eq!(
            std::fs::read_to_string(&c).unwrap().chars().last(),
            Some('\n')
        );
        // Both files it replaced are kept.
        let mut kept: Vec<String> = std::fs::read_dir(s.paths.state_dir.join("settings-sync"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        kept.sort();
        assert!(
            kept.len() == 2 && kept[0].ends_with("-a.json") && kept[1].ends_with("-common.json"),
            "{kept:?}"
        );
    }

    #[test]
    fn leaves_a_file_without_a_base() {
        let s = setup(json!({"tui": "fullscreen"}));
        let all = [s.a.clone(), s.b.clone()];
        break_link(&s.a, &json!({"tui": "default"}));
        // No sync saw `a` linked: agp can't tell which side changed.
        let r = sync_profiles(&s.paths, &s.common, &all);
        assert_eq!(r.drifted, vec!["a".to_string()]);
        assert!(!is_link(&s.a));
        assert_eq!(read(&s.common.join(SETTINGS)), json!({"tui": "fullscreen"}));
        assert_eq!(
            state(&s.paths, &s.a),
            State::Own {
                differs: vec!["tui".into()],
                base: false
            }
        );
        // Still none on the next sync.
        assert_eq!(
            sync_profiles(&s.paths, &s.common, &all).drifted,
            vec!["a".to_string()]
        );
        // A copy that matches common just goes back to a link.
        break_link(&s.a, &json!({"tui": "fullscreen"}));
        assert_eq!(sync_profiles(&s.paths, &s.common, &all), Report::default());
        assert!(is_link(&s.a));
    }

    /// A sync over some accounts (`--accounts a`) keeps the others' bases.
    #[test]
    fn a_narrow_sync_keeps_other_bases() {
        let s = setup(json!({"a": 1}));
        sync_profiles(&s.paths, &s.common, &[s.a.clone(), s.b.clone()]);
        sync_profiles(&s.paths, &s.common, std::slice::from_ref(&s.a));
        break_link(&s.b, &json!({"a": 2}));
        let r = sync_profiles(&s.paths, &s.common, &[s.a.clone(), s.b.clone()]);
        assert_eq!(r.merged, vec![("b".to_string(), vec!["a".to_string()])]);
        assert!(is_link(&s.b));
    }

    #[test]
    fn follows_the_share_map() {
        let mut c = Config::default();
        assert!(shared(&c));
        c.share.extra_local = vec![SETTINGS.into()];
        assert!(!shared(&c));
    }

    #[test]
    fn keeps_a_linked_common_file() {
        // Common's settings.json is itself a link (a dotfiles repo).
        let s = setup(json!({"a": 1}));
        let real = s.common.join("dotfiles.json");
        std::fs::rename(s.common.join(SETTINGS), &real).unwrap();
        std::os::unix::fs::symlink(&real, s.common.join(SETTINGS)).unwrap();
        let all = [s.a.clone()];
        sync_profiles(&s.paths, &s.common, &all);
        break_link(&s.a, &json!({"a": 2}));
        sync_profiles(&s.paths, &s.common, &all);
        assert_eq!(read(&real), json!({"a": 2}));
        assert!(
            std::fs::symlink_metadata(s.common.join(SETTINGS))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
