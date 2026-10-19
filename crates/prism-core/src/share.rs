//! The share map and adopt (DESIGN §4.2, §4.3, §4.6 step 2).
//!
//! Walking never follows symlinks: entries are inspected with
//! `symlink_metadata`, and a symlink is moved or compared as a link.

use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::ShareRules;
use crate::fsutil::write_atomic;
use crate::model::Tool;

/// The class of a top-level entry of a profile dir.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareClass {
    /// A symlink into the common home.
    Shared,
    /// A real file/dir of the profile.
    Local,
    /// agent-prism never touches it.
    Unmanaged,
}

const SHARED: &[&str] = &[
    "*.md",
    "settings.json",
    "settings.local.json",
    "keybindings.json",
    "agents",
    "commands",
    "skills",
    "hooks",
    "output-styles",
    "rules",
    "plugins",
    "statusline",
    "statusline*.sh",
    "superpowers",
    "dev-mods",
    "projects",
    "file-history",
    "todos",
    "tasks",
    "teams",
    "plans",
    "history.jsonl",
    "paste-cache",
    "shell-snapshots",
    "session-env",
    "uploads",
    "downloads",
];

const LOCAL: &[&str] = &[
    ".claude.json",
    ".credentials.json",
    ".oauth_refresh.lock*",
    "policy-limits.json*",
    "remote-settings.json*",
    "daemon",
    "daemon.*",
    "sessions",
    "jobs",
    "cache",
    ".cache",
    "backups",
    "stats-cache.json",
    "mcp-needs-auth-cache.json",
    "statsig",
    "telemetry",
    ".last-*",
    "chrome",
    "ide",
];

/// Local entries that are dirs (created empty for a new profile).
const LOCAL_DIRS: &[&str] = &["daemon", "sessions", "jobs", "cache", ".cache", "backups"];

const UNMANAGED: &[&str] = &["*.bak*", "*.premerge"];

/// Codex (`CODEX_HOME`): config, instructions, skills, past sessions and
/// memories are shared. The thread databases are shared by agent-prism with
/// `CODEX_SQLITE_HOME`, not by links.
const CODEX_SHARED: &[&str] = &[
    "*.md",
    "config.toml",
    "hooks.json",
    "prompts",
    "skills",
    "rules",
    "plugins",
    "vendor_imports",
    "pets",
    "memories",
    "memories_v2",
    "sessions",
    "archived_sessions",
    "session_index.jsonl",
    "history.jsonl",
    "thread-writer-locks",
    "automations",
    "scheduled-tasks",
    "attachments",
    "generated_images",
    ".personality_migration",
    ".sandbox_migration",
];

const CODEX_LOCAL: &[&str] = &[
    "auth.json",
    ".credentials.json",
    "models_cache.json",
    "version.json",
    "installation_id",
    "cache",
    ".tmp",
    "tmp",
    "log",
    "app-server-daemon",
    "app-server-control",
    "ipc",
    "mcp-oauth-locks",
    "*.sqlite",
    "*.sqlite-*",
    ".sqlite-maintenance.lock",
    ".codex-global-state.json*",
];

const CODEX_UNMANAGED: &[&str] = &["*.bak*", "*.backup*", "*.premerge", "packages"];

/// The built-in lists of one tool.
struct Lists {
    shared: &'static [&'static str],
    local: &'static [&'static str],
    local_dirs: &'static [&'static str],
    unmanaged: &'static [&'static str],
}

fn lists(tool: Tool) -> Lists {
    match tool {
        Tool::Claude => Lists {
            shared: SHARED,
            local: LOCAL,
            local_dirs: LOCAL_DIRS,
            unmanaged: UNMANAGED,
        },
        Tool::Codex => Lists {
            shared: CODEX_SHARED,
            local: CODEX_LOCAL,
            local_dirs: &[],
            unmanaged: CODEX_UNMANAGED,
        },
    }
}

