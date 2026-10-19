use super::*;
use std::fs;

use crate::config::ShareConfig;

fn cfg() -> ShareRules {
    ShareRules::default()
}

fn codex() -> ShareRules {
    ShareRules {
        tool: Tool::Codex,
        extra: ShareConfig::default(),
    }
}

#[test]
fn classify_classes() {
    let c = cfg();
    for n in [
        "CLAUDE.md",
        "RTK.md",
        "settings.json",
        "projects",
        "statusline.sh",
        "statusline-profile.sh",
        "history.jsonl",
    ] {
        assert_eq!(classify(n, &c), ShareClass::Shared, "{n}");
    }
    for n in [
        ".claude.json",
        ".last-cleanup",
        ".last-update-result.json",
        "daemon",
        "daemon.log",
        ".cache",
        "policy-limits.json.stamp.json",
        ".oauth_refresh.lock",
    ] {
        assert_eq!(classify(n, &c), ShareClass::Local, "{n}");
    }
    for n in [
        "scheduled",
        "state",
        "settings.json.bak",
        "statusline-burnrate.sh.bak",
        "x.premerge",
        "weird",
    ] {
        assert_eq!(classify(n, &c), ShareClass::Unmanaged, "{n}");
    }
    let c = ShareRules {
        tool: Tool::Claude,
        extra: ShareConfig {
            extra_shared: vec!["scheduled".into(), "projects".into()],
            extra_local: vec!["projects".into(), "my-*".into()],
        },
    };
    assert_eq!(classify("scheduled", &c), ShareClass::Shared);
    assert_eq!(classify("projects", &c), ShareClass::Local);
    assert_eq!(classify("my-thing", &c), ShareClass::Local);
}

#[test]
fn classify_codex() {
    let c = codex();
    for n in [
        "AGENTS.md",
        "config.toml",
        "hooks.json",
        "skills",
        "sessions",
        "archived_sessions",
        "history.jsonl",
        "session_index.jsonl",
        "memories",
    ] {
        assert_eq!(classify(n, &c), ShareClass::Shared, "{n}");
    }
    for n in [
        "auth.json",
        "state_5.sqlite",
        "state_5.sqlite-wal",
        "thread_history_1.sqlite",
        "models_cache.json",
        "app-server-daemon",
        ".tmp",
    ] {
        assert_eq!(classify(n, &c), ShareClass::Local, "{n}");
    }
    for n in [
        "config.toml.bak-x",
        "config.toml.rea.backup",
        "packages",
        "weird",
    ] {
        assert_eq!(classify(n, &c), ShareClass::Unmanaged, "{n}");
    }
    // Claude names mean nothing in a Codex home.
    assert_eq!(classify("projects", &c), ShareClass::Unmanaged);
    assert_eq!(sessions_entry(Tool::Codex), "sessions");
}

