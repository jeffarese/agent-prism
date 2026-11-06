//! Linux and WSL end to end: there is no Keychain, and Claude Code keeps each
//! account's login in `<config dir>/.credentials.json`. agp reads the logins
//! there (no `AGP_SECRET_STORE_FILE`), picks only logged-in accounts, and
//! `agp doctor` says where each login lives. Every path points into a temp
//! dir; nothing talks to Anthropic.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const AGP: &str = env!("CARGO_BIN_EXE_agp");

struct Setup {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn write_mode(path: &Path, text: &str, mode: u32) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
}

impl Setup {
    /// Accounts `a` (logged in, 10% used), `b` (logged in, 60% used) and
    /// `c` (never logged in), with a fake `claude`.
    fn new() -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let common = home.join(".claude");
        std::fs::create_dir_all(common.join("projects")).unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        std::fs::create_dir_all(root.join("state/usage")).unwrap();
        let fake = root.join("fake-claude.sh");
        write_mode(
            &fake,
            "#!/bin/sh\necho \"ran on ${CLAUDE_CONFIG_DIR##*/.claude-}\"\n",
            0o755,
        );
        let t = now();
        let exp = (t + 7200) * 1000;
        let mut profiles = String::new();
        for (id, used) in [("a", Some(10.0)), ("b", Some(60.0)), ("c", None)] {
            let dir = home.join(format!(".claude-{id}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(common.join("projects"), dir.join("projects")).unwrap();
            std::fs::write(dir.join(".claude.json"), "{}").unwrap();
            profiles.push_str(&format!(
                "[[profile]]\nid = \"{id}\"\nlabel = \"Acct-{id}\"\ndir = \"{}\"\n\n",
                dir.display()
            ));
            let Some(u5) = used else { continue };
            // What Claude Code writes on Linux after /login (mode 0600).
            write_mode(
                &dir.join(".credentials.json"),
                &format!(
                    r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat-SECRET","refreshToken":"sk-ant-ort-SECRET","expiresAt":{exp},"scopes":["user:inference"],"subscriptionType":"max","rateLimitTier":"default_claude_max_5x"}}}}"#
                ),
                0o600,
            );
            let usage = serde_json::json!({
                "snapshot": {
                    "fetched_at": t,
                    "five_hour": {"utilization": u5, "resets_at": t + 4 * 3600, "inferred": false},
                    "seven_day": {"utilization": 20.0, "resets_at": t + 3 * 86_400, "inferred": false},
                },
                "last_attempt": t,
            });
            std::fs::write(
                root.join(format!("state/usage/{id}.json")),
                usage.to_string(),
            )
            .unwrap();
        }
        let config = format!(
            "common_home = \"{}\"\n\n[claude]\nbin = \"{}\"\n\n{profiles}",
            common.display(),
            fake.display()
        );
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join("config/config.toml"), config).unwrap();
        Setup { _tmp: tmp, root }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(AGP)
            .args(args)
            .current_dir(self.root.join("work"))
            .env("HOME", self.root.join("home"))
            .env("AGP_CONFIG_DIR", self.root.join("config"))
            .env("AGP_STATE_DIR", self.root.join("state"))
            .env("AGP_NO_POLL", "1")
            .env_remove("AGP_SECRET_STORE_FILE")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("AGP_SOCK")
            .env_remove("AGP")
            .env_remove("AGP_PROFILE")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

#[test]
fn logins_come_from_credentials_files() {
    let s = Setup::new();
    let v = s.json(&["status", "--json"]);
    let token = |id: &str| {
        v["views"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["profile_id"] == id)
            .unwrap_or_else(|| panic!("no view for {id}: {v:#}"))["token"]
            .clone()
    };
    assert!(token("a").get("Valid").is_some(), "{v:#}");
    assert!(token("b").get("Valid").is_some(), "{v:#}");
    assert_eq!(token("c"), "Missing", "{v:#}");
    // The least used logged-in account goes first.
    assert_eq!(v["pick"]["chosen"], "a", "{v:#}");
    assert!(!v.to_string().contains("SECRET"));
}

#[test]
fn print_mode_runs_on_the_best_account() {
    let s = Setup::new();
    let out = s.run(&["-p", "hello"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ran on a");
}

#[test]
fn doctor_names_the_platform_and_the_login_files() {
    let s = Setup::new();
    let out = s.run(&["doctor"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("platform   Linux") || text.contains("platform   WSL"),
        "{text}"
    );
    let file = s.root.join("home/.claude-a/.credentials.json");
    assert!(
        text.contains(&format!("login    {}", file.display())),
        "{text}"
    );
    assert!(!text.contains("keychain"), "{text}");
    assert!(!text.contains("SECRET"), "{text}");
}
