//! `config.toml` (DESIGN §3).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::{DockIcons, DockMode, OnLimit, Policy, Tool};
use crate::paths::Paths;
use crate::profile::Profile;

/// The whole config file. Every key is optional.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub common_home: String,
    pub claude: ClaudeConfig,
    pub codex: CodexConfig,
    pub balance: BalanceConfig,
    pub usage: UsageConfig,
    pub dock: DockConfig,
    pub keys: KeysConfig,
    pub backup: BackupConfig,
    #[serde(rename = "profile", skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ProfileConfig>,
    pub share: ShareConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            common_home: "~/.claude".into(),
            claude: ClaudeConfig::default(),
            codex: CodexConfig::default(),
            balance: BalanceConfig::default(),
            usage: UsageConfig::default(),
            dock: DockConfig::default(),
            keys: KeysConfig::default(),
            backup: BackupConfig::default(),
            profiles: Vec::new(),
            share: ShareConfig::default(),
        }
    }
}

/// `[claude]`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    /// Empty = first `claude` on PATH that is not agent-prism.
    pub bin: String,
    pub args: Vec<String>,
}

/// `[codex]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexConfig {
    /// The Codex home every Codex account shares (like `common_home`).
    pub common_home: String,
    /// Empty = first `codex` on PATH.
    pub bin: String,
    pub args: Vec<String>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        CodexConfig {
            common_home: "~/.codex".into(),
            bin: String::new(),
            args: Vec::new(),
        }
    }
}

/// `[balance]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BalanceConfig {
    pub policy: Policy,
    pub on_limit: OnLimit,
    pub countdown_secs: u32,
    pub resume_prompt: String,
    pub soft_5h: f64,
    pub hard: f64,
}

impl Default for BalanceConfig {
    fn default() -> Self {
        BalanceConfig {
            policy: Policy::Smart,
            on_limit: OnLimit::Auto,
            countdown_secs: 5,
            resume_prompt: "continue".into(),
            soft_5h: 90.0,
            hard: 98.0,
        }
    }
}

/// How expired tokens are handled (DESIGN §6.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenRefresh {
    #[default]
    Lazy,
    Direct,
}

/// `[usage]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageConfig {
    pub poll_active_secs: u64,
    pub poll_idle_secs: u64,
    pub token_refresh: TokenRefresh,
}

impl Default for UsageConfig {
    fn default() -> Self {
        UsageConfig {
            poll_active_secs: 60,
            poll_idle_secs: 180,
            token_refresh: TokenRefresh::Lazy,
        }
    }
}

/// Which accounts the dock lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DockShow {
    #[default]
    Pool,
    All,
}

/// `[dock]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DockConfig {
    pub mode: DockMode,
    pub show: DockShow,
    pub projections: bool,
    pub icons: DockIcons,
}

impl Default for DockConfig {
    fn default() -> Self {
        DockConfig {
            mode: DockMode::Compact,
            show: DockShow::Pool,
            projections: true,
            icons: DockIcons::Nerd,
        }
    }
}

/// `[keys]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeysConfig {
    pub menu: String,
}

impl Default for KeysConfig {
    fn default() -> Self {
        KeysConfig {
            menu: "ctrl+\\".into(),
        }
    }
}

/// `[backup]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupConfig {
    /// A local path or `ssh://host/path`.
    pub target: String,
}

impl Default for BackupConfig {
    fn default() -> Self {
        BackupConfig {
            target: "~/.local/state/agent-prism/backups".into(),
        }
    }
}

/// One `[[profile]]` as written in the file (dir not yet expanded).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileConfig {
    pub id: String,
    /// `claude` (default) or `codex`.
    #[serde(skip_serializing_if = "Tool::is_claude")]
    pub tool: Tool,
    /// Empty = use the id.
    pub label: String,
    pub dir: String,
    pub pool: bool,
    pub weight: f64,
    pub reserve_7d: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl Default for ProfileConfig {
    fn default() -> Self {
        ProfileConfig {
            id: String::new(),
            tool: Tool::Claude,
            label: String::new(),
            dir: String::new(),
            pool: true,
            weight: 1.0,
            reserve_7d: 0,
            color: None,
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }
}

/// `[share]`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShareConfig {
    pub extra_shared: Vec<String>,
    pub extra_local: Vec<String>,
}

