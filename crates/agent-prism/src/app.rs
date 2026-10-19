//! Process-wide context shared by the CLI commands, the session host and
//! the UI.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use prism_core::config::Config;
use prism_core::keychain::{SecretStore, SecurityCli};
use prism_core::model::Tool;
use prism_core::paths::Paths;
use prism_core::profile::Profile;
use prism_usage::cache::UsageCache;

pub struct AppCtx {
    pub paths: Paths,
    pub store: Arc<dyn SecretStore>,
    pub cache: UsageCache,
}

impl AppCtx {
    pub fn from_env() -> Result<Self> {
        let paths = Paths::from_env()?;
        let cache = UsageCache::new(paths.usage_dir());
        Ok(Self {
            paths,
            store: secret_store_from_env(),
            cache,
        })
    }

    /// The config, or the defaults when no file exists yet.
    pub fn load_config(&self) -> Result<Config> {
        Config::load_or_default(&self.paths)
    }

    pub fn profiles(&self, config: &Config) -> Vec<Profile> {
        config.profiles(&self.paths)
    }

    /// The real `claude` binary: `claude.bin` from the config, else the first
    /// executable `claude` on PATH that is not this agent-prism binary.
    pub fn claude_bin(&self, config: &Config) -> Result<PathBuf> {
        self.tool_bin(config, Tool::Claude)
    }

    /// The real binary of `tool`: `claude.bin` / `codex.bin` from the
    /// config, else the first executable of that name on PATH that is not
    /// this agent-prism binary.
    pub fn tool_bin(&self, config: &Config, tool: Tool) -> Result<PathBuf> {
        let (bin, _) = config.tool_bin_args(tool);
        if !bin.is_empty() {
            return Ok(self.paths.expand(bin));
        }
        let me = std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok());
        let path = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(tool.bin());
            if is_executable(&cand) && cand.canonicalize().ok() != me {
                return Ok(cand);
            }
        }
        let name = tool.bin();
        bail!("{name} not found on PATH; set {name}.bin in the agent-prism config")
    }
}

/// The secret store: `/usr/bin/security`, or — for automated tests only —
/// a JSON file `{service: value}` named by `AGP_SECRET_STORE_FILE`.
fn secret_store_from_env() -> Arc<dyn SecretStore> {
    match prism_core::env::var_os("AGP_SECRET_STORE_FILE") {
        Some(f) => Arc::new(crate::tokens::FileStore::new(f.into())),
        None => Arc::new(SecurityCli),
    }
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
