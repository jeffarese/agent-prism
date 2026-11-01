//! `agp skill …`: the agp skill that tells agents how to use agp (§11e).
//!
//! The skill is a folder (`skills/agp/`: `SKILL.md` plus `reference/*.md`)
//! built into the binary, so an install always matches it. `agp skill
//! install` copies it to `<skills dir>/agp` for every detected agent and
//! writes a manifest (`.agp-skill.json`) that lists the files it wrote. Only
//! a folder with that manifest (or the old one-file layout) counts as
//! agp's: agp updates, prunes and removes nothing else.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use prism_core::config::Config;
use prism_core::model::Tool;
use prism_core::paths::Paths;

use crate::app::AppCtx;
use crate::cli::{SkillCmd, SkillTargets};

/// The skill's folder name and frontmatter `name`.
pub const NAME: &str = "agp";

/// The files of the skill, relative to its folder. `SKILL.md` comes first.
pub const FILES: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("../skills/agp/SKILL.md")),
    (
        "reference/lanes.md",
        include_str!("../skills/agp/reference/lanes.md"),
    ),
    (
        "reference/background.md",
        include_str!("../skills/agp/reference/background.md"),
    ),
    (
        "reference/status.md",
        include_str!("../skills/agp/reference/status.md"),
    ),
];

/// The skill file (`SKILL.md`).
pub const SKILL: &str = FILES[0].1;

/// The manifest agp writes into the folder it manages.
const MANIFEST: &str = ".agp-skill.json";

/// Where an agent loads user-level skills from.
#[derive(Clone, Copy, Debug)]
enum Home {
    /// The tool's common home from the config (`~/.claude`, `~/.codex`):
    /// every account of the tool shares its `skills/`.
    Common(Tool),
    /// `detect` (relative to the home dir) shows that the agent is set up;
    /// skills go to `skills` (relative to the home dir).
    Dir {
        detect: &'static str,
        skills: &'static str,
    },
    /// The agent also reads another agent's skills dir (Claude's), so that
    /// copy serves it and a second one would show the skill twice.
    Uses {
        detect: &'static str,
        agent: &'static str,
    },
}

/// An agent `agp skill` knows.
struct Agent {
    /// The `--agent` name.
    name: &'static str,
    title: &'static str,
    home: Home,
    /// `agp skill install` without `--agent` installs it when detected.
    auto: bool,
}

/// Agents whose user skills dir is confirmed by their docs (and, for
/// Claude Code and Codex, by the installed binaries); README "Skills for
/// agents" has the sources. Order: the install order.
const AGENTS: &[Agent] = &[
    Agent {
        name: "claude",
        title: "Claude Code",
        home: Home::Common(Tool::Claude),
        auto: true,
    },
    Agent {
        name: "codex",
        title: "Codex CLI",
        home: Home::Common(Tool::Codex),
        auto: true,
    },
    Agent {
        name: "gemini",
        title: "Gemini CLI",
        home: Home::Dir {
            detect: ".gemini",
            skills: ".gemini/skills",
        },
        auto: true,
    },
    Agent {
        name: "copilot",
        title: "GitHub Copilot CLI",
        home: Home::Dir {
            detect: ".copilot",
            skills: ".copilot/skills",
        },
        auto: true,
    },
    Agent {
        name: "droid",
        title: "Factory Droid",
        home: Home::Dir {
            detect: ".factory",
            skills: ".factory/skills",
        },
        auto: true,
    },
    Agent {
        name: "agents",
        title: "shared ~/.agents/skills",
        home: Home::Dir {
            detect: ".agents",
            skills: ".agents/skills",
        },
        // Codex reads it besides its own dir, so an automatic copy here
        // would show the skill twice there. Only on request.
        auto: false,
    },
    Agent {
        name: "amp",
        title: "Amp",
        home: Home::Uses {
            detect: ".config/amp",
            agent: "claude",
        },
        auto: false,
    },
    Agent {
        name: "opencode",
        title: "OpenCode",
        home: Home::Uses {
            detect: ".config/opencode",
            agent: "claude",
        },
        auto: false,
    },
    Agent {
        name: "cursor",
        title: "Cursor",
        home: Home::Uses {
            detect: ".cursor",
            agent: "claude",
        },
        auto: false,
    },
    Agent {
        name: "goose",
        title: "Goose",
        home: Home::Uses {
            detect: ".config/goose",
            agent: "claude",
        },
        auto: false,
    },
];

