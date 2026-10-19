//! Detect running `claude` (or `codex`) processes and their config dir
//! (DESIGN §4.5).

use std::path::PathBuf;
use std::process::Command;

use crate::model::Tool;
use crate::profile::Profile;

/// A running `claude` (or `codex`) of the current user.
#[derive(Clone, Debug, PartialEq)]
pub struct RunningClaude {
    pub pid: u32,
    /// `CLAUDE_CONFIG_DIR` (or `CODEX_HOME`) from its environment; `None` =
    /// default install (only meaningful when `env_readable`).
    pub config_dir: Option<PathBuf>,
    pub env_readable: bool,
}

/// Best-effort list of the current user's `claude` processes.
pub fn running_claudes() -> Vec<RunningClaude> {
    running(Tool::Claude)
}

/// Best-effort list of the current user's processes of `tool`.
pub fn running(tool: Tool) -> Vec<RunningClaude> {
    let (Some(comm), Some(ucomm)) = (ps_column("comm="), ps_column("ucomm=")) else {
        return Vec::new();
    };
    tool_pids(tool.bin(), &comm, &ucomm, std::process::id())
        .into_iter()
        .map(|pid| inspect(tool, pid))
        .collect()
}

/// `(pid, value)` for every process of the user from `ps -xo pid=,<col>`.
/// The value is the rest of the line, so it may contain spaces.
fn ps_column(col: &str) -> Option<Vec<(u32, String)>> {
    let out = Command::new("ps")
        .args(["-xo", &format!("pid=,{col}")])
        .output()
        .ok()?;
    Some(parse_ps(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_ps(text: &str) -> Vec<(u32, String)> {
    text.lines()
        .filter_map(|l| {
            let (pid, val) = l.trim_start().split_once(char::is_whitespace)?;
            Some((pid.parse().ok()?, val.trim().to_string()))
        })
        .collect()
}

/// Pids whose argv[0] (`comm`) has the base name `bin`, except `me` and
/// every process that runs the same executable (`ucomm`) as `me`: agent-prism runs
/// as `argv[0] = claude` (or `codex`) so terminals show it as that tool.
fn tool_pids(bin: &str, comm: &[(u32, String)], ucomm: &[(u32, String)], me: u32) -> Vec<u32> {
    let exe_of = |pid: u32| ucomm.iter().find(|(p, _)| *p == pid).map(|(_, u)| u);
    let my_exe = exe_of(me);
    let wrapper = |pid: u32| my_exe.is_some() && exe_of(pid) == my_exe;
    comm.iter()
        .filter(|(pid, comm)| {
            let base = comm.rsplit('/').next().unwrap_or(comm);
            base == bin && *pid != me && !wrapper(*pid)
        })
        .map(|(pid, _)| *pid)
        .collect()
}

fn inspect(tool: Tool, pid: u32) -> RunningClaude {
    let out = Command::new("ps")
        .args(["-Eww", "-o", "command=", "-p", &pid.to_string()])
        .output();
    let text = out
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    parse_command(tool, pid, &text)
}

fn parse_command(tool: Tool, pid: u32, text: &str) -> RunningClaude {
    let var = format!("{}=", tool.home_env());
    let words: Vec<&str> = text.split_whitespace().collect();
    let env_readable = words
        .iter()
        .any(|w| w.starts_with("PATH=") || w.starts_with("HOME="));
    let config_dir = words
        .iter()
        .find_map(|w| w.strip_prefix(var.as_str()))
        .map(|d| PathBuf::from(d.trim_end_matches('/')));
    RunningClaude {
        pid,
        config_dir,
        env_readable,
    }
}

/// True if a running claude uses this profile. A process whose env cannot be
/// read counts as "in use" (unknown is treated conservatively).
pub fn profile_in_use(profile: &Profile, running: &[RunningClaude]) -> bool {
    running.iter().any(|r| {
        if !r.env_readable {
            return true;
        }
        match &r.config_dir {
            Some(d) => *d == profile.dir,
            None => profile.is_default_install,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prof(dir: &str, default: bool) -> Profile {
        let common = if default { dir } else { "/h/.claude" };
        Profile::bare("p", Tool::Claude, dir.into(), common.into())
    }

    #[test]
    fn matches_env() {
        let r = parse_command(
            Tool::Claude,
            1,
            "claude --x PATH=/bin CLAUDE_CONFIG_DIR=/h/.claude-work HOME=/h",
        );
        assert_eq!(r.config_dir, Some(PathBuf::from("/h/.claude-work")));
        assert!(profile_in_use(
            &prof("/h/.claude-work", false),
            std::slice::from_ref(&r)
        ));
        assert!(!profile_in_use(&prof("/h/.claude-alt", false), &[r]));
        let d = parse_command(Tool::Claude, 2, "claude PATH=/bin HOME=/h");
        assert!(profile_in_use(
            &prof("/h/.claude", true),
            std::slice::from_ref(&d)
        ));
        assert!(!profile_in_use(&prof("/h/.claude-alt", false), &[d]));
        let u = parse_command(Tool::Claude, 3, "claude");
        assert!(!u.env_readable);
        assert!(profile_in_use(&prof("/h/.claude-alt", false), &[u]));
        let c = parse_command(Tool::Codex, 4, "codex PATH=/bin CODEX_HOME=/h/.codex-b/");
        assert_eq!(c.config_dir, Some(PathBuf::from("/h/.codex-b")));
        let n = parse_command(
            Tool::Codex,
            5,
            "codex PATH=/bin CLAUDE_CONFIG_DIR=/h/.claude-x",
        );
        assert_eq!(n.config_dir, None);
    }

    #[test]
    fn skips_agp_wrappers() {
        let comm = parse_ps(
            "  10 agp\n  11 /Users/me/.local/bin/claude\n  12 claude\n  13 claude\n  14 zsh\n",
        );
        let ucomm = parse_ps("  10 agp\n  11 2.1.301\n  12 agp\n  13 agp\n  14 zsh\n");
        // 12 is another agent-prism session, 13 is this one.
        assert_eq!(tool_pids("claude", &comm, &ucomm, 13), vec![11]);
        // A caller that is not agent-prism skips only itself.
        let ucomm_other = parse_ps("  11 2.1.301\n  12 agp\n  13 agp\n  99 doctor\n");
        assert_eq!(
            tool_pids("claude", &comm, &ucomm_other, 99),
            vec![11, 12, 13]
        );
        // Values with spaces stay whole.
        assert_eq!(parse_ps("7 /a b/claude")[0], (7, "/a b/claude".into()));
    }
}