#[test]
fn codex_adopt_and_health() {
    let tmp = tempfile::tempdir().unwrap();
    let common = tmp.path().join(".codex");
    let dir = tmp.path().join(".codex-b");
    fs::create_dir_all(common.join("sessions/2026")).unwrap();
    fs::write(common.join("config.toml"), "model = \"a\"\n").unwrap();
    fs::write(common.join("AGENTS.md"), "x").unwrap();
    fs::write(
        common.join("history.jsonl"),
        "{\"session_id\":\"a\",\"ts\":10,\"text\":\"one\"}\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("sessions/2025")).unwrap();
    fs::write(dir.join("sessions/2025/r.jsonl"), "r").unwrap();
    fs::write(dir.join("config.toml"), "model = \"b\"\n").unwrap();
    fs::write(
        dir.join("history.jsonl"),
        "{\"session_id\":\"b\",\"ts\":5,\"text\":\"two\"}\n",
    )
    .unwrap();
    fs::write(dir.join("auth.json"), "{}").unwrap();
    fs::write(dir.join("state_5.sqlite"), "db").unwrap();
    let h = share_health(&dir, &common, &codex()).unwrap();
    assert!(!h.sessions_shared && h.to_fix > 0);
    let plan = plan_adopt("b", &dir, &common, &codex(), Path::new("/no-overlay")).unwrap();
    let jdir = tmp.path().join("journal");
    let j = apply_adopt(&plan, &jdir).unwrap();
    // config.toml differs and is not JSON: a conflict copy, no overlay.
    assert!(
        j.entries
            .iter()
            .any(|e| e.op == "conflict" && e.name == "config.toml")
    );
    assert!(!j.entries.iter().any(|e| e.op == "overlay"));
    assert!(common.join("sessions/2025/r.jsonl").is_file());
    let hist = fs::read_to_string(common.join("history.jsonl")).unwrap();
    assert!(
        hist.find("two").unwrap() < hist.find("one").unwrap(),
        "{hist}"
    );
    // Local entries stay.
    assert!(dir.join("auth.json").is_file() && dir.join("state_5.sqlite").is_file());
    let h = share_health(&dir, &common, &codex()).unwrap();
    assert!(h.sessions_shared && h.to_fix == 0, "{h:?}");
    // A new Codex profile links the shared entries and makes no Claude dirs.
    let n = tmp.path().join(".codex-n");
    create_profile_dir(&n, &common, &codex()).unwrap();
    assert!(n.join("AGENTS.md").is_symlink() && n.join("sessions").is_symlink());
    assert!(!n.join("daemon").exists());
}

#[test]
fn glob_basics() {
    assert!(glob("*.md", "a.md"));
    assert!(!glob("*.md", "a.mdx"));
    assert!(glob("statusline*.sh", "statusline.sh"));
    assert!(glob("*.bak*", "settings.json.bak.2026"));
    assert!(!glob("a*b*c", "ac"));
}

#[test]
fn history_order() {
    let common = vec![
        r#"{"display":"b","timestamp":2000}"#.to_string(),
        "no-ts-1".to_string(),
    ];
    let prof = vec![
        r#"{"display":"a","timestamp":"1970-01-01T00:00:01Z"}"#.to_string(),
        r#"{"display":"b","timestamp":2000}"#.to_string(),
        "no-ts-2".to_string(),
        r#"{"display":"c","timestamp":3000}"#.to_string(),
    ];
    let m = merge_history(&common, &prof);
    assert_eq!(
        m,
        vec![
            prof[0].clone(),
            common[0].clone(),
            prof[3].clone(),
            "no-ts-1".into(),
            "no-ts-2".into()
        ]
    );
}

struct Fx {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    common: PathBuf,
    overlay: PathBuf,
    journal: PathBuf,
}

fn w(p: &Path, s: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, s).unwrap();
}