/// One place to install to.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// The `--agent` name, or `dir` for `--dir`.
    pub agent: String,
    /// The Agent Skills folder; the skill goes to `<skills_dir>/agp`.
    pub skills_dir: PathBuf,
    /// The agent looks set up here (its config dir exists).
    pub detected: bool,
    /// Installed without `--agent` when detected.
    pub auto: bool,
    /// The agent whose copy this one reads (`Home::Uses`).
    pub via: Option<&'static str>,
}

impl Target {
    /// `<skills_dir>/agp`.
    pub fn dir(&self) -> PathBuf {
        self.skills_dir.join(NAME)
    }
}

/// Every known agent's target.
fn known_targets(paths: &Paths, config: &Config) -> Vec<Target> {
    // (config dir, skills dir) of an agent with a folder of its own.
    let own = |home: Home| -> (PathBuf, PathBuf) {
        match home {
            Home::Common(tool) => {
                let h = config.common_home_for(paths, tool);
                (h.clone(), h.join("skills"))
            }
            Home::Dir { detect, skills } => (paths.home.join(detect), paths.home.join(skills)),
            Home::Uses { detect, .. } => (paths.home.join(detect), PathBuf::new()),
        }
    };
    AGENTS
        .iter()
        .map(|a| {
            let (detect, mut skills) = own(a.home);
            let via = match a.home {
                Home::Uses { agent, .. } => {
                    if let Some(o) = AGENTS.iter().find(|o| o.name == agent) {
                        skills = own(o.home).1;
                    }
                    Some(agent)
                }
                _ => None,
            };
            Target {
                agent: a.name.to_string(),
                skills_dir: skills,
                detected: detect.is_dir(),
                auto: a.auto,
                via,
            }
        })
        .collect()
}

/// The targets `--agent`/`--dir` name. None named: for install the
/// detected automatic agents; for uninstall (`all`) every agent's own
/// folder.
fn select(known: &[Target], sel: &SkillTargets, all: bool) -> Result<Vec<Target>> {
    if sel.agents.is_empty() && sel.dirs.is_empty() {
        return Ok(known
            .iter()
            .filter(|t| t.via.is_none() && (all || (t.auto && t.detected)))
            .cloned()
            .collect());
    }
    let mut out = Vec::new();
    for name in &sel.agents {
        match known.iter().find(|t| t.agent == *name) {
            Some(t) => out.push(t.clone()),
            None => bail!(
                "unknown agent {name:?}; known: {} (or use --dir <skills dir>)",
                AGENTS.iter().map(|a| a.name).collect::<Vec<_>>().join(", ")
            ),
        }
    }
    for d in &sel.dirs {
        let p = PathBuf::from(d);
        let p = if p.is_absolute() {
            p
        } else {
            std::env::current_dir()?.join(p)
        };
        out.push(Target {
            agent: "dir".into(),
            detected: p.is_dir(),
            skills_dir: p,
            auto: false,
            via: None,
        });
    }
    Ok(out)
}

/// `agp skill …`.
pub fn run(ctx: &AppCtx, cmd: SkillCmd) -> Result<i32> {
    if cmd == SkillCmd::Show {
        print!("{SKILL}");
        return Ok(0);
    }
    let config = Config::load(&ctx.paths)?.unwrap_or_default();
    let known = known_targets(&ctx.paths, &config);
    let mut failed = false;
    match cmd {
        SkillCmd::Show => unreachable!(),
        SkillCmd::List => {
            for (t, a) in known.iter().zip(AGENTS) {
                println!("{:<9} {:<24} {}", t.agent, a.title, t.dir().display());
                println!("{:<9} {}", "", list_state(t));
            }
        }
        SkillCmd::Install(sel) => {
            let explicit = !sel.agents.is_empty() || !sel.dirs.is_empty();
            let targets = select(&known, &sel, false)?;
            if targets.is_empty() {
                eprintln!("agp: no agent found; use --agent <name> or --dir <skills dir>");
                return Ok(1);
            }
            let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
            for t in &targets {
                if t.agent == "dir" && !t.skills_dir.is_dir() {
                    println!(
                        "{:<8} {:<11} {}: no such folder",
                        t.agent,
                        "skipped",
                        t.skills_dir.display()
                    );
                    failed = true;
                    continue;
                }
                let r = install(&t.skills_dir, &mut done);
                failed |= report(t, r);
            }
            if !explicit
                || targets
                    .iter()
                    .any(|t| matches!(t.agent.as_str(), "claude" | "codex"))
            {
                println!(
                    "All accounts of a tool share its skills dir, so every account sees the skill."
                );
            }
        }
        SkillCmd::Uninstall(sel) => {
            for t in select(&known, &sel, true)? {
                let r = uninstall(&t.dir());
                failed |= report(&t, r);
            }
        }
    }
    Ok(if failed { 1 } else { 0 })
}

