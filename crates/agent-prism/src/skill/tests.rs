use std::os::unix::fs::symlink;

use super::*;

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

/// Installs into `skills` with a fresh run list.
fn inst(skills: &Path) -> Outcome {
    install(skills, &mut Vec::new()).unwrap()
}

/// The frontmatter's `key: value` lines.
fn frontmatter() -> Vec<(String, String)> {
    let body = SKILL.strip_prefix("---\n").expect("frontmatter");
    let end = body.find("\n---\n").expect("frontmatter end");
    body[..end]
        .lines()
        .map(|l| {
            let (k, v) = l.split_once(": ").expect("key: value");
            (k.to_string(), v.to_string())
        })
        .collect()
}

#[test]
fn frontmatter_and_install() {
    assert!(SKILL.starts_with("---\nname: agp\ndescription: "));
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    assert_eq!(inst(&skills), Outcome::Installed);
    let agp = skills.join("agp");
    for (rel, text) in FILES {
        assert_eq!(read(&agp.join(rel)), *text, "{rel}");
    }
    assert!(agp.join(MANIFEST).is_file());
    assert_eq!(
        check(&agp),
        Found::Ours {
            files: FILES.iter().map(|(p, _)| p.to_string()).collect(),
            current: true
        }
    );
    assert_eq!(inst(&skills), Outcome::UpToDate);
    std::fs::write(agp.join("SKILL.md"), "old").unwrap();
    assert!(matches!(check(&agp), Found::Ours { current: false, .. }));
    assert_eq!(inst(&skills), Outcome::Updated);
    assert_eq!(read(&agp.join("SKILL.md")), SKILL);
    // A missing reference file is put back.
    std::fs::remove_file(agp.join("reference/lanes.md")).unwrap();
    assert_eq!(inst(&skills), Outcome::Updated);
    assert_eq!(inst(&skills), Outcome::UpToDate);
    // No temp files are left behind.
    let names: Vec<_> = std::fs::read_dir(&agp)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().all(|n| !n.ends_with(".agp-tmp")), "{names:?}");
}

#[test]
fn frontmatter_follows_the_agent_skills_rules() {
    let fm = frontmatter();
    let keys: Vec<&str> = fm.iter().map(|(k, _)| k.as_str()).collect();
    // Only the keys every agent accepts (Codex rejects unknown ones).
    assert_eq!(keys, ["name", "description"]);
    let name = &fm[0].1;
    assert_eq!(name, NAME, "the name matches the folder");
    assert!(name.len() <= 64);
    assert!(
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    );
    let desc = &fm[1].1;
    assert!(!desc.is_empty() && desc.len() <= 1024, "{}", desc.len());
    assert!(!desc.contains('<') && !desc.contains('>'));
    // A valid YAML plain scalar: no `: ` or ` #`, no quote or indicator first.
    assert!(!desc.contains(": ") && !desc.contains(" #"));
    assert!(!desc.starts_with(['"', '\'', '`', '[', '{', '*', '&', '!', '|', '>', '%', '@']));
    for phrase in [
        "claude -p",
        "claude --bg",
        "codex exec",
        "parallel",
        "quota",
        "expired",
    ] {
        assert!(desc.contains(phrase), "{phrase}");
    }
    assert!(SKILL.lines().count() < 500);
}

#[test]
fn embedded_files_match_the_folder_and_links() {
    // Every file of skills/agp is built in, and nothing else.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("skills")
        .join(NAME);
    fn walk(root: &Path, d: &Path, out: &mut BTreeSet<String>) {
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut on_disk = BTreeSet::new();
    walk(&root, &root, &mut on_disk);
    let built: BTreeSet<String> = FILES.iter().map(|(p, _)| p.to_string()).collect();
    assert_eq!(on_disk, built);
    for (rel, text) in FILES {
        assert_eq!(read(&root.join(rel)), *text, "{rel}");
    }
    // SKILL.md links every reference file by its relative path, and every
    // relative link in the skill points at a file of the skill.
    for (rel, _) in &FILES[1..] {
        assert!(SKILL.contains(&format!("]({rel})")), "SKILL.md links {rel}");
    }
    for (rel, text) in FILES {
        let base = Path::new(rel).parent().unwrap();
        for part in text.split("](").skip(1) {
            let target = part.split(')').next().unwrap();
            let target = target.split('#').next().unwrap();
            if target.contains("://") || target.is_empty() {
                continue;
            }
            let joined = base.join(target);
            assert!(
                built.contains(joined.to_str().unwrap()),
                "{rel} links {target}"
            );
        }
    }
}

#[test]
fn uninstall_removes_only_agps_files() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(skills.join("other")).unwrap();
    std::fs::write(skills.join("other/SKILL.md"), "x").unwrap();
    inst(&skills);
    let agp = skills.join("agp");
    assert_eq!(uninstall(&agp).unwrap(), Outcome::Removed);
    assert!(agp.symlink_metadata().is_err());
    assert!(skills.join("other/SKILL.md").is_file(), "other skills stay");
    assert_eq!(uninstall(&agp).unwrap(), Outcome::NotInstalled);

    // A file the user added stays, with the folder.
    inst(&skills);
    std::fs::write(agp.join("reference/mine.md"), "notes").unwrap();
    assert_eq!(uninstall(&agp).unwrap(), Outcome::RemovedSome(1));
    assert_eq!(read(&agp.join("reference/mine.md")), "notes");
    assert!(!agp.join("SKILL.md").exists());
    assert!(!agp.join(MANIFEST).exists());
}

