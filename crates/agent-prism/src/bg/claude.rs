//! Running `claude` for background sessions in a profile's env, and
//! spawning the detached watcher.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use prism_core::profile::Profile;

use super::job::{self, Agent, Launched};
use crate::launch;

/// How long to read a pipe after the process exited. `claude --bg` may
/// start the account's supervisor, and a supervisor that kept the pipe
/// must not hold agent-prism.
const DRAIN: Duration = Duration::from_secs(1);

/// The `claude` binary.
#[derive(Clone, Debug)]
pub struct Claude {
    pub bin: PathBuf,
}

/// What a finished command printed.
#[derive(Debug, Default)]
pub struct Captured {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Claude {
    /// `claude <args>` in `profile`'s env (no agent-prism variables, no
    /// config or profile args: subcommands and resumes take none).
    pub fn command(&self, profile: &Profile, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args);
        let (set, remove) = launch::child_env(profile, Path::new(""));
        for k in remove {
            cmd.env_remove(k);
        }
        cmd.envs(set);
        cmd.stdin(Stdio::null());
        cmd
    }

    /// `claude agents --json --all` in `profile`.
    pub fn agents(&self, profile: &Profile) -> Result<Vec<Agent>> {
        let args = ["agents", "--json", "--all"].map(String::from);
        let out = run_capture(self.command(profile, &args), Duration::from_secs(30))?;
        anyhow::ensure!(
            out.code == 0,
            "claude agents in {} exited {}: {}",
            profile.id,
            out.code,
            out.stderr.trim()
        );
        Ok(job::parse_agents(&out.stdout))
    }

    /// The live pid of the session in `profile`, if `claude agents` lists
    /// one (and the process exists).
    pub fn live_pid(&self, profile: &Profile, sid: &str, short: &str) -> Result<Option<u32>> {
        Ok(self
            .agents(profile)?
            .into_iter()
            .find(|a| a.is(sid, short))
            .and_then(|a| a.pid)
            .filter(|p| pid_alive(*p)))
    }

    /// `claude stop <short>` in `profile`.
    pub fn stop(&self, profile: &Profile, short: &str) -> Result<Captured> {
        let args = ["stop".to_string(), short.to_string()];
        run_capture(self.command(profile, &args), Duration::from_secs(30))
    }

    /// A `claude --bg …` line in `profile` with `cwd`, and what it printed.
    pub fn launch(
        &self,
        profile: &Profile,
        cwd: &Path,
        args: &[String],
    ) -> Result<(Captured, Option<Launched>)> {
        let mut cmd = self.command(profile, args);
        cmd.current_dir(cwd);
        let out = run_capture(cmd, Duration::from_secs(60))?;
        let launched = job::parse_launch_output(&out.stdout);
        Ok((out, launched))
    }

