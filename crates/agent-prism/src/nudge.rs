//! `token_refresh = "nudge"` (DESIGN §6.3): keeps the logins of idle
//! Claude accounts fresh without a model call, so their usage can be polled
//! and `agp status` shows them as fresh rather than `idle`.
//!
//! An access token lasts about 8 hours, and only Claude refreshes it, when
//! it next runs on the account. agent-prism never writes the Keychain
//! itself. Instead it runs `claude mcp get <made-up name>` on the account.
//! Before it looks the name up, Claude fetches the claude.ai connector list,
//! and that fetch refreshes an expired token through Claude's own locked
//! refresh path. The unknown name then ends the command (always with exit
//! code 1). No MCP server is started and no model is called; this was
//! checked with Claude 2.1.316. If a later Claude stops doing this, nothing breaks: the
//! token stays expired, the log says so, and the next real run refreshes it.
//!
//! The nudge runs detached (`agp _nudge <id>`): at most one per account at
//! a time, and at most one every [`GAP_SECS`]. It needs the user's GUI
//! login session, where the Keychain is readable; over SSH the token can't
//! be read, so no nudge starts.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use prism_core::config::{Config, TokenRefresh};
use prism_core::model::Tool;
use prism_core::paths::Paths;
use prism_core::profile::Profile;
use prism_usage::tokens::TokenSource;
use serde::{Deserialize, Serialize};

use crate::app::AppCtx;
use crate::launch::{self, unix_now};
use crate::tokens::KeychainTokens;

/// The MCP server name `claude mcp get` looks up. No such server exists.
pub const PROBE: &str = "agp-refresh-probe";
/// At most one nudge per account this often.
pub const GAP_SECS: i64 = 30 * 60;
/// Claude refreshes a token only this close to its expiry, or after it.
const EARLY_SECS: i64 = 5 * 60;
/// A nudge that runs longer is killed.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The last nudge of an account, in `<state>/nudge/<id>.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Last {
    /// When it started (unix seconds).
    pub at: i64,
    /// The token's expiry before and after it (unix seconds).
    pub before: Option<i64>,
    pub after: Option<i64>,
}

fn dir(paths: &Paths) -> PathBuf {
    paths.state_dir.join("nudge")
}

fn file(paths: &Paths, id: &str, ext: &str) -> PathBuf {
    dir(paths).join(format!("{id}.{ext}"))
}

