//! Command-line parsing (DESIGN §11). Parsed by hand because every unknown
//! argument passes through to `claude` (or `codex`) unchanged.

use std::ffi::OsString;

use anyhow::{Result, bail};
use prism_core::model::{DockMode, Policy, Tool};

/// Launch-mode options plus the args for `claude` (or `codex`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LaunchArgs {
    /// `agp codex ...` runs Codex; else Claude.
    pub tool: Tool,
    /// `--as <id>`: use this profile.
    pub as_profile: Option<String>,
    /// `--policy <p>`: override the balance policy for this pick.
    pub policy: Option<Policy>,
    /// `--dock <mode>`: initial dock mode.
    pub dock: Option<DockMode>,
    /// Everything else, in order, for `claude` (or `codex`).
    pub tool_args: Vec<String>,
}

impl LaunchArgs {
    /// No wrapper: `-p`/`--print` among the claude args before any `--`;
    /// for Codex a non-interactive line such as `codex exec`.
    pub fn print_mode(&self) -> bool {
        match self.tool {
            Tool::Claude => has_print(&self.tool_args),
            Tool::Codex => crate::codex::is_direct(&self.tool_args),
        }
    }
}

/// `-p`/`--print` before the first `--`.
fn has_print(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "-p" || a == "--print")
}

/// A parsed command line.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Launch(LaunchArgs),
    Status {
        json: bool,
        watch: bool,
    },
    Pick {
        json: bool,
        tool: Tool,
    },
    Settings,
    Setup,
    Add {
        id: Option<String>,
        tool: Option<Tool>,
    },
    Doctor,
    Adopt {
        target: String,
        apply: bool,
        no_backup: bool,
        force: bool,
    },
    Exec {
        profile: String,
        cmd: Vec<String>,
    },
    Hook(String),
    Help,
    Version,
}

/// Help text for `agp --help`.
pub const HELP: &str = "\
agent-prism — run claude (or codex) on the account with the most usable quota

Usage:
  agp [--as <id>] [--policy <p>] [--dock <mode>] [claude args...]
  agp codex [--as <id>] [--policy <p>] [--dock <mode>] [codex args...]
                                      the same for Codex (ChatGPT plans)
  agp status [--json] [--watch]     usage of all accounts
  agp pick [--json] [--codex]       the account a new session would use
  agp settings                      settings (full screen)
  agp setup                         first-run setup
  agp add [--codex] [id]            add an account and log in
  agp doctor                        check the setup
  agp adopt <id|dir> [--apply] [--no-backup] [--force]
                                      share data between accounts
  agp exec <id> -- <cmd...>         run a command with that profile's env
  agp hook <Event>                  internal (Claude hooks)

Launch options:
  --as <id>        use this profile
  --policy <p>     smart | priority | round-robin | headroom
  --dock <mode>    auto | full | compact | hidden

Launch options must come first. The first other argument and everything
after it go to claude (or codex) unchanged. A leading `--` ends agent-prism's
options. Lines with -p/--print always go to claude (e.g. `agp status -p`);
codex lines that are not interactive (`agp codex exec ...`) run without
the dock. For claude's own help run `agp -- --help`, for codex's
`agp codex --help`.
";

fn s(a: &OsString) -> Result<String> {
    a.to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("argument is not valid UTF-8: {a:?}"))
}

fn parse_policy(v: &str) -> Result<Policy> {
    Ok(match v {
        "smart" => Policy::Smart,
        "priority" => Policy::Priority,
        "round-robin" => Policy::RoundRobin,
        "headroom" => Policy::Headroom,
        _ => bail!("unknown policy {v:?} (smart, priority, round-robin, headroom)"),
    })
}

/// Parses a dock mode name.
pub fn parse_dock(v: &str) -> Result<DockMode> {
    Ok(match v {
        "auto" => DockMode::Auto,
        "full" => DockMode::Full,
        "compact" => DockMode::Compact,
        "hidden" => DockMode::Hidden,
        _ => bail!("unknown dock mode {v:?} (auto, full, compact, hidden)"),
    })
}

