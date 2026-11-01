//! Parallel lanes end to end with a fake `claude`: runs started together
//! spread over the accounts (DESIGN §6.6), pinned runs count, `agp pick
//! --n` plans, `agp status --json` reports runs and capacity, print runs
//! get a session id, and a resume goes back to its account. Every path
//! points into a temp dir; nothing talks to Anthropic.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const AGP: &str = env!("CARGO_BIN_EXE_agp");

/// Logs "<pid> <account id> <lane> <session id>", then works a while.
const FAKE: &str = r#"#!/bin/bash
lane=""; sid=""; prev=""
for a in "$@"; do
  case "$a" in lane-*) lane="$a";; esac
  [ "$prev" = "--session-id" ] && sid="$a"
  prev="$a"
done
echo "$$ ${CLAUDE_CONFIG_DIR##*/.claude-} $lane $sid" >> "$FAKE_LOG"
sleep "${FAKE_SLEEP:-2}"
"#;

/// Accounts a, b and c at 10%, 30% and 40% of their 5h window (20% weekly).
struct Setup {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    log: PathBuf,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

impl Setup {
    fn new() -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let common = home.join(".claude");
        std::fs::create_dir_all(common.join("projects")).unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        std::fs::create_dir_all(root.join("state/usage")).unwrap();
        let fake = root.join("fake-claude.sh");
        std::fs::write(&fake, FAKE).unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let t = now();
        let exp = (t + 7200) * 1000;
        let mut store = serde_json::Map::new();
        let mut profiles = String::new();
        for (id, u5) in [("a", 10.0), ("b", 30.0), ("c", 40.0)] {
            let dir = home.join(format!(".claude-{id}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(common.join("projects"), dir.join("projects")).unwrap();
            std::fs::write(dir.join(".claude.json"), "{}").unwrap();
            profiles.push_str(&format!(
                "[[profile]]\nid = \"{id}\"\nlabel = \"Acct-{id}\"\ndir = \"{}\"\n\n",
                dir.display()
            ));
            store.insert(
                prism_core::profile::keychain_service_for(&dir, false),
                Value::String(format!(
                    r#"{{"claudeAiOauth":{{"accessToken":"t","expiresAt":{exp},"subscriptionType":"team"}}}}"#
                )),
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
        std::fs::write(root.join("store.json"), Value::Object(store).to_string()).unwrap();
        Setup {
            log: root.join("fake.log"),
            _tmp: tmp,
            root,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(AGP);
        c.args(args)
            .current_dir(self.root.join("work"))
            .env("HOME", self.root.join("home"))
            .env("AGP_CONFIG_DIR", self.root.join("config"))
            .env("AGP_STATE_DIR", self.root.join("state"))
            .env("AGP_SECRET_STORE_FILE", self.root.join("store.json"))
            .env("AGP_NO_POLL", "1")
            .env("FAKE_LOG", &self.log)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("AGP_SOCK")
            .env_remove("AGP")
            .env_remove("AGP_PROFILE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        c
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.cmd(args).spawn().unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.cmd(args).stdout(Stdio::piped()).output().unwrap();
        assert!(
            out.status.success() || args[0] == "pick",
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// The fake's log: (account, lane, session id) per run.
    fn runs(&self) -> Vec<(String, String, String)> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let f: Vec<&str> = l.split(' ').collect();
                let get = |i: usize| f.get(i).copied().unwrap_or("").to_string();
                (get(1), get(2), get(3))
            })
            .collect()
    }

    /// Starts `n` lanes `agp <extra> -p lane-<i>` at once and waits for them.
    fn lanes(&self, n: usize, extra: &[&str]) -> HashMap<String, usize> {
        let _ = std::fs::remove_file(&self.log);
        let lanes: Vec<String> = (1..=n).map(|i| format!("lane-{i}")).collect();
        let children: Vec<Child> = lanes
            .iter()
            .map(|lane| {
                let mut args = extra.to_vec();
                args.extend(["-p", lane.as_str()]);
                self.spawn(&args)
            })
            .collect();
        for mut c in children {
            assert!(c.wait().unwrap().success());
        }
        let mut per: HashMap<String, usize> = HashMap::new();
        for (acct, _, _) in self.runs() {
            *per.entry(acct).or_default() += 1;
        }
        assert_eq!(per.values().sum::<usize>(), n, "{:?}", self.runs());
        per
    }

    /// Claims in the run registry (not the atomic writer's temp files).
    fn claims(&self) -> usize {
        std::fs::read_dir(self.root.join("state/sessions"))
            .map(|d| {
                d.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .count()
            })
            .unwrap_or(0)
    }
}

fn wait_for(what: &str, f: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn lanes_started_together_spread() {
    let s = Setup::new();
    // The plan of three new runs: each counts the ones before it.
    let plan = s.json(&["pick", "--n", "3", "--json"]);
    let ids: Vec<&str> = plan["plan"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["profile"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a", "b", "a"]);
    assert_eq!(plan["plan"][0]["label"], "Acct-a");

    // Three `agp -p` at once (they replace themselves with the tool): the
    // same split, not three on a.
    let per = s.lanes(3, &[]);
    assert_eq!(
        (per.get("a"), per.get("b")),
        (Some(&2), Some(&1)),
        "{per:?}"
    );
    // Supervised runs (`--on-limit`) spread the same way.
    let per = s.lanes(3, &["--on-limit", "switch"]);
    assert_eq!(
        (per.get("a"), per.get("b")),
        (Some(&2), Some(&1)),
        "{per:?}"
    );
    // Round-robin: one run per account.
    let per = s.lanes(3, &["--policy", "round-robin"]);
    assert_eq!(per.len(), 3, "{per:?}");
}

#[test]
fn pinned_runs_count_and_status_shows_them() {
    let s = Setup::new();
    let mut pinned = s.cmd(&["--as", "a", "-p", "lane-1"]);
    pinned.env("FAKE_SLEEP", "5");
    let mut child = pinned.spawn().unwrap();
    wait_for("the pinned run's claim", || s.claims() == 1);
    // a: 10% + 25 (the run) + 25 (a new one) is more than b: 30% + 25.
    let pick = s.json(&["pick", "--json"]);
    assert_eq!(pick["chosen"], "b", "{pick}");
    let status = s.json(&["status", "--json"]);
    let view = |id: &str| {
        status["views"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["profile_id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(view("a")["runs"], 1);
    assert_eq!(view("a")["load_5h"], 35.0);
    assert_eq!(view("a")["usable"], true);
    assert_eq!(view("b")["runs"], 0);
    // Under the soft mark (90%) with one more run each time: b (55, 80),
    // a (60, 85) and c (65): five more runs.
    assert_eq!(status["capacity"]["claude"], 5, "{status}");
    assert!(child.wait().unwrap().success());
    // The claim went with the process.
    assert_eq!(s.json(&["pick", "--json"])["chosen"], "a");
}

#[test]
fn print_runs_get_a_session_and_resumes_go_back() {
    let s = Setup::new();
    let (code, _) = (s.cmd(&["--as", "c", "-p", "lane-1"]).status().unwrap(), ());
    assert!(code.success());
    let runs = s.runs();
    let sid = runs[0].2.clone();
    assert!(uuid::Uuid::parse_str(&sid).is_ok(), "{runs:?}");
    let journal = std::fs::read_to_string(s.root.join("state/runs.jsonl")).unwrap();
    let start: Value = serde_json::from_str(journal.lines().next().unwrap()).unwrap();
    assert_eq!(
        (start["session"].as_str(), start["profile"].as_str()),
        (Some(sid.as_str()), Some("c"))
    );

    // A resume goes back to c (its cache is there), though a has more room.
    let _ = std::fs::remove_file(&s.log);
    let st = s.cmd(&["-p", "--resume", &sid, "lane-2"]).status().unwrap();
    assert!(st.success());
    assert_eq!(s.runs()[0].0, "c");
    // So does `-c` in the same folder.
    let _ = std::fs::remove_file(&s.log);
    assert!(s.cmd(&["-p", "-c", "lane-3"]).status().unwrap().success());
    assert_eq!(s.runs()[0].0, "c");
    // A new conversation goes to the account with the most room.
    let _ = std::fs::remove_file(&s.log);
    assert!(s.cmd(&["-p", "lane-4"]).status().unwrap().success());
    assert_eq!(s.runs()[0].0, "a");
}
