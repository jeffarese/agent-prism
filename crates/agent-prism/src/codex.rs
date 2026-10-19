//! Codex specifics: its command line, and the thread id of a running
//! session from Codex's own database (Codex has no `--session-id` and its
//! hooks need a trust approval, so agent-prism reads the id instead).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Codex options that take a value (top level and `resume`).
const VALUE_FLAGS: [&str; 21] = [
    "-c",
    "--config",
    "--enable",
    "--disable",
    "--remote",
    "--remote-auth-token-env",
    "-i",
    "--image",
    "-m",
    "--model",
    "--local-provider",
    "-p",
    "--profile",
    "-s",
    "--sandbox",
    "-C",
    "--cd",
    "--add-dir",
    "-a",
    "--ask-for-approval",
    "--color",
];

/// Subcommands that keep the interactive TUI (agent-prism wraps them).
const INTERACTIVE: [&str; 2] = ["resume", "fork"];

/// Options that only make sense for the picker of `resume` / `fork`.
const PICKER_FLAGS: [&str; 3] = ["--last", "--all", "--include-non-interactive"];

fn flag_name(a: &str) -> &str {
    a.split_once('=').map_or(a, |(f, _)| f)
}

fn takes_value(a: &str) -> bool {
    !a.contains('=') && VALUE_FLAGS.contains(&a)
}

/// The positional args (index, value) before any `--`, skipping option
/// values.
fn positionals(args: &[String]) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            break;
        }
        if a.starts_with('-') && a != "-" {
            i += if takes_value(a) { 2 } else { 1 };
            continue;
        }
        out.push((i, a));
        i += 1;
    }
    out
}

/// The subcommand, if the first positional is one Codex knows. A prompt is
/// any other first positional.
pub fn subcommand(args: &[String]) -> Option<&str> {
    const KNOWN: [&str; 30] = [
        "exec",
        "e",
        "review",
        "login",
        "logout",
        "mcp",
        "mcp-server",
        "plugin",
        "app-server",
        "remote-control",
        "app",
        "completion",
        "update",
        "doctor",
        "sandbox",
        "debug",
        "apply",
        "a",
        "resume",
        "queue",
        "archive",
        "delete",
        "migrate-rollouts",
        "unarchive",
        "fork",
        "cloud",
        "exec-server",
        "features",
        "help",
        "agents",
    ];
    positionals(args)
        .first()
        .map(|(_, a)| *a)
        .filter(|a| KNOWN.contains(a))
}

/// True when the line does not start the interactive TUI (`codex exec`,
/// `codex login`, `--help`, ...): agent-prism runs it directly, without the dock.
pub fn is_direct(args: &[String]) -> bool {
    let opts = crate::launch::options(args);
    if opts
        .iter()
        .any(|a| matches!(a.as_str(), "-h" | "--help" | "-V" | "--version"))
    {
        return true;
    }
    subcommand(args).is_some_and(|s| !INTERACTIVE.contains(&s))
}

/// The session id of `codex resume <id>` / `codex fork <id>` (a UUID or a
/// name), if the user gave one.
pub fn resume_target(args: &[String]) -> Option<String> {
    let pos = positionals(args);
    match pos.as_slice() {
        [(_, "resume"), (_, id), ..] => Some((*id).to_string()),
        _ => None,
    }
}

/// `user` with `extra` placed so Codex reads it: after a leading `resume`
/// or `fork` subcommand, else first.
pub fn insert_flags(user: &[String], extra: Vec<String>) -> Vec<String> {
    let at = match positionals(user).first() {
        Some((i, s)) if INTERACTIVE.contains(s) => i + 1,
        _ => 0,
    };
    let mut out = user[..at].to_vec();
    out.extend(extra);
    out.extend(user[at..].iter().cloned());
    out
}

/// The user's options for a resume on another account: no subcommand, no
/// session id, no prompt, no picker flags, nothing after `--`.
pub fn strip_for_switch(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            break;
        }
        if a.starts_with('-') && a != "-" {
            let n = if takes_value(a) { 2 } else { 1 };
            if !PICKER_FLAGS.contains(&flag_name(a)) {
                out.extend(args[i..(i + n).min(args.len())].iter().cloned());
            }
            i += n;
            continue;
        }
        i += 1;
    }
    out
}

