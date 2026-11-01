//! Resume affinity (DESIGN §6.6): a resumed conversation starts on the
//! account that ran it last, where its prompt cache is, while that account
//! is usable and has room (the balancer checks; `--as` still wins).

use std::path::Path;

use prism_core::journal::{self, Event};
use prism_core::model::Tool;
use prism_core::paths::Paths;

/// What a launch line resumes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resumes {
    /// This session id.
    Session(String),
    /// The last conversation in the work dir (`-c`, `resume --last`).
    LastHere,
}

/// A conversation a launch line goes on with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resume {
    /// Its session id, when known (not for a fork, which gets a new one).
    pub session: Option<String>,
    /// The account that ran it last, when agp knows.
    pub account: Option<String>,
}

/// The conversation `args` resume, if agp knows it: Claude `--resume <id>`
/// / `-r <id>` / `-c`, Codex `resume <id>` / `exec resume <id>` /
/// `--last`. A `--continue` or `--last` is the last conversation agp ran in
/// `cwd`.
pub fn resume(paths: &Paths, tool: Tool, args: &[String], cwd: &Path) -> Option<Resume> {
    let target = match tool {
        Tool::Claude => claude_resumes(args)?,
        Tool::Codex => match crate::codex::resumes(args)? {
            Some(id) => Resumes::Session(id),
            None => Resumes::LastHere,
        },
    };
    let fork = tool == Tool::Claude
        && crate::launch::options(args)
            .iter()
            .any(|a| a == "--fork-session");
    let events = journal::read_since(paths, 0);
    let id = match target {
        Resumes::Session(id) => id,
        Resumes::LastHere => last_session_in(&events, tool, cwd)?,
    };
    let account = crate::bg::session_profile(paths, &id).or_else(|| account_of(&events, &id));
    Some(Resume {
        session: (!fork).then_some(id),
        account,
    })
}

/// The account that last ran the conversation `args` resume, if known.
pub fn last_account(paths: &Paths, tool: Tool, args: &[String], cwd: &Path) -> Option<String> {
    resume(paths, tool, args, cwd)?.account
}

/// What the Claude args resume. A `--resume` value that is not a session
/// id (a search term, or none: the picker) is unknown.
fn claude_resumes(args: &[String]) -> Option<Resumes> {
    let opts = crate::launch::options(args);
    let mut it = opts.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-c" | "--continue" => return Some(Resumes::LastHere),
            "-r" | "--resume" => {
                let v = it.peek().filter(|v| !v.starts_with('-'))?;
                return uuid::Uuid::parse_str(v)
                    .is_ok()
                    .then(|| Resumes::Session(v.to_string()));
            }
            _ => {
                if let Some(v) = a.strip_prefix("--resume=") {
                    return uuid::Uuid::parse_str(v)
                        .is_ok()
                        .then(|| Resumes::Session(v.to_string()));
                }
            }
        }
    }
    None
}

/// The session of the last run of `tool` that started in `cwd` with a
/// known session.
fn last_session_in(events: &[Event], tool: Tool, cwd: &Path) -> Option<String> {
    let cwd = cwd.to_string_lossy();
    events
        .iter()
        .rev()
        .filter(|e| e.tool == tool && e.cwd.as_deref() == Some(&*cwd))
        .find_map(|start| {
            events
                .iter()
                .rev()
                .filter(|e| e.run == start.run)
                .find_map(|e| e.session.clone())
        })
}

/// The account of the last journal event about session `id`.
fn account_of(events: &[Event], id: &str) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|e| e.session.as_deref() == Some(id) && !e.profile.is_empty())
        .map(|e| e.profile.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_core::journal::{EventKind, Recorder, RunMode};

    const SID: &str = "0b9c6a52-4c1e-4f0e-9d41-6f7b8d1f2a33";

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn claude_flags() {
        assert_eq!(
            claude_resumes(&v(&["--resume", SID, "go on"])),
            Some(Resumes::Session(SID.into()))
        );
        assert_eq!(
            claude_resumes(&v(&[&format!("--resume={SID}")])),
            Some(Resumes::Session(SID.into()))
        );
        assert_eq!(
            claude_resumes(&v(&["-c", "-p", "x"])),
            Some(Resumes::LastHere)
        );
        // A search term, the picker, or no resume at all.
        assert_eq!(claude_resumes(&v(&["-r", "auth bug"])), None);
        assert_eq!(claude_resumes(&v(&["--resume"])), None);
        assert_eq!(claude_resumes(&v(&["-p", "--", "-c"])), None);
    }

    #[test]
    fn from_the_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let rec = Recorder::new(paths.clone(), Tool::Claude, RunMode::Exec, 100);
        let mut ev = rec.event(EventKind::Start, "a", 100);
        ev.session = Some(SID.into());
        ev.cwd = Some("/work".into());
        rec.write(ev);
        // The run moved to b.
        let mut ev = rec.event(EventKind::Switch, "b", 200);
        ev.from = Some("a".into());
        ev.session = Some(SID.into());
        rec.write(ev);
        // A later run elsewhere.
        let other = Recorder::new(paths.clone(), Tool::Claude, RunMode::Exec, 300);
        let mut ev = other.event(EventKind::Start, "c", 300);
        ev.session = Some("other".into());
        ev.cwd = Some("/elsewhere".into());
        other.write(ev);

        // A later run in /work whose session is not known.
        let unknown = Recorder::new(paths.clone(), Tool::Claude, RunMode::Tui, 400);
        let mut ev = unknown.event(EventKind::Start, "a", 400);
        ev.cwd = Some("/work".into());
        unknown.write(ev);

        let cwd = Path::new("/work");
        let get = |args: &[&str], dir: &Path| last_account(&paths, Tool::Claude, &v(args), dir);
        let r = resume(&paths, Tool::Claude, &v(&["-c"]), cwd).unwrap();
        assert_eq!(r.session.as_deref(), Some(SID));
        let fork = resume(
            &paths,
            Tool::Claude,
            &v(&["-r", SID, "--fork-session"]),
            cwd,
        )
        .unwrap();
        assert_eq!((fork.session, fork.account.as_deref()), (None, Some("b")));
        assert_eq!(get(&["--resume", SID], cwd).as_deref(), Some("b"));
        assert_eq!(get(&["-c"], cwd).as_deref(), Some("b"));
        assert_eq!(get(&["-c"], Path::new("/elsewhere")).as_deref(), Some("c"));
        assert_eq!(get(&["-c"], Path::new("/new")), None);
        assert_eq!(get(&["fix it"], cwd), None);
        // Codex lines of another tool find nothing here.
        assert_eq!(
            last_account(&paths, Tool::Codex, &v(&["exec", "resume", "--last"]), cwd),
            None
        );
    }
}