/// Parses agent-prism's arguments (without the program name).
pub fn parse(args: &[OsString]) -> Result<Command> {
    let args: Vec<String> = args.iter().map(s).collect::<Result<_>>()?;
    let rest = || args[1..].to_vec();
    let has = |f: &str| args[1..].iter().any(|a| a == f);
    let Some(first) = args.first() else {
        return Ok(Command::Launch(LaunchArgs::default()));
    };
    const WORDS: &[&str] = &[
        "status",
        "pick",
        "settings",
        "setup",
        "add",
        "doctor",
        "adopt",
        "codex",
        "help",
        "-h",
        "--help",
        "-V",
        "--version",
    ];
    if first != "codex" && WORDS.contains(&first.as_str()) && has_print(&args) {
        return Ok(Command::Launch(LaunchArgs {
            tool_args: args,
            ..Default::default()
        }));
    }
    Ok(match first.as_str() {
        "help" | "--help" | "-h" => Command::Help,
        "--version" | "-V" => Command::Version,
        "status" => Command::Status {
            json: has("--json"),
            watch: has("--watch"),
        },
        "pick" => Command::Pick {
            json: has("--json"),
            tool: if has("--codex") {
                Tool::Codex
            } else {
                Tool::Claude
            },
        },
        "settings" => Command::Settings,
        "setup" => Command::Setup,
        "add" => {
            let mut tool = None;
            let mut id = None;
            for a in rest() {
                match a.as_str() {
                    "--codex" => tool = Some(Tool::Codex),
                    "--claude" => tool = Some(Tool::Claude),
                    _ if id.is_none() && !a.starts_with('-') => id = Some(a),
                    _ => bail!("usage: agp add [--codex | --claude] [id]"),
                }
            }
            Command::Add { id, tool }
        }
        "codex" => Command::Launch(LaunchArgs {
            tool: Tool::Codex,
            ..parse_launch(&args[1..])?
        }),
        "doctor" => Command::Doctor,
        "adopt" => {
            let target = rest()
                .into_iter()
                .find(|a| !a.starts_with("--"))
                .ok_or_else(|| anyhow::anyhow!("usage: agp adopt <id|dir> [--apply]"))?;
            Command::Adopt {
                target,
                apply: has("--apply"),
                no_backup: has("--no-backup"),
                force: has("--force"),
            }
        }
        "exec" => {
            let r = rest();
            let profile = r
                .first()
                .filter(|a| *a != "--")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("usage: agp exec <id> -- <cmd...>"))?;
            let cmd: Vec<String> = match r.iter().position(|a| a == "--") {
                Some(i) => r[i + 1..].to_vec(),
                None => r[1..].to_vec(),
            };
            if cmd.is_empty() {
                bail!("usage: agp exec <id> -- <cmd...>");
            }
            Command::Exec { profile, cmd }
        }
        "hook" => Command::Hook(args.get(1).cloned().unwrap_or_default()),
        _ => Command::Launch(parse_launch(&args)?),
    })
}

