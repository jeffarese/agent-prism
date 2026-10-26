//! Command-line parsing (DESIGN §11). Parsed by hand because every unknown
//! argument passes through to `claude` (or `codex`) unchanged.

use std::ffi::OsString;

use anyhow::{Result, bail};
use prism_core::config::Config;
use prism_core::model::{DockMode, OnLimit, Policy, Tool};

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
    /// Per-run limit and account options.
    pub run: RunOptions,
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

/// What a run does when its account hits a limit (`--on-limit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitAction {
    /// Interactive behavior from the config's `on_limit` values.
    Mode(OnLimit),
    /// Print mode only: give up with exit code 75 (`EX_TEMPFAIL`).
    Fail,
}

/// Per-run options: they change this run only, never the config.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunOptions {
    /// `--on-limit <switch|ask|stay|wait|fail>`.
    pub on_limit: Option<LimitAction>,
    /// `--max-wait <dur>`: the longest wait for a reset, in seconds.
    pub max_wait: Option<i64>,
    /// `--accounts <a,b>`: the pool for this run (in or out of the pool).
    pub accounts: Option<Vec<String>>,
    /// `--exclude <a,b>`: never use these accounts in this run.
    pub exclude: Vec<String>,
    /// `--max-usage <pct>`: an account at or over this 5h or weekly percent
    /// counts as limited in this run.
    pub max_usage: Option<f64>,
}

/// Exit code when a run gives up on a limit (`EX_TEMPFAIL`).
pub const EXIT_LIMITED: i32 = 75;

impl RunOptions {
    /// The config's `on_limit` for this run.
    pub fn on_limit(&self) -> Option<OnLimit> {
        match self.on_limit? {
            LimitAction::Mode(m) => Some(m),
            LimitAction::Fail => None,
        }
    }

    /// Applies the options to `config` for `tool`'s accounts: the pool, the
    /// limit threshold and `on_limit`. Unknown account ids are an error.
    pub fn apply(&self, config: &mut Config, tool: Tool) -> Result<()> {
        let known = |id: &String| {
            config
                .profiles
                .iter()
                .any(|p| p.tool == tool && &p.id == id)
        };
        for id in self.accounts.iter().flatten().chain(&self.exclude) {
            if !known(id) {
                bail!("no {} account {id:?} in the config", tool.name());
            }
        }
        for p in config.profiles.iter_mut().filter(|p| p.tool == tool) {
            if let Some(only) = &self.accounts {
                p.pool = only.contains(&p.id);
            }
            if self.exclude.contains(&p.id) {
                p.pool = false;
            }
        }
        if let Some(pct) = self.max_usage {
            let b = &mut config.balance;
            b.hard = b.hard.min(pct);
            b.soft_5h = b.soft_5h.min(pct);
        }
        if let Some(m) = self.on_limit() {
            config.balance.on_limit = m;
        }
        Ok(())
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
    /// `agp dashboard [--port <n>] [--no-open]`: the local web dashboard.
    Dashboard {
        port: Option<u16>,
        open: bool,
    },
    /// `agp skill [install|show]`: the agp skill for Claude and Codex.
    Skill {
        install: bool,
    },
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
  agp dashboard [--port <n>] [--no-open]
                                      usage dashboard in the browser
  agp skill [install]               show or install the agp skill for agents
  agp setup                         first-run setup
  agp add [--codex] [id]            add an account and log in
  agp doctor                        check the setup
  agp adopt <id|dir> [--apply] [--no-backup] [--force]
                                      share data between accounts
  agp exec <id> -- <cmd...>         run a command with that profile's env
  agp hook <Event>                  internal (Claude hooks)

Launch options:
  --as <id>          use this profile
  --policy <p>       smart | priority | round-robin | headroom
  --dock <mode>      auto | full | compact | hidden

Per-run options (this run only; the config does not change):
  --on-limit <a>     what to do when the account hits a limit:
                       switch  move to another account (config: auto)
                       ask     ask first (interactive)
                       stay    stay on the account, only tell
                       wait    stay, wait for the reset, then go on
                       fail    -p / codex exec: exit 75 instead of waiting
                     With -p or codex exec, agent-prism then stays the parent
                     process and resumes the conversation after a limit.
  --max-wait <dur>   wait at most this long for a reset (e.g. 90m, 2h, 1d);
                     longer: -p / codex exec exit 75, a session stays
  --accounts <ids>   use only these accounts (comma-separated)
  --exclude <ids>    never use these accounts
  --max-usage <pct>  treat an account at or over this 5h or weekly % as limited

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

/// Parses `--on-limit` values.
pub fn parse_limit_action(v: &str) -> Result<LimitAction> {
    Ok(match v {
        "switch" | "auto" => LimitAction::Mode(OnLimit::Auto),
        "ask" => LimitAction::Mode(OnLimit::Ask),
        "stay" => LimitAction::Mode(OnLimit::Stay),
        "wait" => LimitAction::Mode(OnLimit::Wait),
        "fail" => LimitAction::Fail,
        _ => bail!("unknown --on-limit {v:?} (switch, ask, stay, wait, fail)"),
    })
}

/// Parses a duration: `90`, `90s`, `45m`, `2h`, `1d`, `1h30m`.
pub fn parse_duration(v: &str) -> Result<i64> {
    let bad = || anyhow::anyhow!("bad duration {v:?} (e.g. 90m, 2h, 1d)");
    if let Ok(n) = v.parse::<i64>() {
        return (n >= 0).then_some(n).ok_or_else(bad);
    }
    let (mut total, mut num) = (0i64, String::new());
    for c in v.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return Err(bad()),
        };
        let n: i64 = num.parse().map_err(|_| bad())?;
        total += n * unit;
        num.clear();
    }
    if !num.is_empty() || v.is_empty() {
        return Err(bad());
    }
    Ok(total)
}