fn fixture() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let common = tmp.path().join("home/.claude");
    let dir = tmp.path().join("home/.claude-x");
    fs::create_dir_all(&dir).unwrap();
    // Shared entry missing in the profile.
    w(&common.join("CLAUDE.md"), "rules");
    // projects: real dir in the profile.
    w(&common.join("projects/p1/same.jsonl"), "same");
    w(
        &common.join("projects/p1/memory/MEMORY.md"),
        "# Memory\n- [a](a.md)\n",
    );
    w(&common.join("projects/p1/memory/note.md"), "common note");
    // The memory store's repo in common.
    w(&common.join("projects/p1/memory/.git/HEAD"), "ref");
    w(&common.join("projects/p1/memory/.gitignore"), "*.lock\n");
    w(&common.join("projects/p1/memory/.memory-store"), "{}");
    w(&common.join("projects/p1/conf.jsonl"), "common conf");
    w(&dir.join("projects/p1/conf.jsonl"), "profile conf");
    w(&dir.join("projects/p1/same.jsonl"), "same");
    w(
        &dir.join("projects/p1/memory/MEMORY.md"),
        "# Memory\n\n- [b](b.md)\n- [a](a.md)\n",
    );
    w(&dir.join("projects/p1/memory/note.md"), "profile note");
    w(&dir.join("projects/p1/memory/new.md"), "new memory");
    // Same bytes as common, but never compared with the store.
    w(&dir.join("projects/p1/memory/.gitignore"), "*.lock\n");
    w(&dir.join("projects/p1/new-session.jsonl"), "new");
    w(&dir.join("projects/p2/s.jsonl"), "p2");
    // A memory dir linked by hand to the common one: dropped, not a conflict.
    w(&common.join("projects/p3/memory/MEMORY.md"), "# p3\n");
    fs::create_dir_all(dir.join("projects/p3")).unwrap();
    symlink(
        common.join("projects/p3/memory"),
        dir.join("projects/p3/memory"),
    )
    .unwrap();
    // history in both.
    w(
        &common.join("history.jsonl"),
        "{\"display\":\"c1\",\"timestamp\":1000}\n{\"display\":\"c3\",\"timestamp\":3000}\n",
    );
    w(
        &dir.join("history.jsonl"),
        "{\"display\":\"p2\",\"timestamp\":2000}\n{\"display\":\"c3\",\"timestamp\":3000}\n",
    );
    // .cache linked into common (clausona leftover).
    fs::create_dir_all(common.join(".cache")).unwrap();
    w(&common.join(".cache/x"), "cache");
    symlink(common.join(".cache"), dir.join(".cache")).unwrap();
    // settings differ.
    w(
        &common.join("settings.json"),
        r#"{"model":"opus","env":{"A":"1"},"same":true}"#,
    );
    w(
        &dir.join("settings.json"),
        r#"{"model":"sonnet","env":{"A":"2"},"same":true}"#,
    );
    // agents links elsewhere.
    fs::create_dir_all(tmp.path().join("elsewhere")).unwrap();
    fs::create_dir_all(common.join("agents")).unwrap();
    symlink(tmp.path().join("elsewhere"), dir.join("agents")).unwrap();
    // unmanaged + local real file.
    w(&dir.join("scheduled/job"), "j");
    w(&dir.join(".claude.json"), "{}");
    Fx {
        overlay: tmp.path().join("config/profiles/x/settings.json"),
        journal: tmp.path().join("state/adopt"),
        dir,
        common,
        _tmp: tmp,
    }
}

fn find<'a>(plan: &'a AdoptPlan, n: &str) -> &'a Action {
    plan.actions
        .iter()
        .find(|a| match a {
            Action::Link { name }
            | Action::Merge { name, .. }
            | Action::Relink { name, .. }
            | Action::Localize { name, .. }
            | Action::Skip { name, .. } => name == n,
        })
        .unwrap_or_else(|| panic!("no action for {n}"))
}