/// The state `agp skill list` shows for a target.
fn list_state(t: &Target) -> String {
    match t.via {
        Some(via) if t.detected => format!("reads the {via} copy: {}", check(&t.dir()).describe()),
        Some(_) => "agent not found".into(),
        None if t.detected || t.dir().symlink_metadata().is_ok() => check(&t.dir()).describe(),
        None if !t.auto => "not installed (only with --agent)".into(),
        None => "agent not found".into(),
    }
}

/// Prints one line for a target; true when it failed or was skipped.
fn report(t: &Target, r: Result<Outcome>) -> bool {
    match r {
        Ok(o) => {
            let bad = matches!(o, Outcome::Skipped(_));
            let note = match (o.note(), t.via) {
                (Some(n), Some(via)) => Some(format!("{n} (the {via} copy)")),
                (None, Some(via)) => Some(format!("the {via} copy")),
                (n, None) => n,
            };
            match note {
                Some(note) => println!(
                    "{:<8} {:<11} {}: {note}",
                    t.agent,
                    o.word(),
                    t.dir().display()
                ),
                None => println!("{:<8} {:<11} {}", t.agent, o.word(), t.dir().display()),
            }
            bad
        }
        Err(e) => {
            println!(
                "{:<8} {:<11} {}: {e:#}",
                t.agent,
                "failed",
                t.dir().display()
            );
            true
        }
    }
}

/// What an install or uninstall did.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Installed,
    Updated,
    UpToDate,
    /// Already done in this run through another path (a symlink).
    Same(PathBuf),
    Removed,
    /// Removed agp's files; others are left in the folder.
    RemovedSome(usize),
    NotInstalled,
    /// Left alone, with the reason.
    Skipped(String),
}

impl Outcome {
    fn word(&self) -> &'static str {
        match self {
            Outcome::Installed => "installed",
            Outcome::Updated => "updated",
            Outcome::UpToDate | Outcome::Same(_) => "up to date",
            Outcome::Removed | Outcome::RemovedSome(_) => "removed",
            Outcome::NotInstalled => "not there",
            Outcome::Skipped(_) => "skipped",
        }
    }

    fn note(&self) -> Option<String> {
        match self {
            Outcome::Same(p) => Some(format!("same folder as {}", p.display())),
            Outcome::RemovedSome(n) => Some(format!("left {n} file(s) agp did not write")),
            Outcome::Skipped(why) => Some(why.clone()),
            _ => None,
        }
    }
}

/// What is at `<skills dir>/agp`.
#[derive(Clone, Debug, PartialEq)]
pub enum Found {
    Missing,
    /// agp's folder; `files` are the ones it wrote last time.
    Ours {
        files: Vec<String>,
        current: bool,
    },
    /// A symlink; `ours` when it points at a folder agp manages.
    Link {
        to: PathBuf,
        ours: bool,
        current: bool,
    },
    /// Something agp did not install.
    Foreign(String),
}

impl Found {
    fn describe(&self) -> String {
        match self {
            Found::Missing => "not installed".into(),
            Found::Ours { current: true, .. } => "current".into(),
            Found::Ours { current: false, .. } => "outdated".into(),
            Found::Link { to, ours, current } => match (ours, current) {
                (true, true) => format!("current (link to {})", to.display()),
                (true, false) => format!("outdated (link to {})", to.display()),
                (false, _) => format!("link to {}, not agp's", to.display()),
            },
            Found::Foreign(why) => format!("not agp's: {why}"),
        }
    }
}

/// Looks at `dir` (`<skills dir>/agp`) without changing anything.
pub fn check(dir: &Path) -> Found {
    let meta = match dir.symlink_metadata() {
        Ok(m) => m,
        Err(_) => return Found::Missing,
    };
    if meta.file_type().is_symlink() {
        return match dir.canonicalize() {
            Ok(to) => match owned_files(&to) {
                Some(files) if to.is_dir() => Found::Link {
                    current: is_current(&to, &files),
                    to,
                    ours: true,
                },
                _ => Found::Link {
                    to,
                    ours: false,
                    current: false,
                },
            },
            Err(_) => Found::Link {
                to: std::fs::read_link(dir).unwrap_or_default(),
                ours: false,
                current: false,
            },
        };
    }
    if !meta.is_dir() {
        return Found::Foreign("a file, not a folder".into());
    }
    match owned_files(dir) {
        Some(files) => Found::Ours {
            current: is_current(dir, &files),
            files,
        },
        None => Found::Foreign(format!("no {MANIFEST}; agp did not install it")),
    }
}