#[test]
fn layout_change_prunes_old_files() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    let agp = skills.join("agp");

    // The old one-file layout (agp before the folder) is agp's.
    std::fs::create_dir_all(&agp).unwrap();
    let old = "---\nname: agp\ndescription: Use agp (agent-prism) ...\n---\nold\n";
    std::fs::write(agp.join("SKILL.md"), old).unwrap();
    assert!(matches!(check(&agp), Found::Ours { current: false, .. }));
    assert_eq!(inst(&skills), Outcome::Updated);
    assert_eq!(read(&agp.join("SKILL.md")), SKILL);
    assert_eq!(inst(&skills), Outcome::UpToDate);

    // A manifest from a layout with files that are gone now: those go;
    // a file the manifest doesn't list stays, and so does anything outside.
    std::fs::create_dir_all(agp.join("old")).unwrap();
    std::fs::write(agp.join("old/gone.md"), "x").unwrap();
    std::fs::write(agp.join("keep.md"), "mine").unwrap();
    std::fs::write(dir.path().join("outside.md"), "x").unwrap();
    let m = serde_json::json!({
        "skill": "agp",
        "files": ["SKILL.md", "old/gone.md", "../outside.md", "/etc/hosts"],
    });
    std::fs::write(agp.join(MANIFEST), m.to_string()).unwrap();
    assert!(matches!(check(&agp), Found::Ours { current: false, .. }));
    assert_eq!(inst(&skills), Outcome::Updated);
    assert!(
        !agp.join("old").exists(),
        "the old file and its empty folder go"
    );
    assert_eq!(read(&agp.join("keep.md")), "mine");
    assert!(dir.path().join("outside.md").is_file());
    assert_eq!(read(&agp.join(MANIFEST)), manifest());
    assert_eq!(inst(&skills), Outcome::UpToDate);
}

#[test]
fn a_folder_agp_did_not_install_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    let agp = skills.join("agp");
    std::fs::create_dir_all(&agp).unwrap();
    std::fs::write(
        agp.join("SKILL.md"),
        "---\nname: agp\ndescription: theirs\n---\n",
    )
    .unwrap();
    assert!(matches!(check(&agp), Found::Foreign(_)));
    assert!(matches!(inst(&skills), Outcome::Skipped(_)));
    assert!(matches!(uninstall(&agp).unwrap(), Outcome::Skipped(_)));
    assert!(read(&agp.join("SKILL.md")).contains("theirs"));
    // An empty folder is free to use.
    let skills2 = dir.path().join("s2");
    std::fs::create_dir_all(skills2.join("agp")).unwrap();
    assert_eq!(inst(&skills2), Outcome::Installed);
}