/// The last nudge of `id`, if any.
pub fn last(paths: &Paths, id: &str) -> Option<Last> {
    let text = std::fs::read_to_string(file(paths, id, "json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn store(paths: &Paths, id: &str, last: &Last) {
    let path = file(paths, id, "json");
    let tmp = file(paths, id, &format!("{}.tmp", std::process::id()));
    let res = std::fs::create_dir_all(dir(paths))
        .and_then(|()| std::fs::write(&tmp, serde_json::to_vec(last).unwrap_or_default()))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        log::warn!("token nudge {id}: {e}");
    }
}

/// The lock a running nudge of `id` holds (`None`: held elsewhere).
fn try_lock(paths: &Paths, id: &str) -> Option<File> {
    std::fs::create_dir_all(dir(paths)).ok()?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(file(paths, id, "lock"))
        .ok()?;
    fs4::FileExt::try_lock(&f).ok()?;
    Some(f)
}

/// A nudge is due: the token expired (or expires within minutes) and the
/// last nudge was [`GAP_SECS`] ago or more.
pub fn due(last: Option<&Last>, expires_at: Option<i64>, now: i64) -> bool {
    let Some(exp) = expires_at else {
        return false;
    };
    exp - now <= EARLY_SECS && last.is_none_or(|l| now - l.at >= GAP_SECS)
}

/// Starts a detached nudge of `p` when the config asks for nudges and one
/// is due (`expires_at`: the token's expiry). Never fails the caller.
pub fn maybe_start(paths: &Paths, config: &Config, p: &Profile, expires_at: Option<i64>, now: i64) {
    if config.usage.token_refresh != TokenRefresh::Nudge
        || p.tool != Tool::Claude
        || launch::polling_disabled()
    {
        return;
    }
    // Held while deciding, so that callers that look together start one.
    let Some(lock) = try_lock(paths, &p.id) else {
        return;
    };
    if !due(last(paths, &p.id).as_ref(), expires_at, now) {
        return;
    }
    store(
        paths,
        &p.id,
        &Last {
            at: now,
            before: expires_at,
            after: None,
        },
    );
    drop(lock);
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.args(["_nudge", &p.id]);
    match crate::bg::spawn_detached(cmd) {
        Ok(pid) => log::info!("token nudge for {} started (pid {pid})", p.id),
        Err(e) => log::warn!("token nudge for {}: {e:#}", p.id),
    }
}

/// `agp _nudge <id>` (internal): runs the probe on the account, then reads
/// its token again and polls its usage.
pub fn run(ctx: &AppCtx, id: &str) -> Result<i32> {
    let config = ctx.load_config()?;
    let p = config
        .profiles_for(&ctx.paths, Tool::Claude)
        .into_iter()
        .find(|p| p.id == id)
        .with_context(|| format!("no Claude account {id:?}"))?;
    let Some(_lock) = try_lock(&ctx.paths, id) else {
        log::info!("token nudge {id}: another one runs");
        return Ok(0);
    };
    let now = unix_now();
    let expiry = || {
        KeychainTokens::new(ctx.store.clone(), vec![p.clone()])
            .token(id)
            .ok()
            .flatten()
            .and_then(|t| t.expires_at)
    };
    let before = expiry();
    if before.is_none_or(|e| e - now > EARLY_SECS) {
        log::info!("token nudge {id}: nothing to do");
        return Ok(0);
    }
    let status = probe(ctx, &config, &p);
    let after = expiry();
    store(
        &ctx.paths,
        id,
        &Last {
            at: now,
            before,
            after,
        },
    );
    match after {
        Some(a) if a - unix_now() > EARLY_SECS => {
            log::info!("token nudge {id}: refreshed (probe {status})");
            let mut fresh = config.clone();
            fresh.usage.poll_active_secs = 0;
            let tokens = Arc::new(KeychainTokens::new(ctx.store.clone(), vec![p.clone()]));
            launch::refresh_stale(
                &ctx.paths,
                &ctx.cache,
                &fresh,
                std::slice::from_ref(&p),
                &tokens,
                Duration::from_secs(10),
            );
        }
        _ => log::warn!(
            "token nudge {id}: the token is still expired (probe {status}); \
             the next run on the account refreshes it"
        ),
    }
    Ok(0)
}

/// Runs `claude mcp get <PROBE>` on `p` in an empty folder, all output
/// dropped, killed after [`TIMEOUT`]. Returns a word for the log.
fn probe(ctx: &AppCtx, config: &Config, p: &Profile) -> String {
    let bin = match ctx.claude_bin(config) {
        Ok(b) => b,
        Err(e) => return format!("not started: {e:#}"),
    };
    let cwd = dir(&ctx.paths).join("empty");
    if let Err(e) = std::fs::create_dir_all(&cwd) {
        return format!("not started: {e}");
    }
    let mut cmd = Command::new(bin);
    cmd.args(["mcp", "get", PROBE])
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let (set, remove) = launch::child_env(p, Path::new(""));
    for k in remove {
        cmd.env_remove(k);
    }
    cmd.envs(set);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("not started: {e}"),
    };
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return format!("exited {}", st.code().unwrap_or(-1)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return "killed after 60 s".into();
            }
            Err(e) => return format!("lost: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_due() {
        let now = 100_000;
        // Valid for an hour: Claude would not refresh it.
        assert!(!due(None, Some(now + 3600), now));
        // Expired, or about to: due unless a nudge ran within the gap.
        assert!(due(None, Some(now - 10), now));
        assert!(due(None, Some(now + 60), now));
        let recent = Last {
            at: now - 60,
            ..Default::default()
        };
        assert!(!due(Some(&recent), Some(now - 10), now));
        let old = Last {
            at: now - GAP_SECS,
            ..Default::default()
        };
        assert!(due(Some(&old), Some(now - 10), now));
        // No expiry known: nothing to refresh.
        assert!(!due(None, None, now));
    }

    #[test]
    fn last_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        assert_eq!(last(&paths, "a"), None);
        let l = Last {
            at: 5,
            before: Some(1),
            after: Some(9),
        };
        store(&paths, "a", &l);
        assert_eq!(last(&paths, "a"), Some(l));
        assert!(try_lock(&paths, "a").is_some());
    }

    #[test]
    fn off_unless_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let p = Profile::bare(
            "a",
            Tool::Claude,
            tmp.path().join("a"),
            tmp.path().join("c"),
        );
        // The default (`lazy`) never starts a nudge, and leaves no trace.
        maybe_start(&paths, &Config::default(), &p, Some(1), 100_000);
        assert_eq!(last(&paths, "a"), None);
    }
}
