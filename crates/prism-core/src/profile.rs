//! A resolved profile (one Claude or Codex login = one config dir).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::model::Tool;
use crate::paths::Paths;

/// A profile from the config with its dir expanded (absolute, no trailing slash).
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub id: String,
    pub tool: Tool,
    pub label: String,
    pub dir: PathBuf,
    /// The dir is the common home: launch without `CLAUDE_CONFIG_DIR`
    /// (or `CODEX_HOME`).
    pub is_default_install: bool,
    /// The common home of the tool (`~/.claude` or `~/.codex`, expanded).
    pub common_home: PathBuf,
    pub pool: bool,
    pub weight: f64,
    pub reserve_7d: u8,
    pub color: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl Profile {
    /// The Keychain service that holds this profile's OAuth credentials.
    pub fn keychain_service(&self) -> String {
        keychain_service_for(&self.dir, self.is_default_install)
    }

    /// `<dir>/.claude.json`, or `<home>/.claude.json` for the default install.
    pub fn claude_json_path(&self, paths: &Paths) -> PathBuf {
        if self.is_default_install {
            paths.home.join(".claude.json")
        } else {
            self.dir.join(".claude.json")
        }
    }

    /// The value for `CLAUDE_CONFIG_DIR` (or `CODEX_HOME`); `None` for the
    /// default install, where the caller must remove the variable instead.
    pub fn config_dir_env(&self) -> Option<&Path> {
        (!self.is_default_install).then_some(self.dir.as_path())
    }

    /// `<dir>/auth.json`: the Codex login.
    pub fn codex_auth_path(&self) -> PathBuf {
        self.dir.join("auth.json")
    }

    /// Where Codex keeps its databases (threads) for this profile: the
    /// common home, so that `codex resume` finds a thread from any account.
    /// A `CODEX_SQLITE_HOME` in the profile env wins.
    pub fn codex_sqlite_home(&self) -> PathBuf {
        self.env
            .get("CODEX_SQLITE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.common_home.clone())
    }

    /// A profile for a dir that is not in the config (discovery, tests).
    pub fn bare(id: &str, tool: Tool, dir: PathBuf, common_home: PathBuf) -> Profile {
        Profile {
            id: id.to_string(),
            tool,
            label: id.to_string(),
            is_default_install: dir == common_home,
            dir,
            common_home,
            pool: false,
            weight: 1.0,
            reserve_7d: 0,
            color: None,
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }
}

/// Keychain service name for a config dir (DESIGN §6.1). `dir` must be the
/// exact absolute string used as `CLAUDE_CONFIG_DIR`.
pub fn keychain_service_for(dir: &Path, is_default_install: bool) -> String {
    const BASE: &str = "Claude Code-credentials";
    if is_default_install {
        return BASE.to_string();
    }
    let digest = Sha256::digest(dir.to_string_lossy().as_bytes());
    let hex = hex::encode(digest);
    format!("{BASE}-{}", &hex[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_vectors() {
        assert_eq!(
            keychain_service_for(Path::new("/Users/me/.claude-work"), false),
            "Claude Code-credentials-1e91dd84"
        );
        assert_eq!(
            keychain_service_for(Path::new("/Users/me/.claude-home"), false),
            "Claude Code-credentials-66a87874"
        );
        assert_eq!(
            keychain_service_for(Path::new("/Users/me/.claude"), true),
            "Claude Code-credentials"
        );
    }
}