/// The shared entry that holds past sessions; `--resume` after a switch
/// needs it linked.
pub fn sessions_entry(tool: Tool) -> &'static str {
    match tool {
        Tool::Claude => "projects",
        Tool::Codex => "sessions",
    }
}

/// Match a pattern with `*` wildcards against a whole name.
fn glob(pat: &str, name: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

fn any_glob(pats: &[impl AsRef<str>], name: &str) -> bool {
    pats.iter().any(|p| glob(p.as_ref(), name))
}

/// Classify a top-level entry name. `extra_local` wins over `extra_shared`,
/// which wins over the built-in lists of the tool.
pub fn classify(name: &str, rules: &ShareRules) -> ShareClass {
    let l = lists(rules.tool);
    if any_glob(&rules.extra.extra_local, name) {
        ShareClass::Local
    } else if any_glob(&rules.extra.extra_shared, name) {
        ShareClass::Shared
    } else if any_glob(l.unmanaged, name) {
        ShareClass::Unmanaged
    } else if any_glob(l.local, name) {
        ShareClass::Local
    } else if any_glob(l.shared, name) {
        ShareClass::Shared
    } else {
        ShareClass::Unmanaged
    }
}

/// What merging one entry would do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePreview {
    /// Files (or whole subtrees) moved into common.
    pub move_files: usize,
    /// Profile copies dropped because common has the same bytes.
    pub identical: usize,
    /// Paths (relative to the profile dir) that go to `.agent-prism-conflicts`.
    pub conflicts: Vec<PathBuf>,
    /// Top-level JSON keys saved to the settings overlay.
    pub overlay_keys: Vec<String>,
    /// Differing entries under `projects/<slug>/memory/` (not in
    /// `conflicts`). They go to `.agent-prism-conflicts` like the others; listed
    /// apart because someone must reconcile them by hand.
    pub memory_conflicts: Vec<PathBuf>,
}

/// One adopt action (DESIGN §4.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Link { name: String },
    Merge { name: String, preview: MergePreview },
    Relink { name: String, old_target: PathBuf },
    Localize { name: String, was_dir: bool },
    Skip { name: String, reason: String },
}

const OK: &str = "ok";
const UNMANAGED_REASON: &str = "unmanaged";

/// The adopt plan for one profile. Built read-only by [`plan_adopt`].
#[derive(Clone, Debug, PartialEq)]
pub struct AdoptPlan {
    pub profile_id: String,
    pub tool: Tool,
    pub dir: PathBuf,
    pub common: PathBuf,
    pub overlay_path: PathBuf,
    pub actions: Vec<Action>,
}

impl AdoptPlan {
    /// True if every action is a `Skip`.
    pub fn is_noop(&self) -> bool {
        self.actions
            .iter()
            .all(|a| matches!(a, Action::Skip { .. }))
    }

    /// Short human-readable description, one line per action kind.
    pub fn summary(&self) -> String {
        let mut s = format!("{} ({}):", self.profile_id, self.dir.display());
        if self.is_noop() {
            s.push_str(" nothing to do");
        }
        let names = |f: &dyn Fn(&Action) -> Option<String>| -> Vec<String> {
            self.actions.iter().filter_map(f).collect()
        };
        let link = names(&|a| match a {
            Action::Link { name } => Some(name.clone()),
            _ => None,
        });
        let merge = names(&|a| match a {
            Action::Merge { name, preview: p } => {
                let mut t = format!("{name} [{} move, {} identical", p.move_files, p.identical);
                if !p.memory_conflicts.is_empty() {
                    let _ = write!(t, ", {} memory conflict", p.memory_conflicts.len());
                }
                if !p.conflicts.is_empty() {
                    let _ = write!(t, ", {} conflict", p.conflicts.len());
                }
                if !p.overlay_keys.is_empty() {
                    let _ = write!(t, ", overlay: {}", p.overlay_keys.join(" "));
                }
                t.push(']');
                Some(t)
            }
            _ => None,
        });
        let relink = names(&|a| match a {
            Action::Relink { name, old_target } => {
                Some(format!("{name} (was -> {})", old_target.display()))
            }
            _ => None,
        });
        let localize = names(&|a| match a {
            Action::Localize { name, was_dir } => {
                Some(format!("{name}{}", if *was_dir { "/" } else { "" }))
            }
            _ => None,
        });
        let unmanaged = names(&|a| match a {
            Action::Skip { name, reason } if reason == UNMANAGED_REASON => Some(name.clone()),
            _ => None,
        });
        let ok = names(&|a| match a {
            Action::Skip { name, reason } if reason == OK => Some(name.clone()),
            _ => None,
        });
        for (label, v) in [
            ("link", &link),
            ("merge", &merge),
            ("relink", &relink),
            ("localize", &localize),
            ("unmanaged", &unmanaged),
        ] {
            if !v.is_empty() {
                let _ = write!(s, "\n  {label}: {}", v.join(", "));
            }
        }
        let _ = write!(s, "\n  ok: {}", ok.len());
        s
    }
}