fn parse_ids(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_pct(v: &str) -> Result<f64> {
    match v.trim_end_matches('%').parse::<f64>() {
        Ok(p) if (1.0..=100.0).contains(&p) => Ok(p),
        _ => bail!("--max-usage needs a percent from 1 to 100"),
    }
}

fn parse_port(v: Option<String>) -> Result<u16> {
    v.as_deref()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("--port needs a port number"))
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
        "dashboard",
        "skill",
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
        "dashboard" => {
            let mut port = None;
            let mut open = true;
            let mut it = rest().into_iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--no-open" => open = false,
                    "--port" => port = Some(parse_port(it.next())?),
                    _ if a.starts_with("--port=") => {
                        port = Some(parse_port(Some(a["--port=".len()..].to_string()))?)
                    }
                    _ => bail!("usage: agp dashboard [--port <n>] [--no-open]"),
                }
            }
            Command::Dashboard { port, open }
        }
        "skill" => match rest().first().map(String::as_str) {
            None | Some("show") => Command::Skill { install: false },
            Some("install") => Command::Skill { install: true },
            Some(_) => bail!("usage: agp skill [show | install]"),
        },
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
            "--on-limit" => out.run.on_limit = Some(parse_limit_action(&value("--on-limit")?)?),
            "--max-wait" => out.run.max_wait = Some(parse_duration(&value("--max-wait")?)?),
            "--accounts" => out.run.accounts = Some(parse_ids(&value("--accounts")?)),
            "--exclude" => out.run.exclude.extend(parse_ids(&value("--exclude")?)),
            "--max-usage" => out.run.max_usage = Some(parse_pct(&value("--max-usage")?)?),
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
    fn run_options() {
        let l = launch(&[
            "--on-limit",
            "wait",
            "--max-wait=2h30m",
            "--accounts",
            "a, b",
            "--exclude=c",
            "--max-usage",
            "80%",
            "-p",
            "hi",
        ]);
        assert_eq!(l.run.on_limit, Some(LimitAction::Mode(OnLimit::Wait)));
        assert_eq!(l.run.max_wait, Some(9000));
        assert_eq!(l.run.accounts, Some(vec!["a".into(), "b".into()]));
        assert_eq!(l.run.exclude, ["c"]);
        assert_eq!(l.run.max_usage, Some(80.0));
        assert_eq!(l.tool_args, ["-p", "hi"]);
        assert!(l.print_mode());
        let l = launch(&["codex", "--on-limit", "fail", "exec", "x"]);
        assert_eq!(l.run.on_limit, Some(LimitAction::Fail));
        assert_eq!(l.tool_args, ["exec", "x"]);
        assert_eq!(
            launch(&["--on-limit", "switch"]).run.on_limit,
            Some(LimitAction::Mode(OnLimit::Auto))
        );
        // After the first other argument they go to claude.
        let l = launch(&["-p", "--on-limit", "wait"]);
        assert_eq!(l.run, RunOptions::default());
        for d in ["", "2x", "h", "1h5"] {
            assert!(parse_duration(d).is_err(), "{d}");
        }
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert_eq!(parse_duration("1d").unwrap(), 86_400);
        for bad in [
            &["--on-limit", "never"][..],
            &["--max-usage", "0"],
            &["--max-usage", "abc"],
            &["--max-wait", "soon"],
        ] {
            let a: Vec<OsString> = bad.iter().map(OsString::from).collect();
            assert!(parse(&a).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn apply_run_options() {
        use prism_core::config::ProfileConfig;
        let mut config = Config::default();
        for (id, tool, pool) in [
            ("a", Tool::Claude, true),
            ("b", Tool::Claude, false),
            ("c", Tool::Claude, true),
            ("x", Tool::Codex, true),
        ] {
            config.profiles.push(ProfileConfig {
                id: id.into(),
                tool,
                pool,
                ..Default::default()
            });
        }
        let run = RunOptions {
            on_limit: Some(LimitAction::Mode(OnLimit::Wait)),
            accounts: Some(vec!["b".into(), "c".into()]),
            exclude: vec!["c".into()],
            max_usage: Some(80.0),
            ..Default::default()
        };
        let mut c = config.clone();
        run.apply(&mut c, Tool::Claude).unwrap();
        let pool: Vec<_> = c.profiles.iter().map(|p| (p.id.as_str(), p.pool)).collect();
        assert_eq!(pool, [("a", false), ("b", true), ("c", false), ("x", true)]);
        assert_eq!(c.balance.hard, 80.0);
        assert_eq!(c.balance.soft_5h, 80.0);
        assert_eq!(c.balance.on_limit, OnLimit::Wait);
        // Fail leaves the config's on_limit alone.
        let fail = RunOptions {
            on_limit: Some(LimitAction::Fail),
            ..Default::default()
        };
        let mut c = config.clone();
        fail.apply(&mut c, Tool::Claude).unwrap();
        assert_eq!(c.balance.on_limit, OnLimit::Auto);
        // Ids of the other tool are unknown.
        let bad = RunOptions {
            exclude: vec!["x".into()],
            ..Default::default()
        };
        assert!(bad.apply(&mut config.clone(), Tool::Claude).is_err());
    }

    #[test]
    fn bad_values() {
        let a: Vec<OsString> = ["--policy", "nope"].iter().map(OsString::from).collect();
        assert!(parse(&a).is_err());
        let a: Vec<OsString> = ["--as"].iter().map(OsString::from).collect();
        assert!(parse(&a).is_err());
    }
}
