//! `agp doctor`: checks the setup and suggests fixes. Never prints tokens.

use std::process::Command;
use std::sync::Arc;

use anyhow::Result;
use prism_core::config::Config;
use prism_core::keychain;
use prism_core::model::{TokenState, Tool};
use prism_core::share::{self, Action};

use crate::app::AppCtx;
use crate::launch::unix_now;

/// Runs the checks; exit code 1 when something needs fixing.
pub fn run(ctx: Arc<AppCtx>) -> Result<i32> {
    let mut tips: Vec<String> = Vec::new();
    let file = ctx.paths.config_file();
    let config = match Config::load(&ctx.paths) {
        Ok(Some(c)) => {
            println!("config     {} (ok)", file.display());
            c
        }
        Ok(None) => {
            println!("config     {} (missing)", file.display());
            tips.push("run agp setup to create the config".into());
            Config::default()
        }
        Err(e) => {
            println!("config     {} (invalid: {e:#})", file.display());
            tips.push("fix the config file".into());
            Config::default()
        }
    };
    let profiles = config.profiles(&ctx.paths);
    let has_codex = profiles.iter().any(|p| p.tool == Tool::Codex);
    for tool in Tool::ALL {
        if tool == Tool::Codex && !has_codex {
            continue;
        }
        let common = config.common_home_for(&ctx.paths, tool);
        println!(
            "common     {} ({}){}",
            common.display(),
            if common.is_dir() { "ok" } else { "missing" },
            if has_codex {
                format!(" · {}", tool.name())
            } else {
                String::new()
            }
        );
    }
    println!("state      {}", ctx.paths.state_dir.display());
    let now = unix_now();
    let claude = config.profiles_for(&ctx.paths, Tool::Claude);
    for p in profiles {
        let common = &p.common_home;
        let rules = config.share_rules(p.tool);
        println!(
            "\nprofile    {} ({}){}{}",
            p.id,
            p.label,
            if p.tool == Tool::Codex {
                " · Codex"
            } else {
                ""
            },
            if p.pool { " · pool" } else { "" }
        );
        let exists = p.dir.is_dir();
        println!(
            "  dir      {} ({})",
            p.dir.display(),
            if exists { "ok" } else { "missing" }
        );
        if !exists {
            tips.push(format!(
                "{}: create {} or fix its dir",
                p.id,
                p.dir.display()
            ));
            continue;
        }
        let id = prism_core::identity::read_identity(&p, &ctx.paths);
        println!(
            "  identity {} · {}",
            id.email.as_deref().unwrap_or("not logged in"),
            id.org.as_deref().unwrap_or("—")
        );
        match p.tool {
            Tool::Claude => {
                println!("  keychain {}", p.keychain_service());
                if let Some((own, missing)) = prism_core::trust::status(&ctx.paths, &p, &claude) {
                    println!(
                        "  trust    {own} folder(s) · {missing} trusted on other accounts only{}",
                        if missing > 0 && config.share.sync_trust {
                            " (added at next start)"
                        } else {
                            ""
                        }
                    );
                }
            }
            Tool::Codex => {
                let auth = p.codex_auth_path();
                if let Ok(prism_core::codex::CodexLogin::ApiKey) =
                    prism_core::codex::read_login(&p.dir)
                {
                    tips.push(format!(
                        "{}: this is an API-key login; log in with ChatGPT (agp add {})",
                        p.id, p.id
                    ));
                }
                println!("  auth     {}", auth.display());
            }
        }
        let token = match keychain::read_credentials(ctx.store.as_ref(), &p) {
            Ok(c) => keychain::token_state(c.as_ref(), now),
            Err(e) => TokenState::Unreadable(format!("{e:#}")),
        };
        let t = match &token {
            TokenState::Valid { .. } => "valid".to_string(),
            TokenState::Expired { since } => {
                format!(
                    "expired {} ago",
                    prism_usage::balance::fmt_duration(now - since)
                )
            }
            TokenState::Missing => {
                tips.push(match p.tool {
                    Tool::Claude => {
                        format!("{}: log in with agp add {} or claude /login", p.id, p.id)
                    }
                    Tool::Codex => format!(
                        "{}: log in with agp add {} or CODEX_HOME={} codex login",
                        p.id,
                        p.id,
                        p.dir.display()
                    ),
                });
                "missing".into()
            }
            TokenState::Unreadable(e) => format!("unreadable ({e})"),
        };
        println!("  token    {t}");
        match share::share_health(&p.dir, common, &rules) {
            Ok(h) => {
                println!(
                    "  sharing  ok {} · to fix {} · unmanaged {} · {} {}",
                    h.ok,
                    h.to_fix,
                    h.unmanaged.len(),
                    share::sessions_entry(p.tool),
                    if h.sessions_shared {
                        "shared"
                    } else {
                        "NOT shared"
                    }
                );
                if !h.unmanaged.is_empty() {
                    println!("           unmanaged: {}", h.unmanaged.join(", "));
                }
                if h.to_fix > 0 || !h.sessions_shared {
                    tips.push(format!("{}: agp adopt {} (then --apply)", p.id, p.id));
                    let overlay = ctx.paths.profile_overlay(&p.id);
                    match share::plan_adopt(&p.id, &p.dir, common, &rules, &overlay) {
                        Ok(plan) => {
                            let pending: Vec<_> = plan
                                .actions
                                .iter()
                                .filter_map(|a| match a {
                                    Action::Merge { preview, .. } => {
                                        Some(preview.memory_conflicts.iter())
                                    }
                                    _ => None,
                                })
                                .flatten()
                                .collect();
                            if !pending.is_empty() {
                                println!(
                                    "  memory   {} conflict(s) on adopt (go to .agent-prism-conflicts):",
                                    pending.len()
                                );
                                for m in pending {
                                    println!("           {}", m.display());
                                }
                            }
                        }
                        Err(e) => println!("  memory   plan error: {e:#}"),
                    }
                }
            }
            Err(e) => println!("  sharing  error: {e:#}"),
        }
        match share::parked_memory_conflicts(common, &p.id) {
            Ok(parked) if !parked.is_empty() => {
                println!(
                    "  memory   {} conflict copy(ies) to reconcile:",
                    parked.len()
                );
                for m in &parked {
                    println!("           {}", m.display());
                }
                tips.push(format!(
                    "{}: reconcile the memory conflict copies in {}, then delete them",
                    p.id,
                    common.join(".agent-prism-conflicts").join(&p.id).display()
                ));
            }
            Ok(_) => {}
            Err(e) => println!("  memory   error: {e:#}"),
        }
    }
    println!();
    for tool in Tool::ALL {
        match ctx.tool_bin(&config, tool) {
            Ok(bin) => {
                let version = Command::new(&bin)
                    .arg("--version")
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_else(|| "unknown version".into());
                println!("{:<10} {} ({version})", tool.bin(), bin.display());
            }
            // Codex is optional: only a problem when a Codex account exists.
            Err(_) if tool == Tool::Codex && !has_codex => {
                println!("codex      not found (optional)");
            }
            Err(e) => {
                println!("{:<10} not found ({e:#})", tool.bin());
                tips.push(format!("install {0} or set {0}.bin", tool.bin()));
            }
        }
    }
    if tips.is_empty() {
        println!("\nAll good.");
        Ok(0)
    } else {
        println!("\nSuggestions:");
        for t in &tips {
            println!("  - {t}");
        }
        Ok(1)
    }
}