#[test]
fn symlinks() {
    let dir = tempfile::tempdir().unwrap();
    // `<skills>/agp` links to a folder agp does not manage: skipped.
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("SKILL.md"), "theirs").unwrap();
    let a = dir.path().join("a");
    std::fs::create_dir_all(&a).unwrap();
    symlink(&elsewhere, a.join("agp")).unwrap();
    assert!(matches!(
        check(&a.join("agp")),
        Found::Link { ours: false, .. }
    ));
    assert!(matches!(inst(&a), Outcome::Skipped(_)));
    assert!(matches!(
        uninstall(&a.join("agp")).unwrap(),
        Outcome::Skipped(_)
    ));
    assert_eq!(read(&elsewhere.join("SKILL.md")), "theirs");
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 1);

    // A dangling link: skipped too.
    let d = dir.path().join("d");
    std::fs::create_dir_all(&d).unwrap();
    symlink(dir.path().join("nowhere"), d.join("agp")).unwrap();
    assert!(matches!(inst(&d), Outcome::Skipped(_)));

    // A link to a folder agp manages: updated through the link once per run.
    let b = dir.path().join("b");
    let c = dir.path().join("c");
    inst(&b);
    std::fs::create_dir_all(&c).unwrap();
    symlink(b.join("agp"), c.join("agp")).unwrap();
    assert!(matches!(
        check(&c.join("agp")),
        Found::Link {
            ours: true,
            current: true,
            ..
        }
    ));
    std::fs::write(b.join("agp/SKILL.md"), "old").unwrap();
    let mut done = Vec::new();
    assert_eq!(install(&c, &mut done).unwrap(), Outcome::Updated);
    assert_eq!(read(&b.join("agp/SKILL.md")), SKILL);
    assert_eq!(
        install(&b, &mut done).unwrap(),
        Outcome::Same(c.join("agp"))
    );
    // Uninstall leaves a link agp did not make.
    assert!(matches!(
        uninstall(&c.join("agp")).unwrap(),
        Outcome::Skipped(_)
    ));
    assert!(
        c.join("agp")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );

    // A symlinked folder inside agp's folder is never followed.
    let e = dir.path().join("e");
    inst(&e);
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::remove_dir_all(e.join("agp/reference")).unwrap();
    symlink(&out, e.join("agp/reference")).unwrap();
    assert!(install(&e, &mut Vec::new()).is_err());
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
    // A symlinked file is replaced, not written through.
    let f = dir.path().join("f");
    inst(&f);
    let target = dir.path().join("target.md");
    std::fs::write(&target, "keep").unwrap();
    std::fs::remove_file(f.join("agp/SKILL.md")).unwrap();
    symlink(&target, f.join("agp/SKILL.md")).unwrap();
    assert_eq!(inst(&f), Outcome::Updated);
    assert_eq!(read(&target), "keep");
    assert!(f.join("agp/SKILL.md").symlink_metadata().unwrap().is_file());
}

#[test]
fn targets_and_selection() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let config = Config::default();
    std::fs::create_dir_all(paths.home.join(".claude")).unwrap();
    std::fs::create_dir_all(paths.home.join(".gemini")).unwrap();
    std::fs::create_dir_all(paths.home.join(".agents")).unwrap();
    std::fs::create_dir_all(paths.home.join(".config/amp")).unwrap();
    let known = known_targets(&paths, &config);
    assert_eq!(known.len(), AGENTS.len());
    let names = |ts: &[Target]| ts.iter().map(|t| t.agent.clone()).collect::<Vec<_>>();

    // Detected automatic agents only: no Codex home, `agents` only on
    // request, Amp reads the Claude copy.
    let auto = select(&known, &SkillTargets::default(), false).unwrap();
    assert_eq!(names(&auto), ["claude", "gemini"]);
    assert_eq!(auto[0].skills_dir, paths.home.join(".claude/skills"));
    assert_eq!(auto[1].dir(), paths.home.join(".gemini/skills/agp"));

    let sel = SkillTargets {
        agents: vec!["amp".into(), "agents".into(), "codex".into()],
        dirs: vec!["/x/skills".into()],
    };
    let t = select(&known, &sel, false).unwrap();
    assert_eq!(names(&t), ["amp", "agents", "codex", "dir"]);
    assert_eq!(t[0].skills_dir, paths.home.join(".claude/skills"));
    assert_eq!(t[0].via, Some("claude"));
    assert_eq!(t[1].skills_dir, paths.home.join(".agents/skills"));
    assert_eq!(t[2].skills_dir, paths.home.join(".codex/skills"));
    assert_eq!(t[3].dir(), PathBuf::from("/x/skills/agp"));

    let bad = SkillTargets {
        agents: vec!["nope".into()],
        dirs: vec![],
    };
    assert!(select(&known, &bad, false).is_err());

    // Uninstall without names: every agent's own folder, none twice.
    let all = select(&known, &SkillTargets::default(), true).unwrap();
    assert_eq!(
        names(&all),
        ["claude", "codex", "gemini", "copilot", "droid", "agents"]
    );

    // Installing for Claude and Amp writes the one folder once.
    let mut done = Vec::new();
    assert_eq!(
        install(&auto[0].skills_dir, &mut done).unwrap(),
        Outcome::Installed
    );
    assert!(matches!(
        install(&t[0].skills_dir, &mut done).unwrap(),
        Outcome::Same(_)
    ));
    assert!(list_state(&known[6]).starts_with("reads the claude copy: current"));
}
