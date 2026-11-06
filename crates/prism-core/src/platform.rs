//! What differs between macOS, Linux and WSL (DESIGN §1.1).
//!
//! agent-prism runs on macOS and Linux, WSL 2 included. The differences are
//! few and all live here or behind a `cfg`: where Claude keeps its login
//! (the Keychain on macOS, `<dir>/.credentials.json` elsewhere), how running
//! processes are read (`ps` on macOS, `/proc` on Linux), and which program
//! opens a URL.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The system agent-prism runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    /// Linux under the Windows Subsystem for Linux.
    Wsl,
    /// Another Unix: everything works except what needs `/proc` or the
    /// Keychain.
    Other,
}

impl Os {
    /// `macOS`, `Linux`, `WSL` or the target's name.
    pub fn label(self) -> &'static str {
        match self {
            Os::MacOs => "macOS",
            Os::Linux => "Linux",
            Os::Wsl => "WSL",
            Os::Other => std::env::consts::OS,
        }
    }
}

/// The system this binary runs on, read once.
pub fn os() -> Os {
    static OS: OnceLock<Os> = OnceLock::new();
    *OS.get_or_init(|| {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "linux") {
            let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
            if is_wsl(&release, crate::env::var_os("WSL_DISTRO_NAME").is_some()) {
                Os::Wsl
            } else {
                Os::Linux
            }
        } else {
            Os::Other
        }
    })
}

/// True for a WSL kernel (`5.15.153.1-microsoft-standard-WSL2`) or when WSL
/// set `WSL_DISTRO_NAME`.
fn is_wsl(kernel_release: &str, distro_env: bool) -> bool {
    distro_env || kernel_release.to_ascii_lowercase().contains("microsoft")
}

/// True when Claude keeps its logins in the macOS Keychain; elsewhere they
/// are in `<config dir>/.credentials.json`.
pub fn has_keychain() -> bool {
    os() == Os::MacOs
}

/// The first executable `name` in `PATH`.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_in(&path, name)
}

fn find_in(path: &std::ffi::OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `sqlite3` for reading Codex's thread database: the system one on macOS,
/// else the first on `PATH` (Linux distributions don't always ship it).
pub fn sqlite3() -> Option<PathBuf> {
    let system = Path::new("/usr/bin/sqlite3");
    if is_executable(system) {
        return Some(system.to_path_buf());
    }
    find_on_path("sqlite3")
}

/// A program and its leading args that open a URL in the user's browser, or
/// `None` where there is no browser to open (a Linux box without a desktop,
/// reached over SSH).
pub fn url_opener() -> Option<(PathBuf, Vec<OsString>)> {
    let has_display =
        crate::env::var_os("DISPLAY").is_some() || crate::env::var_os("WAYLAND_DISPLAY").is_some();
    pick_opener(os(), has_display, &|name| find_on_path(name))
}

fn pick_opener(
    os: Os,
    has_display: bool,
    find: &dyn Fn(&str) -> Option<PathBuf>,
) -> Option<(PathBuf, Vec<OsString>)> {
    match os {
        Os::MacOs => Some(("/usr/bin/open".into(), Vec::new())),
        // The Windows browser: `wslview` (wslu) when it is there, else
        // Explorer, which hands a URL to the default browser. Not
        // `xdg-open`: without a Linux desktop it may start a text browser
        // in this terminal.
        Os::Wsl => find("wslview")
            .or_else(|| find("explorer.exe"))
            .or_else(|| {
                let p = PathBuf::from("/mnt/c/Windows/explorer.exe");
                p.is_file().then_some(p)
            })
            .map(|p| (p, Vec::new())),
        Os::Linux | Os::Other if has_display => find("xdg-open")
            .map(|p| (p, Vec::new()))
            .or_else(|| find("gio").map(|p| (p, vec!["open".into()]))),
        Os::Linux | Os::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_from_kernel_or_env() {
        assert!(is_wsl("5.15.153.1-microsoft-standard-WSL2", false));
        assert!(is_wsl("4.4.0-19041-Microsoft", false));
        assert!(is_wsl("6.8.0-45-generic", true));
        assert!(!is_wsl("6.8.0-45-generic", false));
        assert!(!is_wsl("", false));
    }

    #[test]
    fn this_os_is_known() {
        let os = os();
        if cfg!(target_os = "macos") {
            assert_eq!(os, Os::MacOs);
            assert!(has_keychain());
        } else if cfg!(target_os = "linux") {
            assert!(matches!(os, Os::Linux | Os::Wsl), "{os:?}");
            assert!(!has_keychain());
        }
        assert!(!os.label().is_empty());
        // CI says which system each job runs on (`macOS`, `Linux`, `WSL`).
        if let Some(want) = crate::env::var("AGP_TEST_EXPECT_OS") {
            assert_eq!(os.label(), want);
        }
    }

    #[test]
    fn openers() {
        let none = |_: &str| None;
        let all = |n: &str| Some(PathBuf::from(format!("/bin/{n}")));
        let only = |want: &'static str| {
            move |n: &str| (n == want).then(|| PathBuf::from(format!("/bin/{n}")))
        };
        let prog =
            |o: Option<(PathBuf, Vec<OsString>)>| o.map(|(p, a)| (p.display().to_string(), a));

        assert_eq!(
            prog(pick_opener(Os::MacOs, false, &none)),
            Some(("/usr/bin/open".into(), vec![]))
        );
        assert_eq!(
            prog(pick_opener(Os::Wsl, false, &all)),
            Some(("/bin/wslview".into(), vec![]))
        );
        assert_eq!(
            prog(pick_opener(Os::Wsl, true, &only("explorer.exe"))),
            Some(("/bin/explorer.exe".into(), vec![]))
        );
        assert_eq!(
            prog(pick_opener(Os::Linux, true, &all)),
            Some(("/bin/xdg-open".into(), vec![]))
        );
        assert_eq!(
            prog(pick_opener(Os::Linux, true, &only("gio"))),
            Some(("/bin/gio".into(), vec!["open".into()]))
        );
        // No desktop (SSH, a server): print the URL instead.
        assert_eq!(prog(pick_opener(Os::Linux, false, &all)), None);
        assert_eq!(prog(pick_opener(Os::Linux, true, &none)), None);
    }

    #[test]
    fn path_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("tool");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths(["relative/dir".into(), dir.path().to_path_buf()]).unwrap();
        assert_eq!(find_in(&path, "tool"), Some(exe));
        assert_eq!(find_in(&path, "plain"), None);
        assert_eq!(find_in(&path, "missing"), None);
    }
}