/// True if the user connects to a remote app server (`--remote`): then
/// agp adds no `--no-daemon`.
pub fn uses_remote(args: &[String]) -> bool {
    crate::launch::options(args)
        .iter()
        .any(|a| flag_name(a) == "--remote")
}

/// The working dir Codex uses: `-C/--cd <dir>` (relative to `cwd`), else
/// `cwd`.
pub fn work_dir(args: &[String], cwd: &Path) -> PathBuf {
    let opts = crate::launch::options(args);
    let mut it = opts.iter();
    while let Some(a) = it.next() {
        let v = match a.as_str() {
            "-C" | "--cd" => it.next().cloned(),
            _ => a.strip_prefix("--cd=").map(str::to_string),
        };
        if let Some(v) = v {
            let p = cwd.join(v);
            return p.canonicalize().unwrap_or(p);
        }
    }
    cwd.to_path_buf()
}

/// The newest `state_<n>.sqlite` in `sqlite_home`.
pub fn state_db(sqlite_home: &Path) -> Option<PathBuf> {
    let rd = std::fs::read_dir(sqlite_home).ok()?;
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n: u32 = name
                .strip_prefix("state_")?
                .strip_suffix(".sqlite")?
                .parse()
                .ok()?;
            Some((n, e.path()))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, p)| p)
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A thread id as Codex writes it: hex digits and dashes.
fn looks_like_id(s: &str) -> bool {
    s.len() >= 32 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// The SQL for [`find_thread`]; `full` uses the newer columns. `col` is
/// `created_at` or `updated_at` (unix seconds).
fn thread_query(cwd: &str, since_ms: i64, account: Option<&str>, full: bool, col: &str) -> String {
    let mut q = format!(
        "SELECT id FROM threads WHERE cwd = {} AND {col} >= {} AND archived = 0",
        sql_str(cwd),
        since_ms / 1000
    );
    if full {
        q.push_str(" AND (thread_source IS NULL OR thread_source = 'user')");
        if let Some(a) = account {
            q.push_str(&format!(
                " AND (creator_account_id IS NULL OR creator_account_id = {})",
                sql_str(a)
            ));
        }
    }
    q.push_str(&format!(" ORDER BY {col} DESC, id DESC LIMIT 1;"));
    q
}

/// The newest user thread in `cwd` created at or after `since_ms` (by
/// `account` when the database records it). Read-only; `None` on any
/// error.
pub fn find_thread(db: &Path, cwd: &Path, since_ms: i64, account: Option<&str>) -> Option<String> {
    query_thread(db, cwd, since_ms, account, "created_at")
}

/// The user thread in `cwd` updated most recently at or after `since_ms`:
/// the thread that a `resume` without an id (picker, `--last`, a name)
/// opened.
pub fn find_updated_thread(db: &Path, cwd: &Path, since_ms: i64) -> Option<String> {
    query_thread(db, cwd, since_ms, None, "updated_at")
}

fn query_thread(
    db: &Path,
    cwd: &Path,
    since_ms: i64,
    account: Option<&str>,
    col: &str,
) -> Option<String> {
    let cwd = cwd.to_str()?;
    for full in [true, false] {
        let out = Command::new("/usr/bin/sqlite3")
            .arg("-readonly")
            .arg("-noheader")
            .args(["-cmd", ".timeout 2000"])
            .arg(db)
            .arg(thread_query(cwd, since_ms, account, full, col))
            .output()
            .ok()?;
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            return text
                .lines()
                .map(str::trim)
                .find(|l| looks_like_id(l))
                .map(str::to_string);
        }
        log::debug!(
            "codex thread query failed (full={full}): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn direct_and_subcommands() {
        assert!(!is_direct(&v(&[])));
        assert!(!is_direct(&v(&["fix the bug"])));
        // `-m` takes `gpt-6`, so `exec` is the subcommand.
        assert!(is_direct(&v(&["-m", "gpt-6", "exec", "hi"])));
        assert!(is_direct(&v(&["exec", "hi"])));
        assert!(is_direct(&v(&["-c", "x=1", "login"])));
        assert!(is_direct(&v(&["--help"])));
        assert!(!is_direct(&v(&["resume", "abc"])));
        assert!(!is_direct(&v(&["--yolo", "fork", "--last"])));
        assert!(!is_direct(&v(&["--", "--help"])));
        assert_eq!(subcommand(&v(&["-p", "work", "review"])), Some("review"));
        assert_eq!(subcommand(&v(&["hello"])), None);
    }

    #[test]
    fn resume_and_insert() {
        assert_eq!(
            resume_target(&v(&["resume", "01ab", "go"])).as_deref(),
            Some("01ab")
        );
        assert_eq!(resume_target(&v(&["resume", "--last"])), None);
        assert_eq!(resume_target(&v(&["fork", "x"])), None);
        let extra = v(&["--no-daemon"]);
        assert_eq!(
            insert_flags(&v(&["-m", "o", "resume", "id"]), extra.clone()),
            v(&["-m", "o", "resume", "--no-daemon", "id"])
        );
        assert_eq!(
            insert_flags(&v(&["fix it"]), extra.clone()),
            v(&["--no-daemon", "fix it"])
        );
    }

    #[test]
    fn strip() {
        assert_eq!(
            strip_for_switch(&v(&[
                "-m", "o", "resume", "--last", "id", "prompt", "--", "x"
            ])),
            v(&["-m", "o"])
        );
        assert_eq!(
            strip_for_switch(&v(&["--yolo", "-c", "a=1", "--search", "do it"])),
            v(&["--yolo", "-c", "a=1", "--search"])
        );
        assert!(uses_remote(&v(&["--remote=ws://x"])));
        assert!(!uses_remote(&v(&["--", "--remote"])));
    }

    #[test]
    fn work_dir_and_query() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("p");
        std::fs::create_dir_all(&sub).unwrap();
        let cwd = tmp.path().canonicalize().unwrap();
        assert_eq!(work_dir(&v(&["-C", "p"]), &cwd), cwd.join("p"));
        assert_eq!(work_dir(&v(&["hi"]), &cwd), cwd);
        let q = thread_query("/a'b", 5_000, Some("acc"), true, "created_at");
        assert!(
            q.contains("cwd = '/a''b'") && q.contains("created_at >= 5"),
            "{q}"
        );
        assert!(q.contains("creator_account_id = 'acc'"));
        assert!(!thread_query("/a", 0, Some("acc"), false, "created_at").contains("creator"));
        assert!(looks_like_id("01a14e86-cd99-7222-a279-4ad443c24843"));
        assert!(!looks_like_id("Error: x"));
    }

    #[test]
    fn finds_thread_in_a_database() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(state_db(tmp.path()).is_none());
        let db = tmp.path().join("state_5.sqlite");
        std::fs::write(tmp.path().join("state_4.sqlite"), "").unwrap();
        let sql = "CREATE TABLE threads (id TEXT, cwd TEXT, created_at INTEGER, updated_at INTEGER, archived INTEGER, thread_source TEXT, creator_account_id TEXT);
            INSERT INTO threads VALUES ('01a00000-0000-7000-8000-000000000001', '/w', 100, 900, 0, 'user', 'acc');
            INSERT INTO threads VALUES ('01a00000-0000-7000-8000-000000000002', '/w', 200, 200, 0, 'user', 'acc');
            INSERT INTO threads VALUES ('01a00000-0000-7000-8000-000000000003', '/w', 300, 300, 0, 'guardian_review', 'acc');
            INSERT INTO threads VALUES ('01a00000-0000-7000-8000-000000000004', '/w', 400, 400, 0, 'user', 'other');";
        let ok = Command::new("/usr/bin/sqlite3").arg(&db).arg(sql).status();
        if !ok.is_ok_and(|s| s.success()) {
            eprintln!("no sqlite3; skipped");
            return;
        }
        assert_eq!(state_db(tmp.path()), Some(db.clone()));
        let w = Path::new("/w");
        assert_eq!(
            find_thread(&db, w, 150_000, Some("acc")).as_deref(),
            Some("01a00000-0000-7000-8000-000000000002")
        );
        assert_eq!(find_thread(&db, w, 500_000, Some("acc")), None);
        assert_eq!(find_thread(&db, Path::new("/x"), 0, None), None);
        assert_eq!(
            find_updated_thread(&db, w, 800_000).as_deref(),
            Some("01a00000-0000-7000-8000-000000000001")
        );
    }
}
