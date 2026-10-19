//! Create a new profile dir with the share map (symlinks into ~/.claude, or
//! ~/.codex for a `.codex-*` dir, and empty local dirs).
//! Usage: `cargo run -p prism-core --example mkprofile -- ~/.claude-x`
use prism_core::{config::Config, model::Tool, paths::Paths, share};

fn main() -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let arg = std::env::args().nth(1).expect("profile dir");
    let dir = paths.expand(&arg);
    let tool = if arg.contains(".codex") {
        Tool::Codex
    } else {
        Tool::Claude
    };
    let cfg = Config::default();
    let common = cfg.common_home_for(&paths, tool);
    share::create_profile_dir(&dir, &common, &cfg.share_rules(tool))?;
    println!("created {}", dir.display());
    Ok(())
}
