//! `agp adopt <id|dir> [--apply] [--no-backup] [--force]` (DESIGN §4.3).

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use prism_core::backup::{self, BackupTarget};
use prism_core::procs;
use prism_core::share::{self, Action};

use crate::app::AppCtx;
use crate::session::confirm;

/// Options of `agp adopt`.
pub struct AdoptOpts {
    pub target: String,
    pub apply: bool,
    pub no_backup: bool,
    pub force: bool,
}

/// Prints the plan and, with `--apply`, applies it.
pub fn run(ctx: Arc<AppCtx>, o: AdoptOpts) -> Result<i32> {
    let config = ctx.load_config()?;
    let profiles = config.profiles(&ctx.paths);
    let profile = match profiles.iter().find(|p| p.id == o.target) {
        Some(p) => p.clone(),
        None => {
            let dir = ctx.paths.expand(&o.target);
            let Some(p) = profiles.iter().find(|p| p.dir == dir).cloned() else {
                bail!("{} is not a configured profile id or dir", o.target);
            };
            p
        }
    };
    if profile.is_default_install {
        println!("{} is the common home; nothing to adopt.", profile.id);
        return Ok(0);
    }
    let common = profile.common_home.clone();
    let overlay = ctx.paths.profile_overlay(&profile.id);
    let rules = config.share_rules(profile.tool);
    let plan = share::plan_adopt(&profile.id, &profile.dir, &common, &rules, &overlay)?;
    println!("{}", plan.summary());
    let parked = common.join(".agent-prism-conflicts").join(&profile.id);
    let mut memory = 0;
    for a in &plan.actions {
        if let Action::Merge { name, preview } = a {
            for c in &preview.conflicts {
                println!("  conflict  {name}: {}", c.display());
            }
            for m in &preview.memory_conflicts {
                println!(
                    "  memory conflict  {} -> {}",
                    m.display(),
                    parked.join(m).display()
                );
            }
            memory += preview.memory_conflicts.len();
        }
    }
    if memory > 0 {
        println!(
            "  {memory} memory file(s) differ from the shared copy. Adopt keeps the shared \
             copy and does not edit MEMORY.md; reconcile the copies after adopt."
        );
    }
    if plan.is_noop() {
        return Ok(0);
    }
    if !o.apply {
        println!("\nDry run. Apply with: agp adopt {} --apply", profile.id);
        return Ok(0);
    }
    if !o.force && procs::profile_in_use(&profile, &procs::running(profile.tool)) {
        bail!(
            "{} is running on {}; quit it first (or use --force)",
            profile.tool.bin(),
            profile.id
        );
    }
    if profile.tool == prism_core::model::Tool::Codex {
        println!(
            "Quit the Codex app and any `codex` on {} first: its background server \
             keeps files open.",
            profile.id
        );
    }
    if std::io::stdin().is_terminal() && !confirm("Apply?") {
        println!("Cancelled.");
        return Ok(1);
    }
    if o.no_backup {
        println!("Skipping the backup (--no-backup).");
    } else {
        let target = BackupTarget::parse(&config.backup.target, &ctx.paths)?;
        let where_ = backup::backup_profile(
            &target,
            &profile.id,
            &profile.dir,
            &common.join("history.jsonl"),
        )
        .map_err(|e| e.context("backup failed; nothing changed (use --no-backup to skip)"))?;
        println!("Backup: {where_}");
    }
    let journal = share::apply_adopt(&plan, &ctx.paths.adopt_dir())?;
    let n = plan
        .actions
        .iter()
        .filter(|a| !matches!(a, Action::Skip { .. }))
        .count();
    println!(
        "Applied {n} actions ({} journal entries). Journal: {}",
        journal.entries.len(),
        PathBuf::from(&journal.path).display()
    );
    let moved: Vec<_> = journal.memory_conflicts().collect();
    if !moved.is_empty() {
        println!(
            "\n{} memory conflict(s) to reconcile (the shared memory was not edited):",
            moved.len()
        );
        for (_, dst) in moved {
            println!("  {}", dst.display());
        }
    }
    Ok(0)
}
