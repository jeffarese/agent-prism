//! agent-prism — wraps `claude` (or `codex`) and balances sessions across Claude
//! (or ChatGPT) accounts.
//! See docs/DESIGN.md.
//!
//! Environment hooks for automated tests only:
//! - `AGP_SECRET_STORE_FILE=<json map service→value>`: read credentials
//!   from this file instead of `/usr/bin/security`.
//! - `AGP_NO_POLL=1`: no usage poller and no launch-time refresh; the
//!   cache files are used as they are.

mod actions;
mod adopt_cmd;
mod app;
mod cli;
mod codex;
mod doctor;
mod hooks;
mod host;
mod launch;
mod live;
mod logging;
mod session;
mod status;
mod tokens;
mod ui;

use std::ffi::OsString;
use std::process::Command as Process;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::app::AppCtx;
use crate::cli::Command;

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let code = match run(args) {
        Ok(code) => code,
        Err(e) => {
            log::error!("{e:#}");
            eprintln!("agp: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run(args: Vec<OsString>) -> Result<i32> {
    let cmd = cli::parse(&args)?;
    // Hooks must be fast and silent: no context, no logger.
    if let Command::Hook(event) = &cmd {
        return Ok(hooks::run_client(event));
    }
    match cmd {
        Command::Help => {
            print!("{}", cli::HELP);
            return Ok(0);
        }
        Command::Version => {
            println!("agent-prism {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        _ => {}
    }
    let ctx = Arc::new(AppCtx::from_env()?);
    logging::init(ctx.paths.log_file());
    match cmd {
        Command::Launch(l) => session::run(ctx, l),
        Command::Status { json, watch } => status::run_status(ctx, json, watch),
        Command::Pick { json, tool } => status::run_pick(ctx, json, tool),
        Command::Settings => ui::run_settings(ctx).map(|()| 0),
        Command::Setup => ui::run_onboarding(ctx).map(|_| 0),
        Command::Add { id, tool } => ui::run_add(ctx, id, tool).map(|()| 0),
        Command::Doctor => doctor::run(ctx),
        Command::Adopt {
            target,
            apply,
            no_backup,
            force,
        } => adopt_cmd::run(
            ctx,
            adopt_cmd::AdoptOpts {
                target,
                apply,
                no_backup,
                force,
            },
        ),
        Command::Exec { profile, cmd } => run_exec(&ctx, &profile, &cmd),
        Command::Hook(_) | Command::Help | Command::Version => Ok(0),
    }
}

/// `agp exec <id> -- <cmd…>`: runs `cmd` with the profile's env.
fn run_exec(ctx: &AppCtx, id: &str, cmd: &[String]) -> Result<i32> {
    let config = ctx.load_config()?;
    let profile = config
        .profiles(&ctx.paths)
        .into_iter()
        .find(|p| p.id == id)
        .with_context(|| format!("no profile {id:?} in the config"))?;
    let (set, remove) = launch::child_env(&profile, std::path::Path::new(""));
    let mut p = Process::new(&cmd[0]);
    p.args(&cmd[1..]);
    for k in remove {
        p.env_remove(k);
    }
    p.envs(set);
    let status = p.status().with_context(|| format!("running {}", cmd[0]))?;
    Ok(status.code().unwrap_or(1))
}