/// The files agp wrote in `dir` when the folder is agp's: from the
/// manifest, or `SKILL.md` for the old one-file layout (an agp SKILL.md and
/// nothing else). An empty folder is agp's too. `None`: not agp's.
fn owned_files(dir: &Path) -> Option<Vec<String>> {
    match std::fs::read_to_string(dir.join(MANIFEST)) {
        Ok(text) => {
            let v: serde_json::Value = serde_json::from_str(&text).ok()?;
            if v.get("skill")?.as_str()? != NAME {
                return None;
            }
            let files = v
                .get("files")?
                .as_array()?
                .iter()
                .filter_map(|f| f.as_str())
                .filter(|f| safe_rel(f))
                .map(str::to_string)
                .collect();
            Some(files)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let names: Vec<String> = std::fs::read_dir(dir)
                .ok()?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n != ".SKILL.md.tmp")
                .collect();
            match names.as_slice() {
                [] => Some(Vec::new()),
                [one] if one == "SKILL.md" => {
                    let p = dir.join("SKILL.md");
                    let regular = p.symlink_metadata().ok()?.is_file();
                    let text = std::fs::read_to_string(&p).ok()?;
                    let agp = text.starts_with(&format!("---\nname: {NAME}\n"))
                        && text.contains("agent-prism");
                    (regular && agp).then(|| vec!["SKILL.md".to_string()])
                }
                _ => None,
            }
        }
        Err(_) => None,
    }
}

/// A plain relative path inside the folder (no `..`, not absolute).
fn safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// The manifest text for the current layout.
fn manifest() -> String {
    let files: Vec<&str> = FILES.iter().map(|(p, _)| *p).collect();
    let v = serde_json::json!({
        "skill": NAME,
        "by": "agent-prism",
        "files": files,
    });
    format!("{}\n", serde_json::to_string_pretty(&v).unwrap_or_default())
}

/// Whether `dir` holds exactly the current files, the current manifest and
/// none of the old layout's files.
fn is_current(dir: &Path, owned: &[String]) -> bool {
    let same = |rel: &str, want: &str| {
        let p = dir.join(rel);
        p.symlink_metadata().is_ok_and(|m| m.is_file())
            && std::fs::read_to_string(&p).is_ok_and(|t| t == want)
    };
    let names: BTreeSet<&str> = FILES.iter().map(|(p, _)| *p).collect();
    FILES.iter().all(|(rel, text)| same(rel, text))
        && same(MANIFEST, &manifest())
        && owned
            .iter()
            .all(|rel| names.contains(rel.as_str()) || dir.join(rel).symlink_metadata().is_err())
}

/// Installs the skill into `<skills_dir>/agp`. `done` holds the folders
/// written in this run (canonical path, the path it was reached by), so a
/// folder reached twice (a symlink) is written once.
pub fn install(skills_dir: &Path, done: &mut Vec<(PathBuf, PathBuf)>) -> Result<Outcome> {
    std::fs::create_dir_all(skills_dir)
        .with_context(|| format!("creating {}", skills_dir.display()))?;
    let link = skills_dir.join(NAME);
    let dir = match check(&link) {
        Found::Foreign(why) => return Ok(Outcome::Skipped(why)),
        Found::Link {
            to, ours: false, ..
        } => {
            return Ok(Outcome::Skipped(format!(
                "a symlink to {}, which agp does not manage",
                to.display()
            )));
        }
        Found::Link { to, .. } => to,
        Found::Missing | Found::Ours { .. } => {
            std::fs::create_dir_all(&link)
                .with_context(|| format!("creating {}", link.display()))?;
            link.clone()
        }
    };
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.clone());
    if let Some((_, first)) = done.iter().find(|(c, _)| *c == canonical) {
        return Ok(Outcome::Same(first.clone()));
    }
    done.push((canonical, link.clone()));
    let owned = owned_files(&dir).context("the folder changed while agp installed")?;
    let fresh = owned.is_empty() && dir.join(MANIFEST).symlink_metadata().is_err();
    let mut changed = false;
    for (rel, text) in FILES {
        changed |= write_file(&dir, rel, text)?;
    }
    // Files of an older layout that are no longer part of the skill.
    let names: BTreeSet<&str> = FILES.iter().map(|(p, _)| *p).collect();
    for rel in owned.iter().filter(|r| !names.contains(r.as_str())) {
        changed |= remove_file(&dir, rel)?;
    }
    changed |= remove_file(&dir, ".SKILL.md.tmp")?;
    changed |= write_file(&dir, MANIFEST, &manifest())?;
    Ok(if fresh {
        Outcome::Installed
    } else if changed {
        Outcome::Updated
    } else {
        Outcome::UpToDate
    })
}

