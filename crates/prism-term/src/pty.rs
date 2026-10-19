//! Child process in a PTY. See docs/DESIGN.md §7.4.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How to start the child. The runner adds nothing to it.
#[derive(Clone, Debug, PartialEq)]
pub struct ChildSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Variables to set (after `env_remove`).
    pub env_set: Vec<(OsString, OsString)>,
    pub env_remove: Vec<OsString>,
    pub cwd: PathBuf,
}

/// A status change of the child reported by the waiter thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildStatus {
    /// The child exited; the code is `128 + signal` when it was killed.
    Exited(i32),
    /// The child was stopped (e.g. Ctrl+Z).
    Stopped,
    /// The child was continued.
    Continued,
}

/// A child process running in a new session with a PTY as its controlling
/// terminal.
#[derive(Debug)]
pub struct PtyChild {
    master: File,
    pid: libc::pid_t,
    exited: Arc<AtomicBool>,
}

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn set_cloexec(fd: i32) {
    // SAFETY: plain fcntl on an fd we own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

impl PtyChild {
    /// Open a PTY of `rows × cols`, start `spec` on it and start a waiter
    /// thread that calls `on_status` for every status change (the last call
    /// is [`ChildStatus::Exited`]).
    pub fn spawn(
        spec: &ChildSpec,
        rows: u16,
        cols: u16,
        on_status: impl Fn(ChildStatus) + Send + 'static,
    ) -> io::Result<Self> {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let mut ws = winsize(rows, cols);
        // SAFETY: out-pointers are valid; name and termios may be null.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut ws,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty returned two fresh fds that we now own.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        set_cloexec(master.as_raw_fd());
        set_cloexec(slave.as_raw_fd());

        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args);
        for k in &spec.env_remove {
            cmd.env_remove(k);
        }
        for (k, v) in &spec.env_set {
            cmd.env(k, v);
        }
        cmd.current_dir(&spec.cwd);
        cmd.stdin(Stdio::from(slave.try_clone()?));
        cmd.stdout(Stdio::from(slave.try_clone()?));
        cmd.stderr(Stdio::from(slave.try_clone()?));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Restore default dispositions that the parent may have
                // changed (signal-hook handlers are reset by exec anyway).
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop(slave);
        let pid = child.id() as libc::pid_t;
        // We reap with waitpid ourselves; never call Child::wait.
        drop(child);

        let exited = Arc::new(AtomicBool::new(false));
        let ex = exited.clone();
        std::thread::Builder::new()
            .name("agp-child-wait".into())
            .spawn(move || {
                loop {
                    let mut status: libc::c_int = 0;
                    // SAFETY: waitpid on our own child.
                    let r = unsafe {
                        libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WCONTINUED)
                    };
                    if r < 0 {
                        if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        ex.store(true, Ordering::SeqCst);
                        on_status(ChildStatus::Exited(1));
                        return;
                    }
                    if libc::WIFEXITED(status) {
                        ex.store(true, Ordering::SeqCst);
                        on_status(ChildStatus::Exited(libc::WEXITSTATUS(status)));
                        return;
                    }
                    if libc::WIFSIGNALED(status) {
                        ex.store(true, Ordering::SeqCst);
                        on_status(ChildStatus::Exited(128 + libc::WTERMSIG(status)));
                        return;
                    }
                    if libc::WIFSTOPPED(status) {
                        on_status(ChildStatus::Stopped);
                    } else if libc::WIFCONTINUED(status) {
                        on_status(ChildStatus::Continued);
                    }
                }
            })?;

        Ok(Self {
            master: File::from(master),
            pid,
            exited,
        })
    }

    /// A second handle on the master side for a reader thread.
    pub fn reader(&self) -> io::Result<File> {
        self.master.try_clone()
    }

    /// Write bytes to the child's input.
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.master.write_all(bytes)
    }

    /// Set the PTY size (the child gets SIGWINCH).
    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        let ws = winsize(rows, cols);
        // SAFETY: TIOCSWINSZ with a valid winsize pointer.
        let r = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The child's pid (also its process group and session id).
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// True once the waiter thread saw the child exit.
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// Send `sig` to the child's process group.
    pub fn signal_group(&self, sig: i32) -> io::Result<()> {
        if self.has_exited() {
            return Ok(());
        }
        // SAFETY: kill on the child's process group.
        if unsafe { libc::kill(-self.pid, sig) } < 0 {
            // Fall back to the child alone (group may be gone).
            // SAFETY: as above.
            if unsafe { libc::kill(self.pid, sig) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// SIGTERM the group, wait up to `grace` for the exit, then SIGKILL.
    /// Returns true if the child exited within the grace period.
    pub fn terminate(&self, grace: Duration) -> bool {
        let _ = self.signal_group(libc::SIGHUP);
        let _ = self.signal_group(libc::SIGTERM);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if self.has_exited() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.signal_group(libc::SIGKILL);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::mpsc;

    fn spec(script: &str) -> ChildSpec {
        ChildSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env_set: vec![("AGP_T".into(), "yes".into())],
            env_remove: vec!["HOME".into()],
            cwd: "/".into(),
        }
    }

    fn read_all(mut f: File) -> String {
        let mut s = Vec::new();
        let mut b = [0u8; 4096];
        loop {
            match f.read(&mut b) {
                Ok(0) | Err(_) => break,
                Ok(n) => s.extend_from_slice(&b[..n]),
            }
        }
        String::from_utf8_lossy(&s).into_owned()
    }

    #[test]
    fn runs_with_size_env_and_exit_code() {
        let (tx, rx) = mpsc::channel();
        let c = PtyChild::spawn(
            &spec("stty size; echo $AGP_T ${HOME:-nohome}; pwd; sleep 0.2; exit 7"),
            12,
            40,
            move |s| {
                let _ = tx.send(s);
            },
        )
        .unwrap();
        let r = c.reader().unwrap();
        let t = std::thread::spawn(move || read_all(r));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            ChildStatus::Exited(7)
        );
        drop(c);
        let out = t.join().unwrap();
        assert!(out.contains("12 40"), "{out}");
        assert!(out.contains("yes nohome"), "{out}");
    }

    #[test]
    fn terminate_and_resize() {
        let (tx, rx) = mpsc::channel();
        let c = PtyChild::spawn(&spec("sleep 30"), 10, 10, move |s| {
            let _ = tx.send(s);
        })
        .unwrap();
        c.resize(20, 30).unwrap();
        assert!(c.terminate(Duration::from_secs(3)));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            ChildStatus::Exited(_)
        ));
    }
}