/// Our flags only as a leading prefix; the rest goes to claude verbatim.
fn parse_launch(args: &[String]) -> Result<LaunchArgs> {
    let mut out = LaunchArgs::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        let mut value = |name: &str| -> Result<String> {
            match inline.clone() {
                Some(v) => Ok(v),
                None => it
                    .next()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("{name} needs a value")),
            }
        };
        match flag {
            "--" => {
                out.tool_args.extend(it.by_ref().cloned());
                break;
            }
            "--as" => out.as_profile = Some(value("--as")?),
            "--policy" => out.policy = Some(parse_policy(&value("--policy")?)?),
            "--dock" => out.dock = Some(parse_dock(&value("--dock")?)?),
            _ => {
                out.tool_args.push(a.clone());
                out.tool_args.extend(it.by_ref().cloned());
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[&str]) -> Command {
        let a: Vec<OsString> = v.iter().map(OsString::from).collect();
        parse(&a).unwrap()
    }

    fn launch(v: &[&str]) -> LaunchArgs {
        match p(v) {
            Command::Launch(l) => l,
            other => panic!("not launch: {other:?}"),
        }
    }

    #[test]
    fn subcommands() {
        assert_eq!(
            p(&["status", "--json"]),
            Command::Status {
                json: true,
                watch: false
            }
        );
        assert_eq!(
            p(&["pick"]),
            Command::Pick {
                json: false,
                tool: Tool::Claude
            }
        );
        assert_eq!(
            p(&["pick", "--codex", "--json"]),
            Command::Pick {
                json: true,
                tool: Tool::Codex
            }
        );
        assert_eq!(p(&["--help"]), Command::Help);
        assert_eq!(p(&["-V"]), Command::Version);
        assert_eq!(p(&["hook", "Stop"]), Command::Hook("Stop".into()));
        assert_eq!(
            p(&["adopt", "pb", "--apply"]),
            Command::Adopt {
                target: "pb".into(),
                apply: true,
                no_backup: false,
                force: false
            }
        );
        assert_eq!(
            p(&["exec", "pb", "--", "env", "-0"]),
            Command::Exec {
                profile: "pb".into(),
                cmd: vec!["env".into(), "-0".into()]
            }
        );
        assert_eq!(
            p(&["add"]),
            Command::Add {
                id: None,
                tool: None
            }
        );
        assert_eq!(
            p(&["add", "--codex", "work"]),
            Command::Add {
                id: Some("work".into()),
                tool: Some(Tool::Codex)
            }
        );
        let a: Vec<OsString> = ["add", "a", "b"].iter().map(OsString::from).collect();
        assert!(parse(&a).is_err());
    }

    #[test]
    fn codex_launch() {
        let l = launch(&["codex", "--as", "cx", "-m", "gpt-6", "fix it"]);
        assert_eq!(l.tool, Tool::Codex);
        assert_eq!(l.as_profile.as_deref(), Some("cx"));
        assert_eq!(l.tool_args, ["-m", "gpt-6", "fix it"]);
        assert!(!l.print_mode());
        // `-p` is Codex's --profile, not print mode.
        let l = launch(&["codex", "-p", "work"]);
        assert!(!l.print_mode() && l.tool == Tool::Codex);
        assert!(launch(&["codex", "exec", "hi"]).print_mode());
        assert!(launch(&["codex", "--", "--help"]).print_mode());
        assert_eq!(launch(&["codex"]).tool_args, Vec::<String>::new());
        assert_eq!(launch(&["fix it"]).tool, Tool::Claude);
    }

    #[test]
    fn passthrough() {
        // Our flags only count as a leading prefix.
        let l = launch(&["--model", "opus", "--as=pb", "fix it"]);
        assert_eq!(l.as_profile, None);
        assert_eq!(l.tool_args, ["--model", "opus", "--as=pb", "fix it"]);
        let l = launch(&["--as=pb", "-p", "--", "--as", "x"]);
        assert_eq!(l.as_profile.as_deref(), Some("pb"));
        assert_eq!(l.tool_args, ["-p", "--", "--as", "x"]);
        let l = launch(&["--as", "x", "--", "--as"]);
        assert_eq!(l.as_profile.as_deref(), Some("x"));
        assert_eq!(l.tool_args, ["--as"]);
        let l = launch(&["--as", "x", "--policy", "headroom", "--dock=compact", "-c"]);
        assert_eq!(l.as_profile.as_deref(), Some("x"));
        assert_eq!(l.policy, Some(Policy::Headroom));
        assert_eq!(l.dock, Some(DockMode::Compact));
        assert_eq!(l.tool_args, ["-c"]);
        // After `--` everything goes to claude, even agent-prism's flags.
        let l = launch(&["--", "--help", "--as", "y"]);
        assert_eq!(l.as_profile, None);
        assert_eq!(l.tool_args, ["--help", "--as", "y"]);
        assert!(launch(&[]).tool_args.is_empty());
    }

    #[test]
    fn print_mode() {
        assert!(launch(&["-p", "hi"]).print_mode());
        assert!(launch(&["--print", "hi"]).print_mode());
        assert!(!launch(&["hi"]).print_mode());
        assert!(!launch(&["--", "x", "--", "-p"]).print_mode());
        assert!(!launch(&["x", "--", "-p"]).print_mode());
        // Print lines go to claude even when they start with an agent-prism word.
        for w in ["status", "pick", "help", "--help", "-V", "adopt", "add"] {
            let l = launch(&[w, "-p", "q"]);
            assert_eq!(l.tool_args, [w, "-p", "q"]);
            assert_eq!(l.as_profile, None);
        }
        assert_eq!(
            launch(&["status", "--print"]).tool_args,
            ["status", "--print"]
        );
        assert_eq!(
            p(&["status", "--", "-p"]),
            Command::Status {
                json: false,
                watch: false
            }
        );
        assert_eq!(p(&["hook", "-p"]), Command::Hook("-p".into()));
        assert!(matches!(
            p(&["exec", "pb", "--", "x", "-p"]),
            Command::Exec { .. }
        ));
    }

    #[test]
    fn bad_values() {
        let a: Vec<OsString> = ["--policy", "nope"].iter().map(OsString::from).collect();
        assert!(parse(&a).is_err());
        let a: Vec<OsString> = ["--as"].iter().map(OsString::from).collect();
        assert!(parse(&a).is_err());
    }
}
