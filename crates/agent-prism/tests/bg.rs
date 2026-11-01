//! Background sessions end to end with a fake `claude`
//! (`tests/fake-claude-bg.sh`): `agp --bg`, the detached watcher, a move on
//! a limit (both resume forms), `agp bg watch` and `agp bg move`. Every
//! path points into a temp dir; nothing talks to Anthropic.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const AGP: &str = env!("CARGO_BIN_EXE_agp");

/// A temp setup: two Claude accounts `a` (PB2) and `b` (PB4) sharing
/// `projects/`, both trusting the work dir.
struct Setup {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    log: PathBuf,
}

impl Setup {
    fn new() -> Setup {
        Setup::with_args("", "", "")
    }

    /// With `[claude] args` and the args of profiles a and b (TOML arrays,
    /// "" for none).
    fn with_args(claude: &str, a: &str, b: &str) -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let common = home.join(".claude");
        let work = root.join("work");
        std::fs::create_dir_all(common.join("projects")).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-claude-bg.sh");
        let mut store = serde_json::Map::new();
        let exp = (now() + 7200) * 1000;
        let mut profiles = String::new();
        for (id, label, args) in [("a", "PB2", a), ("b", "PB4", b)] {
            let dir = home.join(format!(".claude-{id}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(common.join("projects"), dir.join("projects")).unwrap();
            let trust = serde_json::json!({"projects": {work.to_str().unwrap(): {"hasTrustDialogAccepted": true}}});
            std::fs::write(dir.join(".claude.json"), trust.to_string()).unwrap();
            let args = if args.is_empty() {
                String::new()
            } else {
                format!("args = {args}\n")
            };
            profiles.push_str(&format!(
                "[[profile]]\nid = \"{id}\"\nlabel = \"{label}\"\ndir = \"{}\"\n{args}\n",
                dir.display()
            ));
            store.insert(
                prism_core::profile::keychain_service_for(&dir, false),
                Value::String(format!(
                    r#"{{"claudeAiOauth":{{"accessToken":"t","expiresAt":{exp},"subscriptionType":"team"}}}}"#
                )),
            );
        }
        let claude = if claude.is_empty() {
            String::new()
        } else {
            format!("args = {claude}\n")
        };
        let config = format!(
            "common_home = \"{}\"\n\n[claude]\nbin = \"{}\"\n{claude}\n{profiles}",
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
            work,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(AGP);
        c.args(args)
            .current_dir(&self.work)
            .env("HOME", self.root.join("home"))
            .env("AGP_CONFIG_DIR", self.root.join("config"))
            .env("AGP_STATE_DIR", self.root.join("state"))
            .env("AGP_SECRET_STORE_FILE", self.root.join("store.json"))
            .env("AGP_NO_POLL", "1")
            .env("AGP_BG_POLL_MS", "200")
            .env("FAKE_LOG", &self.log)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("AGP_SOCK")
            .stdin(Stdio::null());
        c
    }

    /// Runs agp to the end; (code, stdout, stderr).
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = self.cmd(args).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.root.join(format!("home/.claude-{id}"))
    }

    fn sessions(&self) -> Vec<Value> {
        let (code, out, err) = self.run(&["bg", "--json"]);
        assert_eq!(code, 0, "{err}");
        let v: Value = serde_json::from_str(&out).unwrap();
        v["sessions"].as_array().unwrap().clone()
    }

    /// Waits until the one session matches `f`.
    fn wait_for(&self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let s = self.sessions();
            if let Some(v) = s.first().filter(|v| f(v)) {
                return v.clone();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {s:#?}\nlog:\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn state_file(&self, id: &str, short: &str) -> PathBuf {
        self.dir(id).join(format!("jobs/{short}/state.json"))
    }

    /// Makes the job in `id` blocked on a rate limit, as Claude Code does.
    fn limit(&self, id: &str, short: &str, stamp: &str) {
        let f = self.state_file(id, short);
        let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        let detail = "You've hit your 5-hour limit · resets 6:10am (Europe/Madrid)";
        v["state"] = "blocked".into();
        v["detail"] = detail.into();
        v["needs"] = format!("rate limited — wait and retry · {detail}").into();
        v["updatedAt"] = stamp.into();
        std::fs::write(&f, v.to_string()).unwrap();
    }

    fn job_pid(&self, id: &str, short: &str) -> Option<i32> {
        std::fs::read_to_string(self.dir(id).join(format!("jobs/{short}/pid")))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}

impl Drop for Setup {
    /// Stops every watcher and fake session process this test started.
    fn drop(&mut self) {
        if let Ok(rd) = std::fs::read_dir(self.root.join("state/bg")) {
            for e in rd.flatten() {
                let Ok(t) = std::fs::read_to_string(e.path()) else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<Value>(&t) else {
                    continue;
                };
                if let Some(pid) = v["watcher_pid"].as_i64() {
                    kill(pid as i32);
                }
            }
        }
        for id in ["a", "b"] {
            let Ok(rd) = std::fs::read_dir(self.dir(id).join("jobs")) else {
                continue;
            };
            for e in rd.flatten() {
                if let Ok(p) = std::fs::read_to_string(e.path().join("pid"))
                    && let Ok(p) = p.trim().parse::<i32>()
                {
                    kill(p);
                }
            }
        }
    }
}

fn kill(pid: i32) {
    if pid > 0 {
        // SAFETY: signalling a process this test started.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn pgid(pid: i32) -> String {
    let out = Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Launches `--bg` with pipes the way chief does and checks that agp exits
/// at once and that nothing keeps the pipes open.
fn launch(s: &Setup, opts: &[&str]) -> (String, String) {
    launch_with(s, opts, &[])
}

/// `launch` with more claude args before the prompt.
fn launch_with(s: &Setup, opts: &[&str], extra: &[&str]) -> (String, String) {
    let mut args: Vec<&str> = opts.to_vec();
    args.extend(["--", "--bg", "--name", "n"]);
    args.extend(extra);
    args.push("do the task");
    let t = Instant::now();
    let mut child = s
        .cmd(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let err = std::thread::spawn(move || {
        let mut b = String::new();
        se.read_to_string(&mut b).unwrap();
        b
    });
    let mut out = String::new();
    so.read_to_string(&mut out).unwrap();
    let err = err.join().unwrap();
    let status = child.wait().unwrap();
    // Both pipes closed and agp gone, with the watcher still running.
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    assert_eq!(status.code(), Some(0), "stdout: {out}\nstderr: {err}");
    // stdout is claude's, unchanged.
    assert!(out.contains("\x1b[1mbackgrounded\x1b[0m · "), "{out:?}");
    assert!(out.contains("note: the session runs in the background"));
    (out, err)
}

#[test]
fn launch_watch_and_move_on_limits() {
    let s = Setup::new();
    let (out, _) = launch(&s, &["--as", "a", "--on-limit", "switch"]);
    let rec = s.wait_for("the watcher", |v| {
        v["watching"] == true && v["watcher_pid"].is_i64()
    });
    let short = rec["short_id"].as_str().unwrap().to_string();
    let sid = rec["session_id"].as_str().unwrap().to_string();
    assert!(out.contains(&short));
    assert!(sid.starts_with(&short));
    assert_eq!(rec["profile"], "a");
    assert_eq!(rec["label"], "PB2");
    assert_eq!(rec["config_dir"], s.dir("a").to_str().unwrap());
    assert_eq!(rec["cwd"], s.work.to_str().unwrap());
    assert_eq!(rec["name"], "n");
    assert_eq!(rec["on_limit"], "switch");
    assert_eq!(rec["state"], "watching");
    assert_eq!(rec["limit"], Value::Null);
    // The watcher is detached: its own process group.
    let wpid = rec["watcher_pid"].as_i64().unwrap() as i32;
    assert!(alive(wpid));
    assert_eq!(pgid(wpid), wpid.to_string());
    // The launch ran in a's env.
    let log = s.log_text();
    assert!(
        log.contains(&format!("cfg={} ", s.dir("a").display())),
        "{log}"
    );
    let old_pid = s.job_pid("a", &short).unwrap();

    // A 5-hour limit in a: the session moves to b with the saved flags.
    s.limit("a", &short, "2026-11-01T04:00:00.000Z");
    let rec = s.wait_for("the move to b", |v| {
        v["profile"] == "b" && v["state"] == "watching"
    });
    assert_eq!(rec["short_id"], short.as_str());
    assert_eq!(rec["session_id"], sid.as_str());
    assert_eq!(rec["config_dir"], s.dir("b").to_str().unwrap());
    let mv = &rec["moves"][0];
    assert_eq!(mv["from"], "a");
    assert_eq!(mv["from_label"], "PB2");
    assert_eq!(mv["to"], "b");
    assert_eq!(mv["to_label"], "PB4");
    assert_eq!(mv["to_config_dir"], s.dir("b").to_str().unwrap());
    assert_eq!(mv["reason"], "5-hour limit");
    assert_eq!(
        rec["previous"],
        serde_json::json!([{"profile": "a", "label": "PB2", "config_dir": s.dir("a").to_str().unwrap()}])
    );
    assert!(!alive(old_pid), "the old process still runs");
    assert!(s.job_pid("b", &short).is_some_and(alive));
    let log = s.log_text();
    let b_resume = log
        .lines()
        .find(|l| {
            l.starts_with(&format!("cfg={} ", s.dir("b").display())) && l.contains("--resume")
        })
        .unwrap_or_else(|| panic!("no resume in b:\n{log}"));
    // b had no job: the flags form, with the saved --name.
    assert!(
        b_resume.contains(&format!("--bg --resume {sid} --name n")),
        "{b_resume}"
    );
    assert!(log.contains(&format!("args=stop {short}")));
    assert!(log.contains("This session moved from account PB2 to PB4 after the 5-hour limit"));
    // Never a copy, never `claude rm`.
    assert!(!log.contains("started a copy"), "{log}");
    assert!(!log.contains("args=rm"));
    // The old job stays, stopped, in a.
    assert!(s.state_file("a", &short).is_file());

    // A limit in b: back to a, which has the job, so the no-flag wake.
    s.limit("b", &short, "2026-11-01T05:00:00.000Z");
    let rec = s.wait_for("the move back to a", |v| {
        v["profile"] == "a" && v["moves"].as_array().unwrap().len() == 2
    });
    assert_eq!(rec["state"], "watching");
    assert_eq!(rec["previous"][0]["profile"], "b");
    assert_eq!(rec["previous"].as_array().unwrap().len(), 1);
    let log = s.log_text();
    let a_resume = log
        .lines()
        .rfind(|l| {
            l.starts_with(&format!("cfg={} ", s.dir("a").display())) && l.contains("--resume")
        })
        .unwrap();
    assert!(
        a_resume.ends_with(&format!("args=--bg --resume {sid} [agent-prism] This session moved from account PB4 to PB2 after the 5-hour limit; the old process's background tasks and subagents stopped, so start again the ones you still need.")),
        "{a_resume}"
    );
    assert!(!log.contains("started a copy"), "{log}");
    assert!(s.job_pid("a", &short).is_some_and(alive));

    // The same limit seen again is not handled twice.
    let moves = rec["moves"].as_array().unwrap().len();
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(s.sessions()[0]["moves"].as_array().unwrap().len(), moves);

    // The job gone: the watch ends.
    kill(s.job_pid("a", &short).unwrap());
    std::fs::remove_dir_all(s.dir("a").join(format!("jobs/{short}"))).unwrap();
    let rec = s.wait_for("the end", |v| v["state"] == "ended");
    assert_eq!(rec["watching"], false);
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(wpid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !alive(wpid) || pgid(wpid).is_empty(),
        "the watcher still runs"
    );
    // The run journal has the whole story under the launch's run id.
    let journal = std::fs::read_to_string(s.root.join("state/runs.jsonl")).unwrap();
    let evs: Vec<Value> = journal
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let kinds: Vec<&str> = evs.iter().map(|e| e["ev"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["start", "limit", "switch", "limit", "switch", "end"]
    );
    assert!(
        evs.iter()
            .all(|e| e["mode"] == "bg" && e["run"] == evs[0]["run"])
    );
}

#[test]
fn watch_and_move_commands() {
    let s = Setup::new();
    // `stay` (here from `fail`): a record, no watcher.
    launch(&s, &["--as", "a", "--on-limit", "fail"]);
    std::thread::sleep(Duration::from_millis(300));
    let rec = s.sessions()[0].clone();
    assert_eq!(rec["on_limit"], "stay");
    assert_eq!(rec["state"], "watching");
    assert_eq!(rec["watching"], false);
    assert_eq!(rec["watcher_pid"], Value::Null);
    let jobs: Vec<_> = std::fs::read_dir(s.dir("a").join("jobs"))
        .unwrap()
        .flatten()
        .collect();
    let short = jobs[0].file_name().to_string_lossy().into_owned();

    // `agp bg watch` arms one; again: it keeps running with new options.
    let (code, out, err) = s.run(&["bg", "watch", &short, "--on-limit", "wait"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.starts_with(&format!("watching {short}")), "{out}");
    let rec = s.wait_for("the watcher", |v| {
        v["watching"] == true && v["watcher_pid"].is_i64()
    });
    assert_eq!(rec["on_limit"], "wait");
    assert_eq!(rec["profile"], "a");
    let wpid = rec["watcher_pid"].as_i64().unwrap();
    let (code, out, _) = s.run(&[
        "bg",
        "watch",
        &short,
        "--on-limit",
        "switch",
        "--max-wait",
        "2h",
    ]);
    assert_eq!(code, 0);
    assert!(out.contains("already runs"), "{out}");
    let rec = s.wait_for("the new options", |v| v["on_limit"] == "switch");
    assert_eq!(rec["watcher_pid"].as_i64(), Some(wpid));
    assert_eq!(rec["options"]["max_wait"], 7200);

    // `agp bg move`: now, with `moving` in the record before the stop.
    let (code, out, err) = s
        .cmd(&["bg", "move", &short, "--to", "b", "--json"])
        .env("FAKE_SNAPSHOT", "1")
        .output()
        .map(|o| {
            (
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stdout).into_owned(),
                String::from_utf8_lossy(&o.stderr).into_owned(),
            )
        })
        .unwrap();
    assert_eq!(code, 0, "{err}\n{}", s.log_text());
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["moved"], true);
    assert_eq!(v["short_id"], short.as_str());
    assert_eq!(v["from"], "a");
    assert_eq!(v["from_label"], "PB2");
    assert_eq!(v["to"], "b");
    assert_eq!(v["to_label"], "PB4");
    assert_eq!(v["to_config_dir"], s.dir("b").to_str().unwrap());
    assert_eq!(
        v["session_id"].as_str().map(|x| x.starts_with(&short)),
        Some(true)
    );
    let snap =
        std::fs::read_to_string(s.dir("a").join(format!("jobs/{short}/record-at-stop.json")))
            .unwrap();
    let snap: Value = serde_json::from_str(&snap).unwrap();
    assert_eq!(snap["state"], "moving");
    let rec = s.sessions()[0].clone();
    assert_eq!(rec["profile"], "b");
    assert_eq!(rec["state"], "watching");
    assert_eq!(rec["moves"][0]["reason"], "manual move");
    assert!(
        s.log_text()
            .contains("moved from account PB2 to PB4 on request")
    );

    // Someone revives it in b mid-move: the move to a is aborted.
    let (code, _, err) = s
        .cmd(&["bg", "move", &short])
        .env("FAKE_REVIVE_AFTER_STOP", "2")
        .output()
        .map(|o| {
            (
                o.status.code().unwrap_or(-1),
                String::new(),
                String::from_utf8_lossy(&o.stderr).into_owned(),
            )
        })
        .unwrap();
    assert_ne!(code, 0);
    assert!(
        err.contains("move aborted: the session runs again in PB4"),
        "{err}"
    );
    let rec = s.sessions()[0].clone();
    assert_eq!(rec["profile"], "b");
    assert_eq!(rec["state"], "watching");
    assert_eq!(rec["moves"].as_array().unwrap().len(), 1);
    let log = s.log_text();
    let resumes_in_a = log
        .lines()
        .filter(|l| {
            l.starts_with(&format!("cfg={} ", s.dir("a").display())) && l.contains("--resume")
        })
        .count();
    assert_eq!(resumes_in_a, 0, "{log}");

    // Someone stops it in b and wakes it in a by hand: the watch follows.
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-claude-bg.sh");
    let sid = rec["session_id"].as_str().unwrap().to_string();
    let by_hand = |id: &str, args: &[&str]| {
        let st = Command::new(&fake)
            .args(args)
            .current_dir(&s.work)
            .env("HOME", s.root.join("home"))
            .env("CLAUDE_CONFIG_DIR", s.dir(id))
            .env("FAKE_LOG", &s.log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success());
    };
    by_hand("b", &["stop", &short]);
    // Older than the margin a move's own writes get.
    std::thread::sleep(Duration::from_millis(2200));
    by_hand("a", &["--bg", "--resume", &sid, "go on"]);
    let rec = s.wait_for("the watch to follow to a", |v| v["profile"] == "a");
    assert_eq!(rec["state"], "watching");
    assert_eq!(rec["watcher_pid"].as_i64(), Some(wpid));
    assert!(
        rec["note"].as_str().unwrap().contains("runs in PB2 now"),
        "{rec}"
    );

    // Unknown sessions and accounts are errors.
    assert_ne!(s.run(&["bg", "move", "00000000"]).0, 0);
    assert_ne!(s.run(&["bg", "move", &short, "--to", "nope"]).0, 0);
    assert_ne!(s.run(&["bg", "watch", "00000000"]).0, 0);
}

#[test]
fn help_has_bg_lines_and_needs_no_stdin() {
    let s = Setup::new();
    let (code, out, _) = s.run(&["--help"]);
    assert_eq!(code, 0);
    assert!(
        out.lines()
            .any(|l| l.trim_start().starts_with("agp bg [--json]"))
    );
    assert!(
        out.lines()
            .any(|l| l.trim_start().starts_with("agp bg watch <session>"))
    );
    assert!(
        out.lines()
            .any(|l| l.trim_start().starts_with("agp bg move <session>"))
    );
}

/// The value after `--settings` in a fake log line.
fn settings_arg(line: &str) -> String {
    let mut it = line.split(' ');
    it.find(|w| *w == "--settings");
    it.next().unwrap_or_default().to_string()
}

fn read_json(path: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn settings_files_and_profile_args() {
    let s = Setup::with_args(
        r#"["--permission-mode", "auto"]"#,
        r#"["--model", "opus-a"]"#,
        r#"["--effort", "high"]"#,
    );
    // An overlay for each account, so there is something to merge even if
    // the memory dir is off in this environment.
    for id in ["a", "b"] {
        let o = s.root.join(format!("config/profiles/{id}/settings.json"));
        std::fs::create_dir_all(o.parent().unwrap()).unwrap();
        std::fs::write(&o, format!(r#"{{"env":{{"ACCOUNT":"{id}"}}}}"#)).unwrap();
    }
    let hooks = |v: &str| {
        let h = format!(
            r#"{{"hooks":{{"Stop":[{{"hooks":[{{"type":"command","command":"echo {v}"}}]}}]}}}}"#
        );
        std::fs::write(s.work.join("hooks.json"), h).unwrap();
    };
    hooks("v1");
    // A relative --settings path, as chief may pass one.
    launch_with(
        &s,
        &["--as", "a", "--on-limit", "switch"],
        &["--settings", "hooks.json"],
    );
    let rec = s.wait_for("the watcher", |v| {
        v["watching"] == true && v["watcher_pid"].is_i64()
    });
    let short = rec["short_id"].as_str().unwrap().to_string();
    let sid = rec["session_id"].as_str().unwrap().to_string();
    assert_eq!(
        rec["launch"]["added"]["a"],
        serde_json::json!(["--permission-mode", "auto", "--model", "opus-a"])
    );
    assert_eq!(
        rec["launch"]["user_settings"],
        s.work.join("hooks.json").to_str().unwrap()
    );

    // The launch passes a file of agp's, not inline JSON.
    let log = s.log_text();
    let line = log
        .lines()
        .find(|l| l.contains("args=--permission-mode"))
        .unwrap();
    assert!(
        line.contains(
            "args=--permission-mode auto --model opus-a --bg --name n do the task --settings "
        ),
        "{line}"
    );
    let file_a = settings_arg(line);
    let dir = s.root.join("state/bg/settings");
    assert!(
        file_a.starts_with(dir.to_str().unwrap()) && file_a.ends_with("-a.json"),
        "{file_a}"
    );
    let v = read_json(&file_a);
    assert_eq!(v["env"]["ACCOUNT"], "a");
    assert!(v.to_string().contains("echo v1"));

    // chief rewrites its hooks and calls `agp bg watch` before a respawn.
    hooks("v2");
    let (code, out, err) = s.run(&["bg", "watch", &short]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("already runs"), "{out}");
    let v = read_json(&file_a);
    assert!(v.to_string().contains("echo v2"), "{v}");
    assert!(!v.to_string().contains("echo v1"));
    assert_eq!(v["env"]["ACCOUNT"], "a");

    // A move to b: b's own file and args, a's args gone.
    let (code, _, err) = s.run(&["bg", "move", &short, "--to", "b", "--json"]);
    assert_eq!(code, 0, "{err}\n{}", s.log_text());
    let log = s.log_text();
    let b_line = log
        .lines()
        .find(|l| {
            l.starts_with(&format!("cfg={} ", s.dir("b").display())) && l.contains("--resume")
        })
        .unwrap();
    assert!(
        b_line.contains(&format!(
            "args=--bg --resume {sid} --permission-mode auto --effort high --name n --settings "
        )),
        "{b_line}"
    );
    assert!(!b_line.contains("opus-a"), "{b_line}");
    let file_b = settings_arg(b_line);
    assert_eq!(file_b, file_a.replace("-a.json", "-b.json"));
    let v = read_json(&file_b);
    assert_eq!(v["env"]["ACCOUNT"], "b");
    assert!(v.to_string().contains("echo v2"));
    let rec = s.sessions()[0].clone();
    assert_eq!(
        rec["launch"]["added"]["b"],
        serde_json::json!(["--permission-mode", "auto", "--effort", "high"])
    );

    // Back to a: the wake form, after a's file is written again.
    hooks("v3");
    let (code, _, err) = s.run(&["bg", "move", &short, "--to", "a"]);
    assert_eq!(code, 0, "{err}\n{}", s.log_text());
    let log = s.log_text();
    let a_line = log
        .lines()
        .rfind(|l| {
            l.starts_with(&format!("cfg={} ", s.dir("a").display())) && l.contains("--resume")
        })
        .unwrap();
    assert!(
        a_line.contains(&format!("args=--bg --resume {sid} [agent-prism]")),
        "{a_line}"
    );
    assert!(read_json(&file_a).to_string().contains("echo v3"));
    // b's file is only written when the session goes back to b.
    assert!(read_json(&file_b).to_string().contains("echo v2"));
}