/// Removes agp's files from `dir` (`<skills dir>/agp`), and the folder when
/// nothing else is left in it.
pub fn uninstall(dir: &Path) -> Result<Outcome> {
    let owned = match check(dir) {
        Found::Missing => return Ok(Outcome::NotInstalled),
        Found::Foreign(why) => return Ok(Outcome::Skipped(why)),
        Found::Link { to, .. } => {
            return Ok(Outcome::Skipped(format!(
                "a symlink to {}; agp did not create it, so it stays",
                to.display()
            )));
        }
        Found::Ours { files, .. } => files,
    };
    let mut rels: BTreeSet<String> = owned.into_iter().collect();
    rels.extend(FILES.iter().map(|(p, _)| p.to_string()));
    for rel in &rels {
        // Only what agp wrote: files of the current layout count only when
        // the manifest is there (an old one-file folder has just SKILL.md).
        remove_file(dir, rel)?;
    }
    remove_file(dir, ".SKILL.md.tmp")?;
    remove_file(dir, MANIFEST)?;
    let left = count_files(dir);
    if left == 0 {
        remove_empty_dirs(dir, dir);
        match std::fs::remove_dir(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", dir.display())),
        }
        Ok(Outcome::Removed)
    } else {
        remove_empty_dirs(dir, dir);
        Ok(Outcome::RemovedSome(left))
    }
}

/// Makes sure each folder between `dir` and `rel`'s file is a real folder
/// inside `dir` (never a symlink out of it), creating missing ones.
fn parent_inside(dir: &Path, rel: &str) -> Result<PathBuf> {
    if !safe_rel(rel) {
        bail!("bad path {rel:?} in the skill");
    }
    let mut p = dir.to_path_buf();
    let comps: Vec<_> = Path::new(rel).components().collect();
    for c in &comps[..comps.len() - 1] {
        p.push(c);
        match p.symlink_metadata() {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("{} is not a plain folder; not following it", p.display()),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                std::fs::create_dir(&p).with_context(|| format!("creating {}", p.display()))?
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", p.display())),
        }
    }
    Ok(p)
}

/// Writes `dir/rel` atomically (temp file + rename) unless it already has
/// `text`. A symlink there is replaced, never followed. True: it wrote.
fn write_file(dir: &Path, rel: &str, text: &str) -> Result<bool> {
    let parent = parent_inside(dir, rel)?;
    let path = dir.join(rel);
    let same = path.symlink_metadata().is_ok_and(|m| m.is_file())
        && std::fs::read_to_string(&path).is_ok_and(|t| t == text);
    if same {
        return Ok(false);
    }
    if path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        bail!("{} is a folder", path.display());
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{name}.agp-tmp"));
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Removes `dir/rel` if it is a file or a symlink inside `dir`, then its
/// parent folders inside `dir` that are left empty. True: it removed it.
fn remove_file(dir: &Path, rel: &str) -> Result<bool> {
    if !safe_rel(rel) {
        return Ok(false);
    }
    // Every folder on the way must be a plain folder inside `dir`.
    let mut p = dir.to_path_buf();
    let comps: Vec<_> = Path::new(rel).components().collect();
    for c in &comps[..comps.len() - 1] {
        p.push(c);
        if !p.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            return Ok(false);
        }
    }
    let path = dir.join(rel);
    match path.symlink_metadata() {
        Ok(m) if m.is_dir() => Ok(false),
        Ok(_) => {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            let mut parent = path.parent().map(Path::to_path_buf);
            while let Some(d) = parent {
                if d == dir || !d.starts_with(dir) || std::fs::remove_dir(&d).is_err() {
                    break;
                }
                parent = d.parent().map(Path::to_path_buf);
            }
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

/// Files (and symlinks) under `dir`, not following symlinks.
fn count_files(dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.filter_map(|e| e.ok())
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => count_files(&e.path()),
            _ => 1,
        })
        .sum()
}

/// Removes empty folders under `d` (not `top` itself), not following
/// symlinks.
fn remove_empty_dirs(top: &Path, d: &Path) {
    let Ok(rd) = std::fs::read_dir(d) else {
        return;
    };
    for e in rd.filter_map(|e| e.ok()) {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            remove_empty_dirs(top, &e.path());
        }
    }
    if d != top {
        let _ = std::fs::remove_dir(d);
    }
}

#[cfg(test)]
mod tests;
