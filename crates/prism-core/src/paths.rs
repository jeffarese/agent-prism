//! Default locations of agent-prism's config and state (DESIGN §2).

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

/// The name of the config and state dirs, the log and the conflicts dir.
pub const NAME: &str = "agent-prism";

/// The roots agent-prism works under. Library code takes these as parameters so
/// tests can point everything at a temp dir.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

fn env_dir(name: &str) -> Option<PathBuf> {
    crate::env::var_os(name).map(PathBuf::from)
}

impl Paths {
    /// Resolve from `HOME`, `AGP_CONFIG_DIR`/`XDG_CONFIG_HOME` and
    /// `AGP_STATE_DIR`/`XDG_STATE_HOME`.
    pub fn from_env() -> Result<Paths> {
        let home = env_dir("HOME")
            .or_else(dirs::home_dir)
            .context("cannot determine the home directory")?;
        let config_dir = env_dir("AGP_CONFIG_DIR")
            .or_else(|| env_dir("XDG_CONFIG_HOME").map(|d| d.join(NAME)))
            .unwrap_or_else(|| home.join(".config").join(NAME));
        let state_dir = env_dir("AGP_STATE_DIR")
            .or_else(|| env_dir("XDG_STATE_HOME").map(|d| d.join(NAME)))
            .unwrap_or_else(|| home.join(".local/state").join(NAME));
        Ok(Paths {
            home,
            config_dir,
            state_dir,
        })
    }

    /// Paths rooted in `root` (for tests): `root/home`, `root/config`, `root/state`.
    pub fn under(root: &Path) -> Paths {
        Paths {
            home: root.join("home"),
            config_dir: root.join("config"),
            state_dir: root.join("state"),
        }
    }

    /// `<config_dir>/config.toml`.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// `<state>/usage`.
    pub fn usage_dir(&self) -> PathBuf {
        self.state_dir.join("usage")
    }

    /// `<state>/sessions` (live-session registry).
    pub fn sessions_dir(&self) -> PathBuf {
        self.state_dir.join("sessions")
    }

    /// `<state>/run/<pid>`.
    pub fn run_dir(&self, pid: u32) -> PathBuf {
        self.state_dir.join("run").join(pid.to_string())
    }

    /// `<state>/adopt` (adopt journals).
    pub fn adopt_dir(&self) -> PathBuf {
        self.state_dir.join("adopt")
    }

    /// `<state>/agent-prism.log`.
    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join(format!("{NAME}.log"))
    }

    /// `<config_dir>/profiles/<id>/settings.json` (settings overlay).
    pub fn profile_overlay(&self, id: &str) -> PathBuf {
        self.config_dir
            .join("profiles")
            .join(id)
            .join("settings.json")
    }

    /// Expand `~`/`~/x` against `home`, make the path absolute (relative to
    /// `home`) and drop trailing slashes and `.` components.
    pub fn expand(&self, s: &str) -> PathBuf {
        let raw = if s == "~" {
            self.home.clone()
        } else if let Some(rest) = s.strip_prefix("~/") {
            self.home.join(rest)
        } else {
            PathBuf::from(s)
        };
        let abs = if raw.is_absolute() {
            raw
        } else {
            self.home.join(raw)
        };
        abs.components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_forms() {
        let p = Paths::under(Path::new("/r"));
        assert_eq!(p.expand("~"), PathBuf::from("/r/home"));
        assert_eq!(
            p.expand("~/.claude-work/"),
            PathBuf::from("/r/home/.claude-work")
        );
        assert_eq!(p.expand("/abs/x/"), PathBuf::from("/abs/x"));
        assert_eq!(
            p.expand("/abs/x/").to_str(),
            Some("/abs/x"),
            "no trailing slash"
        );
        assert_eq!(
            p.profile_overlay("a"),
            PathBuf::from("/r/config/profiles/a/settings.json")
        );
    }
}
