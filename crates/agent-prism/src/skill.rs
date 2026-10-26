//! `agp skill [install]`: the agp skill that tells agents how to use agp.
//! The text is built into the binary, so an install always matches it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use prism_core::config::Config;
use prism_core::model::Tool;

use crate::app::AppCtx;

/// The skill file (`SKILL.md`).
pub const SKILL: &str = include_str!("skill.md");

/// Prints the skill, or installs it for Claude and Codex.
pub fn run(ctx: &AppCtx, install: bool) -> Result<i32> {
    if !install {
        print!("{SKILL}");
        return Ok(0);
    }
    let config = Config::load(&ctx.paths)?.unwrap_or_default();
    for tool in Tool::ALL {
        let home = config.common_home_for(&ctx.paths, tool);
        if !home.is_dir() {
            continue;
        }
        let (path, what) = write_skill(&home)?;
        println!("{} skill {what}: {}", tool.name(), path.display());
    }
    println!("All accounts of a tool share its skills dir, so every account sees it.");
    Ok(0)
}

/// Writes `<home>/skills/agp/SKILL.md`; returns the path and what changed.
fn write_skill(home: &Path) -> Result<(PathBuf, &'static str)> {
    let dir = home.join("skills").join("agp");
    let path = dir.join("SKILL.md");
    let what = match std::fs::read_to_string(&path) {
        Ok(old) if old == SKILL => return Ok((path, "up to date")),
        Ok(_) => "updated",
        Err(_) => "installed",
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(".SKILL.md.tmp");
    std::fs::write(&tmp, SKILL).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok((path, what))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_and_install() {
        assert!(SKILL.starts_with("---\nname: agp\ndescription: "));
        let dir = tempfile::tempdir().unwrap();
        let (p, what) = write_skill(dir.path()).unwrap();
        assert_eq!(what, "installed");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SKILL);
        assert_eq!(write_skill(dir.path()).unwrap().1, "up to date");
        std::fs::write(&p, "old").unwrap();
        assert_eq!(write_skill(dir.path()).unwrap().1, "updated");
    }
}