#[test]
fn plan_apply_replan() {
    let f = fixture();
    let plan = plan_adopt("x", &f.dir, &f.common, &cfg(), &f.overlay).unwrap();
    assert_eq!(
        find(&plan, "CLAUDE.md"),
        &Action::Link {
            name: "CLAUDE.md".into()
        }
    );
    match find(&plan, "projects") {
        Action::Merge { preview, .. } => {
            assert_eq!(preview.move_files, 3);
            assert_eq!(preview.identical, 2);
            assert_eq!(
                preview.conflicts,
                vec![PathBuf::from("projects/p1/conf.jsonl")]
            );
            assert_eq!(
                preview.memory_conflicts,
                vec![
                    PathBuf::from("projects/p1/memory/.gitignore"),
                    PathBuf::from("projects/p1/memory/MEMORY.md"),
                    PathBuf::from("projects/p1/memory/note.md")
                ]
            );
        }
        a => panic!("{a:?}"),
    }
    match find(&plan, "settings.json") {
        Action::Merge { preview, .. } => assert_eq!(preview.overlay_keys, vec!["env", "model"]),
        a => panic!("{a:?}"),
    }
    assert!(
        matches!(find(&plan, "history.jsonl"), Action::Merge { preview, .. } if preview.move_files == 1)
    );
    assert_eq!(
        find(&plan, ".cache"),
        &Action::Localize {
            name: ".cache".into(),
            was_dir: true
        }
    );
    assert!(matches!(find(&plan, "agents"), Action::Relink { .. }));
    assert_eq!(
        find(&plan, "scheduled"),
        &Action::Skip {
            name: "scheduled".into(),
            reason: "unmanaged".into()
        }
    );
    assert!(!plan.is_noop());
    assert!(plan.summary().contains("link: CLAUDE.md"));
    assert!(
        plan.summary().contains("3 memory conflict"),
        "{}",
        plan.summary()
    );
    // Planning is read-only.
    assert!(
        fs::symlink_metadata(f.dir.join("projects"))
            .unwrap()
            .is_dir()
    );
    let h = share_health(&f.dir, &f.common, &cfg()).unwrap();
    assert!(!h.sessions_shared && h.to_fix == 6 && h.unmanaged == vec!["scheduled"]);

    let journal = apply_adopt(&plan, &f.journal).unwrap();
    assert!(journal.path.exists() && !journal.entries.is_empty());

    for n in [
        "CLAUDE.md",
        "projects",
        "history.jsonl",
        "settings.json",
        "agents",
    ] {
        let p = f.dir.join(n);
        assert!(
            fs::symlink_metadata(&p).unwrap().file_type().is_symlink(),
            "{n}"
        );
        assert_eq!(fs::read_link(&p).unwrap(), f.common.join(n));
    }
    assert_eq!(
        fs::read_to_string(f.common.join("projects/p1/new-session.jsonl")).unwrap(),
        "new"
    );
    assert_eq!(
        fs::read_to_string(f.common.join("projects/p2/s.jsonl")).unwrap(),
        "p2"
    );
    // Differing memory goes aside; the live memory dir is not edited.
    let mem = f.common.join("projects/p1/memory");
    assert_eq!(
        fs::read_to_string(mem.join("MEMORY.md")).unwrap(),
        "# Memory\n- [a](a.md)\n"
    );
    assert_eq!(
        fs::read_to_string(mem.join("note.md")).unwrap(),
        "common note"
    );
    assert_eq!(
        fs::read_to_string(mem.join("new.md")).unwrap(),
        "new memory"
    );
    assert_eq!(fs::read_to_string(mem.join(".git/HEAD")).unwrap(), "ref");
    assert_eq!(fs::read_to_string(mem.join(".memory-store")).unwrap(), "{}");
    let mut names: Vec<_> = fs::read_dir(&mem)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            ".git",
            ".gitignore",
            ".memory-store",
            "MEMORY.md",
            "new.md",
            "note.md"
        ]
    );
    let parked = f.common.join(".agent-prism-conflicts/x/projects/p1/memory");
    assert_eq!(
        fs::read_to_string(parked.join("MEMORY.md")).unwrap(),
        "# Memory\n\n- [b](b.md)\n- [a](a.md)\n"
    );
    assert_eq!(
        fs::read_to_string(parked.join("note.md")).unwrap(),
        "profile note"
    );
    assert_eq!(
        parked_memory_conflicts(&f.common, "x").unwrap(),
        vec![
            parked.join(".gitignore"),
            parked.join("MEMORY.md"),
            parked.join("note.md")
        ]
    );
    assert!(
        parked_memory_conflicts(&f.common, "other")
            .unwrap()
            .is_empty()
    );
    let moved: Vec<_> = journal.memory_conflicts().map(|(_, d)| d).collect();
    assert_eq!(
        moved,
        [
            parked.join(".gitignore"),
            parked.join("MEMORY.md"),
            parked.join("note.md")
        ]
    );
    assert_eq!(
        fs::read_to_string(
            f.common
                .join(".agent-prism-conflicts/x/projects/p1/conf.jsonl")
        )
        .unwrap(),
        "profile conf"
    );
    let ops: Vec<_> = journal.entries.iter().map(|e| e.op.as_str()).collect();
    assert!(ops.contains(&"conflict") && ops.contains(&"memory-conflict"));
    assert_eq!(
        fs::read_to_string(f.common.join("projects/p3/memory/MEMORY.md")).unwrap(),
        "# p3\n"
    );
    let hist = fs::read_to_string(f.common.join("history.jsonl")).unwrap();
    let order: Vec<_> = hist.lines().map(|l| line_ts(l).unwrap()).collect();
    assert_eq!(order, vec![1000, 2000, 3000]);
    let overlay: Value = serde_json::from_slice(&fs::read(&f.overlay).unwrap()).unwrap();
    assert_eq!(
        overlay,
        serde_json::json!({"model":"sonnet","env":{"A":"2"}})
    );
    assert_eq!(
        fs::read_to_string(f.common.join("settings.json")).unwrap(),
        r#"{"model":"opus","env":{"A":"1"},"same":true}"#
    );
    let cache = fs::symlink_metadata(f.dir.join(".cache")).unwrap();
    assert!(cache.is_dir() && fs::read_dir(f.dir.join(".cache")).unwrap().next().is_none());
    assert!(f.common.join(".cache/x").exists());
    assert!(f.dir.join("scheduled/job").exists());

    let again = plan_adopt("x", &f.dir, &f.common, &cfg(), &f.overlay).unwrap();
    assert!(again.is_noop(), "{}", again.summary());
    let h = share_health(&f.dir, &f.common, &cfg()).unwrap();
    assert!(h.sessions_shared && h.to_fix == 0);
}

