//! Backup of a profile dir before adopt (DESIGN §4.4).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::paths::Paths;

/// Where backups go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackupTarget {
    Local(PathBuf),
    Ssh { host: String, path: String },
}

impl BackupTarget {
    /// Parse `ssh://host/path` or a local path (with `~`).
    pub fn parse(s: &str, paths: &Paths) -> Result<BackupTarget> {
        if let Some(rest) = s.strip_prefix("ssh://") {
            let (host, path) = rest
                .split_once('/')
                .with_context(|| format!("backup target {s:?} has no path"))?;
            if host.is_empty() || path.is_empty() {
                bail!("backup target {s:?} needs ssh://host/path");
            }
            return Ok(BackupTarget::Ssh {
                host: host.to_string(),
                path: format!("/{}", path.trim_end_matches('/')),
            });
        }
        if s.trim().is_empty() {
            bail!("empty backup target");
        }
        Ok(BackupTarget::Local(paths.expand(s)))
    }
}

/// Single-quote for a POSIX shell.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn tar_command(dir: &Path, common_history: &Path) -> Result<Command> {
    let parent = dir.parent().context("profile dir has no parent")?;
    let name = dir.file_name().context("profile dir has no name")?;
    let mut cmd = Command::new("tar");
    cmd.arg("--no-mac-metadata")
        .arg("-czf")
        .arg("-")
        .arg("-C")
        .arg(parent)
        .arg(name);
    if let (Some(hp), Some(hn)) = (common_history.parent(), common_history.file_name())
        && common_history.is_file()
    {
        cmd.arg("-C").arg(hp).arg(hn);
    }
    Ok(cmd)
}

/// Archive `dir` plus `common_history` to the target. Returns where it went.
pub fn backup_profile(
    target: &BackupTarget,
    profile_id: &str,
    dir: &Path,
    common_history: &Path,
) -> Result<String> {
    let now = jiff::Zoned::now();
    let name = format!(
        "{}-{profile_id}-{}.tar.gz",
        now.strftime("%Y-%m-%d"),
        now.strftime("%H%M%S")
    );
    let mut tar = tar_command(dir, common_history)?;
    match target {
        BackupTarget::Local(root) => {
            std::fs::create_dir_all(root)
                .with_context(|| format!("creating {}", root.display()))?;
            let file = root.join(&name);
            let out = std::fs::File::create(&file)
                .with_context(|| format!("creating {}", file.display()))?;
            let status = tar.stdout(out).status().context("running tar")?;
            if !status.success() {
                let _ = std::fs::remove_file(&file);
                bail!("tar failed ({status})");
            }
            if std::fs::metadata(&file)?.len() == 0 {
                bail!("backup {} is empty", file.display());
            }
            Ok(file.display().to_string())
        }
        BackupTarget::Ssh { host, path } => {
            let remote = format!("{path}/{name}");
            let mut child = tar.stdout(Stdio::piped()).spawn().context("running tar")?;
            let stdout = child.stdout.take().context("tar stdout")?;
            let script = format!("mkdir -p {} && cat > {}", sh_quote(path), sh_quote(&remote));
            let ssh = Command::new("ssh")
                .args(["-o", "BatchMode=yes", host, &script])
                .stdin(stdout)
                .status()
                .context("running ssh")?;
            let tar_status = child.wait().context("waiting for tar")?;
            if !tar_status.success() {
                bail!("tar failed ({tar_status})");
            }
            if !ssh.success() {
                bail!("ssh upload to {host} failed ({ssh})");
            }
            let out = Command::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    host,
                    &format!("wc -c < {}", sh_quote(&remote)),
                ])
                .output()
                .context("running ssh")?;
            let size: u64 = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
            if !out.status.success() || size == 0 {
                bail!("remote backup {host}:{remote} is missing or empty");
            }
            Ok(format!("{host}:{remote}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_targets() {
        let p = Paths::under(Path::new("/r"));
        assert_eq!(
            BackupTarget::parse("ssh://backup-host/backups/x/", &p).unwrap(),
            BackupTarget::Ssh {
                host: "backup-host".into(),
                path: "/backups/x".into()
            }
        );
        assert_eq!(
            BackupTarget::parse("~/b", &p).unwrap(),
            BackupTarget::Local("/r/home/b".into())
        );
        assert!(BackupTarget::parse("ssh://host", &p).is_err());
    }

    #[test]
    fn local_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".claude-x");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), "1").unwrap();
        let hist = tmp.path().join("history.jsonl");
        std::fs::write(&hist, "{}\n").unwrap();
        let t = BackupTarget::Local(tmp.path().join("out"));
        let loc = backup_profile(&t, "x", &dir, &hist).unwrap();
        assert!(loc.ends_with(".tar.gz") && loc.contains("-x-"));
        let list = Command::new("tar").args(["-tzf", &loc]).output().unwrap();
        let list = String::from_utf8_lossy(&list.stdout);
        assert!(
            list.contains(".claude-x/a") && list.contains("history.jsonl"),
            "{list}"
        );
    }
}
