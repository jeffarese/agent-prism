//! Read-only: print the adopt plan and share health of a profile dir
//! against `~/.claude` (or `~/.codex` for a `.codex-*` dir).
//! Usage: `cargo run -p prism-core --example plan -- ~/.claude-work`
use prism_core::{config::Config, model::Tool, paths::Paths, share};

fn main() -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let arg = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "~/.claude-work".into());
    let dir = paths.expand(&arg);
    let tool = if arg.contains(".codex") {
        Tool::Codex
    } else {
        Tool::Claude
    };
    let config = Config::default();
    let common = config.common_home_for(&paths, tool);
    let cfg = config.share_rules(tool);
    let id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let plan = share::plan_adopt(&id, &dir, &common, &cfg, &paths.profile_overlay(&id))?;
    println!("{}", plan.summary());
    for a in &plan.actions {
        if let share::Action::Merge { preview, .. } = a {
            for c in &preview.conflicts {
                println!("  conflict: {}", c.display());
            }
            for m in &preview.memory_conflicts {
                println!("  memory conflict: {}", m.display());
            }
        }
    }
    println!("{:?}", share::share_health(&dir, &common, &cfg)?);
    Ok(())
}