impl Config {
    /// Read the config file; `None` if it does not exist.
    pub fn load(paths: &Paths) -> Result<Option<Config>> {
        let file = paths.config_file();
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
        };
        let cfg = toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        Ok(Some(cfg))
    }

    /// Read the config file, or the defaults if it does not exist.
    pub fn load_or_default(paths: &Paths) -> Result<Config> {
        Ok(Config::load(paths)?.unwrap_or_default())
    }

    /// Write the config atomically (temp file in the same dir + rename).
    pub fn save(&self, paths: &Paths) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serializing config")?;
        crate::fsutil::write_atomic(&paths.config_file(), text.as_bytes())
    }

    /// The profiles, resolved, in config order.
    pub fn profiles(&self, paths: &Paths) -> Vec<Profile> {
        self.profiles
            .iter()
            .map(|p| {
                let dir = paths.expand(&p.dir);
                let common = self.common_home_for(paths, p.tool);
                Profile {
                    id: p.id.clone(),
                    tool: p.tool,
                    label: if p.label.is_empty() {
                        p.id.clone()
                    } else {
                        p.label.clone()
                    },
                    is_default_install: dir == common,
                    dir,
                    common_home: common,
                    pool: p.pool,
                    weight: p.weight,
                    reserve_7d: p.reserve_7d,
                    color: p.color.clone(),
                    args: p.args.clone(),
                    env: p.env.clone(),
                }
            })
            .collect()
    }

    /// The profiles of one tool, in config order.
    pub fn profiles_for(&self, paths: &Paths, tool: Tool) -> Vec<Profile> {
        self.profiles(paths)
            .into_iter()
            .filter(|p| p.tool == tool)
            .collect()
    }

    /// The expanded common home of Claude accounts.
    pub fn common_home(&self, paths: &Paths) -> PathBuf {
        paths.expand(&self.common_home)
    }

    /// The expanded common home of `tool`'s accounts.
    pub fn common_home_for(&self, paths: &Paths, tool: Tool) -> PathBuf {
        match tool {
            Tool::Claude => self.common_home(paths),
            Tool::Codex => paths.expand(&self.codex.common_home),
        }
    }

    /// The share rules for `tool`'s accounts.
    pub fn share_rules(&self, tool: Tool) -> ShareRules {
        ShareRules {
            tool,
            extra: self.share.clone(),
        }
    }

    /// The tool's binary setting and extra args.
    pub fn tool_bin_args(&self, tool: Tool) -> (&str, &[String]) {
        match tool {
            Tool::Claude => (&self.claude.bin, &self.claude.args),
            Tool::Codex => (&self.codex.bin, &self.codex.args),
        }
    }
}

/// What a profile dir of one tool shares (DESIGN §4.2): the built-in lists
/// of the tool plus `[share]`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShareRules {
    pub tool: Tool,
    pub extra: ShareConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        assert!(Config::load(&paths).unwrap().is_none());
        let cfg = Config::default();
        cfg.save(&paths).unwrap();
        assert_eq!(Config::load(&paths).unwrap(), Some(cfg));
    }

    #[test]
    fn sample_parse() {
        let text = r#"
common_home = "~/.claude"
[balance]
policy = "round-robin"
on_limit = "ask"
[usage]
token_refresh = "direct"
[dock]
show = "all"
mode = "compact"
[backup]
target = "ssh://backup-host/backups/agent-prism"
[[profile]]
id = "acme-al"
label = "Acme Al"
dir = "~/.claude-work/"
color = "cyan"
env = { FOO = "1" }
[[profile]]
id = "home"
dir = "~/.claude"
pool = false
[[profile]]
id = "cx"
tool = "codex"
dir = "~/.codex/"
[codex]
args = ["--yolo"]
[share]
extra_local = ["state"]
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.balance.policy, Policy::RoundRobin);
        assert_eq!(cfg.balance.on_limit, OnLimit::Ask);
        assert_eq!(cfg.balance.countdown_secs, 5);
        assert_eq!(cfg.usage.token_refresh, TokenRefresh::Direct);
        assert_eq!(cfg.dock.show, DockShow::All);
        assert_eq!(cfg.dock.mode, DockMode::Compact);
        assert_eq!(cfg.share.extra_local, vec!["state"]);
        let paths = Paths::under(std::path::Path::new("/r"));
        let ps = cfg.profiles(&paths);
        assert_eq!(ps[0].dir, PathBuf::from("/r/home/.claude-work"));
        assert!(ps[0].pool && !ps[0].is_default_install);
        assert_eq!(ps[0].weight, 1.0);
        assert_eq!(ps[0].env["FOO"], "1");
        assert_eq!(ps[1].label, "home");
        assert!(ps[1].is_default_install && !ps[1].pool);
        assert_eq!(ps[2].tool, Tool::Codex);
        assert!(ps[2].is_default_install);
        assert_eq!(ps[2].common_home, PathBuf::from("/r/home/.codex"));
        assert_eq!(cfg.codex.args, vec!["--yolo"]);
        assert_eq!(cfg.profiles_for(&paths, Tool::Codex).len(), 1);
        assert_eq!(cfg.profiles_for(&paths, Tool::Claude).len(), 2);
        // `tool` is written only for Codex profiles.
        let text = toml::to_string(&cfg).unwrap();
        assert_eq!(text.matches("tool = ").count(), 1, "{text}");
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, cfg);
    }
}