fn lstat(p: &Path) -> Option<fs::Metadata> {
    fs::symlink_metadata(p).ok()
}

fn entry_names(dir: &Path) -> Result<Vec<String>> {
    let mut v = Vec::new();
    match fs::read_dir(dir) {
        Ok(rd) => {
            for e in rd {
                v.push(e?.file_name().to_string_lossy().into_owned());
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    }
    Ok(v)
}

/// True if the symlink at `link` (in `dir`) points at `want`.
fn links_to(link: &Path, dir: &Path, want: &Path) -> Result<(bool, PathBuf)> {
    let t = fs::read_link(link).with_context(|| format!("reading link {}", link.display()))?;
    let abs = if t.is_absolute() {
        t.clone()
    } else {
        dir.join(&t)
    };
    let same = abs == want
        || matches!((abs.canonicalize(), want.canonicalize()), (Ok(a), Ok(b)) if a == b);
    Ok((same, t))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// Build the adopt plan. Read-only.
pub fn plan_adopt(
    profile_id: &str,
    dir: &Path,
    common: &Path,
    rules: &ShareRules,
    overlay_path: &Path,
) -> Result<AdoptPlan> {
    plan_inner(profile_id, dir, common, rules, overlay_path, true)
}

fn plan_inner(
    profile_id: &str,
    dir: &Path,
    common: &Path,
    rules: &ShareRules,
    overlay_path: &Path,
    preview: bool,
) -> Result<AdoptPlan> {
    let mut plan = AdoptPlan {
        profile_id: profile_id.to_string(),
        tool: rules.tool,
        dir: dir.to_path_buf(),
        common: common.to_path_buf(),
        overlay_path: overlay_path.to_path_buf(),
        actions: Vec::new(),
    };
    if same_dir(dir, common) {
        return Ok(plan);
    }
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let mut names: BTreeSet<String> = entry_names(dir)?.into_iter().collect();
    names.extend(
        entry_names(common)?
            .into_iter()
            .filter(|n| classify(n, rules) == ShareClass::Shared),
    );
    for name in names {
        let p = dir.join(&name);
        let c = common.join(&name);
        let meta = lstat(&p);
        let action = match (classify(&name, rules), meta) {
            (ShareClass::Shared, None) => Action::Link { name },
            (ShareClass::Shared, Some(m)) if m.file_type().is_symlink() => {
                let (ok, old_target) = links_to(&p, dir, &c)?;
                if ok {
                    Action::Skip {
                        name,
                        reason: OK.into(),
                    }
                } else {
                    Action::Relink { name, old_target }
                }
            }
            (ShareClass::Shared, Some(_)) => {
                let preview = if preview {
                    merge_preview(rules.tool, &name, &p, &c)?
                } else {
                    MergePreview::default()
                };
                Action::Merge { name, preview }
            }
            (ShareClass::Local, None) => continue,
            (ShareClass::Local, Some(m)) if m.file_type().is_symlink() => {
                let was_dir = match fs::metadata(&p) {
                    Ok(t) => t.is_dir(),
                    Err(_) => lists(rules.tool).local_dirs.contains(&name.as_str()),
                };
                Action::Localize { name, was_dir }
            }
            (ShareClass::Local, Some(_)) => Action::Skip {
                name,
                reason: OK.into(),
            },
            (ShareClass::Unmanaged, None) => continue,
            (ShareClass::Unmanaged, Some(_)) => Action::Skip {
                name,
                reason: UNMANAGED_REASON.into(),
            },
        };
        plan.actions.push(action);
    }
    Ok(plan)
}

/// One step of a merge, relative to the merged entry.
#[derive(Debug)]
enum Op {
    /// Absent in common: move (a file, a link or a whole subtree).
    Move(PathBuf, usize),
    /// Same content in common: drop the profile copy.
    Drop(PathBuf),
    /// Different: move the profile copy to `.agent-prism-conflicts`.
    Conflict(PathBuf),
}

fn count_files(p: &Path) -> Result<usize> {
    let m = fs::symlink_metadata(p)?;
    if !m.is_dir() {
        return Ok(1);
    }
    let mut n = 0;
    for e in fs::read_dir(p)? {
        n += count_files(&e?.path())?;
    }
    Ok(n)
}

fn same_entry(a: &Path, b: &Path) -> Result<bool> {
    let (ma, mb) = (fs::symlink_metadata(a)?, fs::symlink_metadata(b)?);
    if ma.file_type().is_symlink() || mb.file_type().is_symlink() {
        return Ok(ma.file_type().is_symlink()
            && mb.file_type().is_symlink()
            && fs::read_link(a)? == fs::read_link(b)?);
    }
    if !(ma.is_file() && mb.is_file()) || ma.len() != mb.len() {
        return Ok(false);
    }
    Ok(fs::read(a)? == fs::read(b)?)
}

/// True if both paths resolve to the same existing file or dir.
fn same_target(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

/// Ops to merge the profile tree `p` (the entry `name`) into the common
/// tree `c`.
fn dir_ops(name: &str, p: &Path, c: &Path, rel: &Path, ops: &mut Vec<Op>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(p)
        .with_context(|| format!("reading {}", p.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let file = e.file_name();
        let (pp, cp, r) = (e.path(), c.join(&file), rel.join(&file));
        let pm = fs::symlink_metadata(&pp)?;
        if is_memory_store(name, &r) {
            // Never merged into (or compared with) the memory store's repo.
            ops.push(Op::Conflict(r));
            continue;
        }
        match lstat(&cp) {
            None => ops.push(Op::Move(r, count_files(&pp)?)),
            // A profile link to the common entry itself (e.g. a memory dir
            // linked by hand): nothing to merge.
            Some(_) if pm.file_type().is_symlink() && same_target(&pp, &cp) => {
                ops.push(Op::Drop(r))
            }
            Some(cm) if pm.is_dir() && cm.is_dir() => dir_ops(name, &pp, &cp, &r, ops)?,
            Some(_) if !pm.is_dir() && same_entry(&pp, &cp)? => ops.push(Op::Drop(r)),
            Some(_) => ops.push(Op::Conflict(r)),
        }
    }
    Ok(())
}

/// Differing top-level keys if both files are JSON objects.
fn json_diff(p: &Path, c: &Path) -> Option<Map<String, Value>> {
    let read = |f: &Path| -> Option<Map<String, Value>> {
        serde_json::from_slice::<Value>(&fs::read(f).ok()?)
            .ok()?
            .as_object()
            .cloned()
    };
    let (pv, cv) = (read(p)?, read(c)?);
    Some(
        pv.into_iter()
            .filter(|(k, v)| cv.get(k) != Some(v))
            .collect(),
    )
}

/// Entries of a memory dir that belong to the memory store (a local git
/// repo kept by a hook in common). Adopt never merges them.
const MEMORY_STORE: &[&str] = &[".git", ".gitignore", ".memory-store"];

/// True for anything under `projects/<slug>/memory/` (`rel` is relative to
/// the merged entry `name`).
pub fn is_memory_path(name: &str, rel: &Path) -> bool {
    let mut parts = rel.components();
    name == "projects"
        && parts.next().is_some()
        && parts.next().is_some_and(|m| m.as_os_str() == "memory")
        && parts.next().is_some()
}

/// True for `projects/<slug>/memory/<store entry>`.
fn is_memory_store(name: &str, rel: &Path) -> bool {
    is_memory_path(name, rel)
        && rel.components().count() == 3
        && rel
            .file_name()
            .is_some_and(|f| MEMORY_STORE.iter().any(|s| f == *s))
}

/// How a single shared entry that is real in the profile merges.
enum MergeKind {
    /// Common has nothing: move the whole entry.
    MoveAll,
    History,
    /// Same bytes or same JSON: just drop the profile copy.
    Identical,
    Overlay(Map<String, Value>),
    Dir(Vec<Op>),
    /// Type mismatch or differing non-JSON file.
    Conflict,
}

/// Line files merged as a union (DESIGN §4.3).
fn is_history(tool: Tool, name: &str) -> bool {
    name == "history.jsonl" || (tool == Tool::Codex && name == "session_index.jsonl")
}

fn merge_kind(tool: Tool, name: &str, p: &Path, c: &Path) -> Result<MergeKind> {
    let Some(cm) = lstat(c) else {
        return Ok(MergeKind::MoveAll);
    };
    let pm = fs::symlink_metadata(p)?;
    // Follow a top-level symlink in common (common is the source of truth).
    let c_is_dir = cm.is_dir() || (cm.file_type().is_symlink() && c.is_dir());
    let c_is_file = cm.is_file() || (cm.file_type().is_symlink() && c.is_file());
    if pm.is_dir() && c_is_dir {
        let mut ops = Vec::new();
        dir_ops(name, p, c, Path::new(""), &mut ops)?;
        return Ok(MergeKind::Dir(ops));
    }
    if !(pm.is_file() && c_is_file) {
        return Ok(MergeKind::Conflict);
    }
    if fs::read(p)? == fs::read(c)? {
        return Ok(MergeKind::Identical);
    }
    if is_history(tool, name) {
        return Ok(MergeKind::History);
    }
    // The overlay is a Claude `--settings` file; a Codex file that differs
    // is kept as a conflict copy.
    if tool == Tool::Codex {
        return Ok(MergeKind::Conflict);
    }
    Ok(match json_diff(p, c) {
        Some(d) if d.is_empty() => MergeKind::Identical,
        Some(d) => MergeKind::Overlay(d),
        None => MergeKind::Conflict,
    })
}

fn merge_preview(tool: Tool, name: &str, p: &Path, c: &Path) -> Result<MergePreview> {
    let mut pv = MergePreview::default();
    match merge_kind(tool, name, p, c)? {
        MergeKind::MoveAll => pv.move_files = count_files(p)?,
        MergeKind::History => {
            let have: HashSet<String> = read_lines(c)?.into_iter().collect();
            pv.move_files = read_lines(p)?.iter().filter(|l| !have.contains(*l)).count();
        }
        MergeKind::Identical => pv.identical = 1,
        MergeKind::Overlay(d) => pv.overlay_keys = d.keys().cloned().collect(),
        MergeKind::Conflict => pv.conflicts.push(PathBuf::from(name)),
        MergeKind::Dir(ops) => {
            for op in ops {
                match op {
                    Op::Move(_, n) => pv.move_files += n,
                    Op::Drop(_) => pv.identical += 1,
                    Op::Conflict(r) if is_memory_path(name, &r) => {
                        pv.memory_conflicts.push(Path::new(name).join(r))
                    }
                    Op::Conflict(r) => pv.conflicts.push(Path::new(name).join(r)),
                }
            }
        }
    }
    Ok(pv)
}

fn read_lines(p: &Path) -> Result<Vec<String>> {
    let text = fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect())
}

/// The time of a history line in ms: Claude's `timestamp` (number or
/// ISO-8601 string), Codex's `ts` (seconds) or `updated_at` (ISO-8601).
fn line_ts(line: &str) -> Option<i64> {
    let v: Value = serde_json::from_str(line).ok()?;
    if let Some(ts) = v.get("ts").and_then(Value::as_i64) {
        return Some(ts * 1000);
    }
    let field = v.get("timestamp").or_else(|| v.get("updated_at"))?;
    match field {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s
            .parse::<jiff::Timestamp>()
            .ok()
            .map(|t| t.as_millisecond())
            .or_else(|| s.parse().ok()),
        _ => None,
    }
}

/// Union of both histories (common first), deduplicated, stably sorted by
/// timestamp; lines without one keep their order at the end.
fn merge_history(common: &[String], profile: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut lines: Vec<(Option<i64>, String)> = common
        .iter()
        .chain(profile)
        .filter(|l| seen.insert(l.as_str()))
        .map(|l| (line_ts(l), l.clone()))
        .collect();
    lines.sort_by_key(|(ts, _)| (ts.is_none(), ts.unwrap_or(0)));
    lines.into_iter().map(|(_, l)| l).collect()
}

/// One journaled filesystem change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// e.g. "link", "move", "drop", "conflict", "memory-conflict",
    /// "overlay", "localize".
    pub op: String,
    pub name: String,
    pub src: Option<PathBuf>,
    pub dst: Option<PathBuf>,
}

/// The record of an applied adopt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub path: PathBuf,
    pub entries: Vec<JournalEntry>,
}

impl Journal {
    /// Where each differing memory entry went (`src`, `dst`).
    pub fn memory_conflicts(&self) -> impl Iterator<Item = (&Path, &Path)> {
        self.entries
            .iter()
            .filter(|e| e.op == MEMORY_CONFLICT)
            .filter_map(|e| Some((e.src.as_deref()?, e.dst.as_deref()?)))
    }
}

const MEMORY_CONFLICT: &str = "memory-conflict";

struct Applier<'a> {
    plan: &'a AdoptPlan,
    entries: Vec<JournalEntry>,
}

impl Applier<'_> {
    fn log(&mut self, op: &str, name: &str, src: Option<&Path>, dst: Option<&Path>) {
        self.entries.push(JournalEntry {
            op: op.into(),
            name: name.into(),
            src: src.map(Path::to_path_buf),
            dst: dst.map(Path::to_path_buf),
        });
    }

    fn rename(&mut self, op: &str, name: &str, from: &Path, to: &Path) -> Result<()> {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(from, to)
            .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        self.log(op, name, Some(from), Some(to));
        Ok(())
    }

    fn conflict_dest(&self, rel: &Path) -> PathBuf {
        let base = self
            .plan
            .common
            .join(".agent-prism-conflicts")
            .join(&self.plan.profile_id)
            .join(rel);
        let mut dest = base.clone();
        let mut i = 1;
        while lstat(&dest).is_some() {
            dest = PathBuf::from(format!("{}.{i}", base.display()));
            i += 1;
        }
        dest
    }

    fn link(&mut self, name: &str) -> Result<()> {
        let (p, c) = (self.plan.dir.join(name), self.plan.common.join(name));
        symlink(&c, &p).with_context(|| format!("linking {}", p.display()))?;
        self.log("link", name, Some(&p), Some(&c));
        Ok(())
    }

    fn merge(&mut self, name: &str) -> Result<()> {
        let (p, c) = (self.plan.dir.join(name), self.plan.common.join(name));
        match merge_kind(self.plan.tool, name, &p, &c)? {
            MergeKind::MoveAll => self.rename("move", name, &p, &c)?,
            MergeKind::Identical => {
                fs::remove_file(&p)?;
                self.log("drop", name, Some(&p), None);
            }
            MergeKind::History => {
                let merged = merge_history(&read_lines(&c)?, &read_lines(&p)?);
                let mut text = merged.join("\n");
                text.push('\n');
                write_atomic(&c, text.as_bytes())?;
                fs::remove_file(&p)?;
                self.log("merge-history", name, Some(&p), Some(&c));
            }
            MergeKind::Overlay(diff) => {
                let path = self.plan.overlay_path.clone();
                let mut obj = fs::read(&path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default();
                obj.extend(diff);
                write_atomic(&path, &serde_json::to_vec_pretty(&Value::Object(obj))?)?;
                self.log("overlay", name, Some(&p), Some(&path));
                fs::remove_file(&p)?;
                self.log("drop", name, Some(&p), None);
            }
            MergeKind::Conflict => {
                let dest = self.conflict_dest(Path::new(name));
                self.rename("conflict", name, &p, &dest)?;
            }
            MergeKind::Dir(ops) => {
                for op in ops {
                    match op {
                        Op::Move(r, _) => self.move_in(name, &p.join(&r), &c.join(&r), &r)?,
                        Op::Drop(r) => {
                            fs::remove_file(p.join(&r))?;
                            self.log("drop", name, Some(&p.join(&r)), None);
                        }
                        Op::Conflict(r) => self.conflict(name, &p.join(&r), &r)?,
                    }
                }
                remove_empty_tree(&p)?;
                self.log("rmdir", name, Some(&p), None);
            }
        }
        self.link(name)
    }

    /// Move the profile entry `pe` (at `rel` inside `name`) to its
    /// `.agent-prism-conflicts` copy.
    fn conflict(&mut self, name: &str, pe: &Path, rel: &Path) -> Result<()> {
        let dest = self.conflict_dest(&Path::new(name).join(rel));
        let op = if is_memory_path(name, rel) {
            MEMORY_CONFLICT
        } else {
            "conflict"
        };
        self.rename(op, name, pe, &dest)
    }

    /// Move `pe` to `ce`, which was absent at plan time. A regular file is
    /// moved without replacing a `ce` that another process created since
    /// (e.g. a memory file written by a session of another account); then
    /// it is dropped if identical, else kept as a conflict.
    fn move_in(&mut self, name: &str, pe: &Path, ce: &Path, rel: &Path) -> Result<()> {
        if !fs::symlink_metadata(pe)?.is_file() {
            return self.rename("move", name, pe, ce);
        }
        if let Some(parent) = ce.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::hard_link(pe, ce) {
            Ok(()) => {
                fs::remove_file(pe)?;
                self.log("move", name, Some(pe), Some(ce));
                Ok(())
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if same_entry(pe, ce)? {
                    fs::remove_file(pe)?;
                    self.log("drop", name, Some(pe), None);
                    Ok(())
                } else {
                    self.conflict(name, pe, rel)
                }
            }
            Err(e) => {
                Err(e).with_context(|| format!("moving {} to {}", pe.display(), ce.display()))
            }
        }
    }

    fn apply(&mut self, action: &Action) -> Result<()> {
        match action {
            Action::Skip { .. } => Ok(()),
            Action::Link { name } => {
                if lstat(&self.plan.dir.join(name)).is_some() {
                    bail!("{name} appeared since the plan was made");
                }
                self.link(name)
            }
            Action::Merge { name, .. } => self.merge(name),
            Action::Relink { name, .. } => {
                let p = self.plan.dir.join(name);
                fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
                self.log("unlink", name, Some(&p), None);
                self.link(name)
            }
            Action::Localize { name, was_dir } => {
                let p = self.plan.dir.join(name);
                fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
                self.log("unlink", name, Some(&p), None);
                if *was_dir {
                    fs::create_dir(&p)?;
                    self.log("mkdir", name, None, Some(&p));
                }
                Ok(())
            }
        }
    }
}

/// Remove a tree that contains only (empty) dirs; fail if anything else is left.
fn remove_empty_tree(p: &Path) -> Result<()> {
    for e in fs::read_dir(p)? {
        let e = e?;
        if !e.file_type()?.is_dir() {
            bail!("{} still has {}", p.display(), e.path().display());
        }
        remove_empty_tree(&e.path())?;
    }
    fs::remove_dir(p).with_context(|| format!("removing {}", p.display()))
}

/// Apply a plan and write its journal to `<journal_dir>/<ts>-<profile>.json`.
/// The journal is written even if an action fails.
pub fn apply_adopt(plan: &AdoptPlan, journal_dir: &Path) -> Result<Journal> {
    let mut ap = Applier {
        plan,
        entries: Vec::new(),
    };
    let mut result = Ok(());
    for a in &plan.actions {
        if let Err(e) = ap.apply(a) {
            result = Err(e);
            break;
        }
    }
    let ts = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S").to_string();
    let journal = Journal {
        path: journal_dir.join(format!("{ts}-{}.json", plan.profile_id)),
        entries: ap.entries,
    };
    write_atomic(&journal.path, &serde_json::to_vec_pretty(&journal)?)?;
    result.with_context(|| format!("adopt failed; journal at {}", journal.path.display()))?;
    Ok(journal)
}

/// Memory conflict copies of `profile_id` still waiting in
/// `<common>/.agent-prism-conflicts/<profile_id>/projects/<slug>/memory/`, as
/// absolute paths (files and links; a dir counts as its files).
pub fn parked_memory_conflicts(common: &Path, profile_id: &str) -> Result<Vec<PathBuf>> {
    fn walk(p: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        if !fs::symlink_metadata(p)?.is_dir() {
            out.push(p.to_path_buf());
            return Ok(());
        }
        let mut names = entry_names(p)?;
        names.sort();
        for n in names {
            walk(&p.join(n), out)?;
        }
        Ok(())
    }
    let projects = common
        .join(".agent-prism-conflicts")
        .join(profile_id)
        .join("projects");
    let mut out = Vec::new();
    if !projects.is_dir() {
        return Ok(out);
    }
    let mut slugs = entry_names(&projects)?;
    slugs.sort();
    for slug in slugs {
        let mem = projects.join(slug).join("memory");
        if lstat(&mem).is_some_and(|m| m.is_dir()) {
            walk(&mem, &mut out)?;
        }
    }
    Ok(out)
}

/// Sharing status of one profile dir.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShareHealth {
    /// Entries already in the right state.
    pub ok: usize,
    /// Entries adopt would change.
    pub to_fix: usize,
    pub unmanaged: Vec<String>,
    /// The sessions entry ([`sessions_entry`]) links to the common one
    /// (needed to resume after a switch).
    pub sessions_shared: bool,
}