    /// Waits until `claude agents` in `profile` shows no live pid for the
    /// session. Between the checks it watches the known pid itself, so
    /// `claude agents` runs only a few times.
    pub fn wait_stopped(
        &self,
        profile: &Profile,
        sid: &str,
        short: &str,
        deadline: Instant,
    ) -> bool {
        loop {
            let pid = match self.live_pid(profile, sid, short) {
                Ok(None) => return true,
                Ok(Some(p)) => Some(p),
                Err(e) => {
                    log::warn!("bg {short}: {e:#}");
                    None
                }
            };
            if Instant::now() >= deadline {
                return false;
            }
            while pid.is_some_and(pid_alive) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(200));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// True unless the process certainly does not exist.
pub fn pid_alive(pid: u32) -> bool {
    let Ok(p) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if p <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(p, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Runs `cmd` with stdout and stderr captured; kills it after `timeout`.
pub fn run_capture(mut cmd: Command, timeout: Duration) -> Result<Captured> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", cmd.get_program().to_string_lossy()))?;
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let code = wait_timeout(&mut child, timeout)?;
    Ok(Captured {
        code,
        stdout: collect(out),
        stderr: collect(err),
    })
}

fn drain<R: Read + Send + 'static>(r: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut r) = r {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    }
    rx
}

fn collect(rx: mpsc::Receiver<Vec<u8>>) -> String {
    rx.recv_timeout(DRAIN)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// The exit code (128 + signal), killing the child after `timeout`.
fn wait_timeout(child: &mut Child, timeout: Duration) -> Result<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok(exit_code(st));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let st = child.wait()?;
            log::warn!("{:?} timed out", child.id());
            return Ok(exit_code(st));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub fn exit_code(st: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0))
}

/// Starts `cmd` fully detached and returns its pid: a new session (so not
/// in the caller's process group, which a timeout may kill), stdin, stdout
/// and stderr on /dev/null, and no other inherited descriptor. The caller
/// does not wait for it.
pub fn spawn_detached(mut cmd: Command) -> Result<u32> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid, getdtablesize and fcntl are async-signal-safe and
    // touch no memory of the parent.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // A descriptor the caller passed without close-on-exec (a pipe
            // of its own) would otherwise stay open in the watcher.
            let max = libc::getdtablesize().clamp(0, 65_536);
            for fd in 3..max {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("starting {}", cmd.get_program().to_string_lossy()))?;
    Ok(child.id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_and_timeout() {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "echo out; echo err >&2; exit 3"]);
        let out = run_capture(c, Duration::from_secs(5)).unwrap();
        assert_eq!(
            (out.code, out.stdout.as_str(), out.stderr.as_str()),
            (3, "out\n", "err\n")
        );
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "sleep 5"]);
        let t = Instant::now();
        let out = run_capture(c, Duration::from_millis(200)).unwrap();
        assert!(t.elapsed() < Duration::from_secs(3));
        assert_eq!(out.code, 128 + libc::SIGKILL);
    }

    /// A child that leaves a grandchild holding the pipe does not hold the
    /// capture past its own exit.
    #[test]
    fn capture_ignores_a_pipe_kept_open() {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "echo hi; (sleep 3 &) ; exit 0"]);
        let t = Instant::now();
        let out = run_capture(c, Duration::from_secs(5)).unwrap();
        assert_eq!(out.code, 0);
        assert!(
            t.elapsed() < Duration::from_millis(2500),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn detached_child_has_its_own_session_and_no_inherited_fds() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("report");
        // A descriptor left open without close-on-exec, at a number nothing
        // else uses (other tests spawn children meanwhile; they may inherit
        // it, but they don't matter here).
        const HIGH: i32 = 250;
        let (_rd, wr) = std::io::pipe().unwrap();
        {
            use std::os::fd::AsRawFd;
            // SAFETY: dup2 onto a number this test owns; dup2 clears
            // close-on-exec on the copy.
            assert_eq!(unsafe { libc::dup2(wr.as_raw_fd(), HIGH) }, HIGH);
        }
        let script = format!(
            "s=; for n in 0 1 2; do [ -c /dev/fd/$n ] && s=\"$s c$n\"; done; \
             {{ ps -o pgid= -p $$; echo \"$s\"; ls /dev/fd; }} > {}.tmp 2>&1; \
             mv {0}.tmp {0}; sleep 2",
            report.display()
        );
        let mut c = Command::new("/bin/sh");
        c.args(["-c", &script]);
        let pid = spawn_detached(c).unwrap();
        // SAFETY: closing the copy made above.
        unsafe { libc::close(HIGH) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !report.exists() {
            assert!(Instant::now() < deadline, "no report");
            std::thread::sleep(Duration::from_millis(20));
        }
        let text = std::fs::read_to_string(&report).unwrap();
        let mut lines = text.lines();
        let pgid = lines.next().unwrap().trim();
        // Its own process group (setsid made it the leader).
        assert_eq!(pgid, pid.to_string());
        // SAFETY: getpgrp has no preconditions.
        assert_ne!(pgid, unsafe { libc::getpgrp() }.to_string());
        // stdin, stdout and stderr are /dev/null (character devices, not
        // the caller's pipes).
        assert_eq!(lines.next().unwrap().trim(), "c0 c1 c2");
        assert!(!lines.any(|l| l.trim() == HIGH.to_string()), "{text}");
        // SAFETY: our own child.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
}