#[test]
fn create_dir_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let common = tmp.path().join(".claude");
    w(&common.join("CLAUDE.md"), "x");
    fs::create_dir_all(common.join("projects")).unwrap();
    fs::create_dir_all(common.join("sessions")).unwrap();
    let dir = tmp.path().join(".claude-new");
    create_profile_dir(&dir, &common, &cfg()).unwrap();
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::read_link(dir.join("projects")).unwrap(),
        common.join("projects")
    );
    assert!(fs::symlink_metadata(dir.join("sessions")).unwrap().is_dir());
    assert!(
        plan_adopt("n", &dir, &common, &cfg(), Path::new("/x"))
            .unwrap()
            .is_noop()
    );
}

#[test]
fn move_in_never_replaces() {
    let f = fixture();
    let plan = plan_adopt("x", &f.dir, &f.common, &cfg(), &f.overlay).unwrap();
    let mut ap = Applier {
        plan: &plan,
        entries: Vec::new(),
    };
    let rel = Path::new("p1/memory/late.md");
    let (pe, ce) = (
        f.dir.join("projects").join(rel),
        f.common.join("projects").join(rel),
    );
    // Another account wrote the same file after the plan: identical -> drop.
    w(&pe, "same");
    w(&ce, "same");
    ap.move_in("projects", &pe, &ce, rel).unwrap();
    assert!(!pe.exists());
    // Different -> the common file stays, the profile copy goes aside.
    w(&pe, "mine");
    w(&ce, "theirs");
    ap.move_in("projects", &pe, &ce, rel).unwrap();
    assert!(!pe.exists());
    assert_eq!(fs::read_to_string(&ce).unwrap(), "theirs");
    let parked = f.common.join(".agent-prism-conflicts/x/projects").join(rel);
    assert_eq!(fs::read_to_string(&parked).unwrap(), "mine");
    // Absent -> moved.
    let rel = Path::new("p1/memory/fresh.md");
    let (pe, ce) = (
        f.dir.join("projects").join(rel),
        f.common.join("projects").join(rel),
    );
    w(&pe, "fresh");
    ap.move_in("projects", &pe, &ce, rel).unwrap();
    assert!(!pe.exists());
    assert_eq!(fs::read_to_string(&ce).unwrap(), "fresh");
    let ops: Vec<_> = ap.entries.iter().map(|e| e.op.as_str()).collect();
    assert_eq!(ops, ["drop", "memory-conflict", "move"]);
}

#[test]
fn memory_paths() {
    assert!(is_memory_path("projects", Path::new("s/memory/a.md")));
    assert!(is_memory_path("projects", Path::new("s/memory/sub/a.md")));
    assert!(!is_memory_path("projects", Path::new("s/memory")));
    assert!(!is_memory_path("projects", Path::new("s/a.jsonl")));
    assert!(!is_memory_path("skills", Path::new("s/memory/a.md")));
    assert!(is_memory_store("projects", Path::new("s/memory/.git")));
    assert!(is_memory_store(
        "projects",
        Path::new("s/memory/.memory-store")
    ));
    assert!(!is_memory_store("projects", Path::new("s/memory/sub/.git")));
    assert!(!is_memory_store(
        "projects",
        Path::new("s/memory/MEMORY.md")
    ));
}