/// Check a profile dir against the share map. Read-only.
pub fn share_health(dir: &Path, common: &Path, rules: &ShareRules) -> Result<ShareHealth> {
    if same_dir(dir, common) {
        return Ok(ShareHealth {
            sessions_shared: true,
            ..Default::default()
        });
    }
    let plan = plan_inner("", dir, common, rules, Path::new(""), false)?;
    let mut h = ShareHealth::default();
    for a in plan.actions {
        match a {
            Action::Skip { reason, .. } if reason == OK => h.ok += 1,
            Action::Skip { name, .. } => h.unmanaged.push(name),
            _ => h.to_fix += 1,
        }
    }
    let name = sessions_entry(rules.tool);
    let p = dir.join(name);
    h.sessions_shared = lstat(&p).is_some_and(|m| m.file_type().is_symlink())
        && links_to(&p, dir, &common.join(name))?.0;
    Ok(h)
}

/// Create a new profile dir (0700) with symlinks for every shared entry that
/// exists in common and empty local dirs.
pub fn create_profile_dir(dir: &Path, common: &Path, rules: &ShareRules) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    for name in entry_names(common)? {
        let p = dir.join(&name);
        if classify(&name, rules) == ShareClass::Shared && lstat(&p).is_none() {
            symlink(common.join(&name), &p).with_context(|| format!("linking {}", p.display()))?;
        }
    }
    for name in lists(rules.tool).local_dirs {
        let p = dir.join(name);
        if lstat(&p).is_none() {
            fs::create_dir(&p).with_context(|| format!("creating {}", p.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
