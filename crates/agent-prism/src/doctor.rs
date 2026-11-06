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
    platform_lines(has_codex, &mut tips);
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
        // The account's own settings.json gets its own line, not adopt:
        // adopt would keep its values as the account's overrides.
        let mut own_settings = false;
        match p.tool {
            Tool::Claude => {
                let (label, place) = keychain::login_location(&p);
                println!("  {label:<8} {place}");
                own_settings = settings_line(&ctx.paths, &config, &p, &mut tips);
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
            Ok(mut h) => {
                h.to_fix -= usize::from(own_settings && h.to_fix > 0);
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

/// Prints how `p`'s `settings.json` stands; true when it is a file of the
/// account's own.
fn settings_line(
    paths: &prism_core::paths::Paths,
    config: &Config,
    p: &prism_core::profile::Profile,
    tips: &mut Vec<String>,
) -> bool {
    use prism_core::settings_sync::{SETTINGS, State};
    if !prism_core::settings_sync::shared(config) {
        println!("  settings per account ([share] extra_local)");
        return false;
    }
    let next = if config.share.sync_settings {
        " (merged at next start)"
    } else {
        " ([share] sync_settings is off)"
    };
    match prism_core::settings_sync::state(paths, p) {
        State::Linked => println!("  settings shared"),
        State::Other => println!("  settings not shared (missing or linked elsewhere)"),
        State::Own { differs, base } => {
            let what = if differs.is_empty() {
                "same as common".to_string()
            } else {
                format!("differs in {}", differs.join(", "))
            };
            if base || differs.is_empty() {
                println!("  settings own copy · {what}{next}");
            } else {
                println!("  settings own copy · {what} · from before agp kept a base");
                let common = p.common_home.join(SETTINGS);
                tips.push(format!(
                    "{}: its settings.json differs from {} in {}; put the values you want \
                     in that file, then run: ln -sf {} {}",
                    p.id,
                    common.display(),
                    differs.join(", "),
                    common.display(),
                    p.dir.join(SETTINGS).display()
                ));
            }
            return true;
        }
    }
    false
}

/// The system, and what agent-prism needs from it there.
fn platform_lines(has_codex: bool, tips: &mut Vec<String>) {
    use prism_core::platform::{self, Os};
    let os = platform::os();
    let logins = if platform::has_keychain() {
        "Claude logins in the Keychain"
    } else {
        "Claude logins in <dir>/.credentials.json"
    };
    println!("platform   {} ({logins})", os.label());
    if has_codex && platform::sqlite3().is_none() {
        println!("  sqlite3  missing: codex resume can't find threads");
        tips.push(match os {
            Os::MacOs => "install sqlite3 (it ships with macOS at /usr/bin/sqlite3)".into(),
            _ => "install sqlite3 (apt install sqlite3, dnf install sqlite)".into(),
        });
    }
    if os == Os::Wsl
        && let Ok(cwd) = std::env::current_dir()
        && cwd.starts_with("/mnt")
    {
        tips.push(format!(
            "{} is on the Windows drive: Claude and git are much faster in the Linux file system (~/…)",
            cwd.display()
        ));
    }
}
