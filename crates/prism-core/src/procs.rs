//! Detect running `claude` (or `codex`) processes and their config dir
//! (DESIGN §4.5): with `ps` on macOS, from `/proc` on Linux.

use std::path::PathBuf;
#[cfg(not(target_os = "linux"))]
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
#[cfg(not(target_os = "linux"))]
pub fn running(tool: Tool) -> Vec<RunningClaude> {
    let (Some(comm), Some(ucomm)) = (ps_column("comm="), ps_column("ucomm=")) else {
        return Vec::new();
    };
    tool_pids(tool.bin(), &comm, &ucomm, std::process::id())
        .into_iter()
        .map(|pid| inspect(tool, pid))
        .collect()
}

/// Best-effort list of the current user's processes of `tool`, from
/// `/proc`: `argv[0]` from `cmdline`, the executable from `exe`, the
/// environment it started with from `environ`.
#[cfg(target_os = "linux")]
pub fn running(tool: Tool) -> Vec<RunningClaude> {
    let procs = linux::list();
    let argv0: Vec<(u32, String)> = procs.iter().map(|p| (p.pid, p.argv0.clone())).collect();
    let exe: Vec<(u32, String)> = procs
        .iter()
        .filter_map(|p| Some((p.pid, p.exe.clone()?)))
        .collect();
    tool_pids(tool.bin(), &argv0, &exe, std::process::id())
        .into_iter()
        .map(|pid| match std::fs::read(format!("/proc/{pid}/environ")) {
            Ok(env) => parse_environ(tool, pid, &env),
            Err(_) => RunningClaude {
                pid,
                config_dir: None,
                env_readable: false,
            },
        })
        .collect()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::os::unix::fs::MetadataExt;

    pub(super) struct Proc {
        pub pid: u32,
        pub argv0: String,
        /// The executable's path; `None` when it can't be read.
        pub exe: Option<String>,
    }

    /// The current user's processes.
    pub(super) fn list() -> Vec<Proc> {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let Ok(rd) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        rd.flatten()
            .filter_map(|e| {
                let pid: u32 = e.file_name().to_str()?.parse().ok()?;
                if e.metadata().ok()?.uid() != uid {
                    return None;
                }
                let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
                Some(Proc {
                    pid,
                    argv0: super::argv0(&cmdline)?,
                    exe: std::fs::read_link(e.path().join("exe"))
                        .ok()
                        .map(|p| super::exe_path(&p.to_string_lossy())),
                })
            })
            .collect()
    }
}

/// The executable of `/proc/<pid>/exe`. One replaced on disk since it
/// started (an agp upgrade) reads `<path> (deleted)`: still the same agp.
#[cfg(any(target_os = "linux", test))]
fn exe_path(link: &str) -> String {
    link.strip_suffix(" (deleted)").unwrap_or(link).to_string()
}

/// `argv[0]` from the NUL-separated text of `/proc/<pid>/cmdline`; `None`
/// for a kernel thread (empty).
#[cfg(any(target_os = "linux", test))]
fn argv0(cmdline: &[u8]) -> Option<String> {
    let first = cmdline.split(|b| *b == 0).next()?;
    (!first.is_empty()).then(|| String::from_utf8_lossy(first).into_owned())
}

/// The config dir from the NUL-separated text of `/proc/<pid>/environ`.
#[cfg(any(target_os = "linux", test))]
fn parse_environ(tool: Tool, pid: u32, environ: &[u8]) -> RunningClaude {
    let var = format!("{}=", tool.home_env());
    let vars: Vec<String> = environ
        .split(|b| *b == 0)
        .filter(|v| !v.is_empty())
        .map(|v| String::from_utf8_lossy(v).into_owned())
        .collect();
    let env_readable = vars
        .iter()
        .any(|v| v.starts_with("PATH=") || v.starts_with("HOME="));
    let config_dir = vars
        .iter()
        .find_map(|v| v.strip_prefix(var.as_str()))
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d.trim_end_matches('/')));
    RunningClaude {
        pid,
        config_dir,
        env_readable,
    }
}

/// `(pid, value)` for every process of the user from `ps -xo pid=,<col>`.
/// The value is the rest of the line, so it may contain spaces.
#[cfg(not(target_os = "linux"))]
fn ps_column(col: &str) -> Option<Vec<(u32, String)>> {
    let out = Command::new("ps")
        .args(["-xo", &format!("pid=,{col}")])
        .output()
        .ok()?;
    Some(parse_ps(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(any(not(target_os = "linux"), test))]
fn parse_ps(text: &str) -> Vec<(u32, String)> {
    text.lines()
        .filter_map(|l| {
            let (pid, val) = l.trim_start().split_once(char::is_whitespace)?;
            Some((pid.parse().ok()?, val.trim().to_string()))
        })
        .collect()
}

/// Pids whose argv[0] (`comm`) has the base name `bin`, except `me` and
/// every process that runs the same executable (`ucomm`; on Linux the path
/// of `/proc/<pid>/exe`) as `me`: agent-prism runs as `argv[0] = claude` (or
/// `codex`) so terminals show it as that tool.
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

#[cfg(not(target_os = "linux"))]
fn inspect(tool: Tool, pid: u32) -> RunningClaude {
    let out = Command::new("ps")
        .args(["-Eww", "-o", "command=", "-p", &pid.to_string()])
        .output();
    let text = out
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    parse_command(tool, pid, &text)
}

#[cfg(any(not(target_os = "linux"), test))]
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
    fn proc_files() {
        assert_eq!(argv0(b"claude\0--resume\0x\0").as_deref(), Some("claude"));
        assert_eq!(argv0(b"/a b/claude\0").as_deref(), Some("/a b/claude"));
        assert_eq!(argv0(b""), None);
        assert_eq!(exe_path("/h/.local/bin/agp (deleted)"), "/h/.local/bin/agp");
        assert_eq!(exe_path("/h/.local/bin/agp"), "/h/.local/bin/agp");
        let env = b"HOME=/h\0CLAUDE_CONFIG_DIR=/h/.claude work/\0X=1\0";
        let r = parse_environ(Tool::Claude, 7, env);
        assert!(r.env_readable);
        assert_eq!(r.config_dir, Some(PathBuf::from("/h/.claude work")));
        let r = parse_environ(Tool::Codex, 7, b"PATH=/bin\0CODEX_HOME=\0");
        assert_eq!((r.env_readable, r.config_dir), (true, None));
        let r = parse_environ(Tool::Claude, 7, b"");
        assert!(!r.env_readable);
    }

    /// A real process named `claude` with its own config dir is found,
    /// with the dir it started with.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn finds_a_live_process() {
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude-probe");
        let mut child = std::process::Command::new("sleep")
            .arg0("claude")
            .arg("30")
            .env("CLAUDE_CONFIG_DIR", &cfg)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let found = loop {
            let found = running(Tool::Claude)
                .into_iter()
                .find(|r| r.pid == child.id());
            if found.is_some() || std::time::Instant::now() > deadline {
                break found;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let _ = child.kill();
        let _ = child.wait();
        let r = found.expect("the child is listed");
        // macOS hides the environment of system binaries such as `sleep`
        // (SIP); `parse_command` covers what `ps -E` prints there.
        if cfg!(target_os = "linux") {
            assert!(r.env_readable);
            assert_eq!(r.config_dir, Some(cfg));
        }
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
