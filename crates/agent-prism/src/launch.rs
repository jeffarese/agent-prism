//! Launching claude (or codex): account inputs, the pick, the hook settings
//! file and the child spec (DESIGN §7.4, §6.6).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use prism_core::config::Config;
use prism_core::keychain;
use prism_core::model::{Policy, Projection, TokenState, Tool};
use prism_core::paths::Paths;
use prism_core::profile::Profile;
use prism_core::registry;
use prism_term::pty::ChildSpec;
use prism_usage::balance::{self, BalanceSettings, PickResult};
use prism_usage::cache::{CachedUsage, Sample, UsageCache};
use prism_usage::infer;
use prism_usage::poller::{UreqFetcher, UsageFetcher};
use prism_usage::tokens::TokenSource;
use prism_usage::views::{AccountInput, ViewCtx};
use serde_json::{Map, Value};

use crate::app::AppCtx;
use crate::tokens::KeychainTokens;

/// Hook events agent-prism registers (DESIGN §8).
pub const HOOK_EVENTS: [&str; 5] = [
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

/// Claude flags that choose a session; agent-prism then adds no `--session-id`.
const SESSION_FLAGS: [&str; 6] = [
    "-r",
    "--resume",
    "-c",
    "--continue",
    "--session-id",
    "--fork-session",
];
const FROM_PR: &str = "--from-pr";

/// Claude flags that take a value (for finding a trailing positional prompt).
const VALUE_FLAGS: [&str; 27] = [
    "--model",
    "--agent",
    "--agents",
    "--append-system-prompt",
    "--system-prompt",
    "--permission-mode",
    "--fallback-model",
    "--settings",
    "--setting-sources",
    "--mcp-config",
    "--add-dir",
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
    "--tools",
    "--output-format",
    "--input-format",
    "--session-id",
    "-r",
    "--resume",
    "--name",
    "-n",
    "--max-budget-usd",
    "--plugin-dir",
    "--debug",
    "--from-pr",
];

/// Current unix time in seconds.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Test hook: `AGP_NO_POLL=1` disables the poller and the launch-time
/// refresh; cache files are used as they are.
pub fn polling_disabled() -> bool {
    prism_core::env::var_os("AGP_NO_POLL").is_some_and(|v| v != "0")
}

/// The args before the first `--` (after it everything is positional).
pub fn options(args: &[String]) -> &[String] {
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    &args[..end]
}

/// `user` with `extra` inserted before the first `--` (else appended), so
/// agent-prism-added flags never become positionals.
pub fn splice_args(user: &[String], extra: Vec<String>) -> Vec<String> {
    let end = options(user).len();
    let mut out = user[..end].to_vec();
    out.extend(extra);
    out.extend(user[end..].iter().cloned());
    out
}

fn flag_name(a: &str) -> &str {
    a.split_once('=').map_or(a, |(f, _)| f)
}

/// True if the user chose a session (resume, continue, fixed id, ...).
pub fn has_session_flag(args: &[String]) -> bool {
    options(args).iter().any(|a| {
        let f = flag_name(a);
        SESSION_FLAGS.contains(&f) || f == FROM_PR
    })
}

/// The args without the user's session flags (and their values) and without
/// a trailing positional prompt (or anything after `--`). Used when resuming
/// on another account.
pub fn strip_for_switch(args: &[String]) -> Vec<String> {
    let args = options(args);
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let f = flag_name(a);
        let inline = a.contains('=') && a.starts_with("--");
        if SESSION_FLAGS.contains(&f) || f == FROM_PR {
            // --session-id always takes a value; -r/--resume/--from-pr take
            // an optional one.
            let takes = matches!(f, "--session-id" | "-r" | "--resume" | FROM_PR);
            let next_is_value = args.get(i + 1).is_some_and(|n| !n.starts_with('-'));
            i += 1;
            if takes && !inline && (f == "--session-id" || next_is_value) {
                i += 1;
            }
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    if let Some(last) = out.last()
        && !last.starts_with('-')
    {
        let prev_takes_value = out.len() >= 2 && VALUE_FLAGS.contains(&out[out.len() - 2].as_str());
        if !prev_takes_value {
            out.pop();
        }
    }
    out
}

/// The args for a print-mode resume: [`strip_for_switch`] and also every
/// other positional (the prompt can sit anywhere among the flags of
/// `claude -p`).
pub fn strip_positionals(args: &[String]) -> Vec<String> {
    let args = strip_for_switch(args);
    let mut out = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if !a.starts_with('-') {
            continue;
        }
        let takes = !a.contains('=') && VALUE_FLAGS.contains(&a.as_str());
        out.push(a);
        if takes && let Some(v) = it.next() {
            out.push(v);
        }
    }
    out
}

/// Splits off the user's `--settings` (value) from the args.
pub fn take_user_settings(args: &[String]) -> (Vec<String>, Option<String>) {
    let mut rest = Vec::new();
    let mut value = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            rest.push(a.clone());
            rest.extend(it.by_ref().cloned());
            break;
        }
        if a == "--settings" {
            value = it.next().cloned();
        } else if let Some(v) = a.strip_prefix("--settings=") {
            value = Some(v.to_string());
        } else {
            rest.push(a.clone());
        }
    }
    (rest, value)
}

/// The `--model` value among the user args.
pub fn user_model(args: &[String]) -> Option<String> {
    let mut it = options(args).iter();
    while let Some(a) = it.next() {
        if a == "--model" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--model=") {
            return Some(v.to_string());
        }
    }
    None
}

/// The model for scoped windows: the user's `--model`, else `model` from
/// `<common>/settings.json`.
pub fn session_model(user_args: &[String], common: &Path) -> Option<String> {
    user_model(user_args).or_else(|| {
        let text = std::fs::read_to_string(common.join("settings.json")).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        v.get("model")?.as_str().map(str::to_string)
    })
}

/// A user `--settings` value: inline JSON or a file path.
pub fn load_user_settings(value: &str) -> Result<Value> {
    let t = value.trim_start();
    let text = if t.starts_with('{') {
        value.to_string()
    } else {
        std::fs::read_to_string(value).with_context(|| format!("reading --settings {value}"))?
    };
    let v: Value = serde_json::from_str(&text).context("--settings is not valid JSON")?;
    if !v.is_object() {
        bail!("--settings must be a JSON object");
    }
    Ok(v)
}

/// Single-quotes `s` for `sh`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Merges `add` into `base`: top-level keys of `add` win, except `hooks`,
/// whose per-event arrays are concatenated.
pub fn merge_settings(base: &mut Value, add: &Value) {
    let (Some(b), Some(a)) = (base.as_object_mut(), add.as_object()) else {
        return;
    };
    for (k, v) in a {
        if k == "hooks"
            && let (Some(Value::Object(bh)), Some(ah)) = (b.get_mut("hooks"), v.as_object())
        {
            for (ev, list) in ah {
                match (bh.get_mut(ev), list) {
                    (Some(Value::Array(dst)), Value::Array(src)) => dst.extend(src.iter().cloned()),
                    _ => {
                        bh.insert(ev.clone(), list.clone());
                    }
                }
            }
            continue;
        }
        b.insert(k.clone(), v.clone());
    }
}

/// The hooks settings for `exe`, merged with the profile overlay and the
/// user's `--settings`.
pub fn settings_json(exe: &Path, overlay: Option<&Value>, user: Option<&Value>) -> Value {
    let cmd_base = shell_quote(&exe.to_string_lossy());
    let mut hooks = Map::new();
    for ev in HOOK_EVENTS {
        hooks.insert(
            ev.to_string(),
            serde_json::json!([{"hooks":[{"type":"command","command":format!("{cmd_base} hook {ev}"),"timeout":5}]}]),
        );
    }
    let mut v = serde_json::json!({ "hooks": hooks });
    for extra in [overlay, user].into_iter().flatten() {
        merge_settings(&mut v, extra);
    }
    v
}

/// Env vars that move Claude's auto-memory dir.
pub const MEMORY_ENV: [&str; 2] = [
    "CLAUDE_COWORK_MEMORY_PATH_OVERRIDE",
    "CLAUDE_CODE_REMOTE_MEMORY_DIR",
];

/// Claude's project key for `base` (non-default config dir): every
/// non-alphanumeric char becomes `-`. None for non-ASCII or > 200 chars
/// (Claude then appends a hash we can't compute).
pub fn memory_key(base: &str) -> Option<String> {
    if !base.is_ascii() || base.len() > 200 {
        return None;
    }
    Some(
        base.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect(),
    )
}

/// The project base Claude keys memory on: the main repo root (also for a
/// linked worktree), else the git toplevel, else `cwd`.
pub fn project_base(cwd: &Path) -> PathBuf {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
            "--show-toplevel",
        ])
        .stderr(std::process::Stdio::null())
        .output();
    let Some(out) = out.ok().filter(|o| o.status.success()) else {
        return cwd.to_path_buf();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let (Some(common), Some(top)) = (lines.next(), lines.next()) else {
        return cwd.to_path_buf();
    };
    let common = Path::new(common);
    match common.parent() {
        Some(p) if common.file_name().is_some_and(|n| n == ".git") => p.to_path_buf(),
        _ => PathBuf::from(top),
    }
}

/// An agent-prism-chosen auto-memory dir.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryDir {
    pub dir: String,
    pub key: String,
    /// cwd is the project base (the transcript key must match `key`).
    pub at_base: bool,
}

fn has_memory_key(v: &Value) -> bool {
    v.get("autoMemoryDirectory").is_some()
}

fn file_has_memory_key(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .is_some_and(|v| has_memory_key(&v))
}

/// The `autoMemoryDirectory` agent-prism sets so memory writes go to the real
/// path behind the shared `projects/` symlink (no sensitive-path prompt).
/// None when the default install, no symlink, a memory env var, an existing
/// setting, or an unsure key. `given` are the user and overlay settings.
pub fn memory_dir(
    profile: &Profile,
    cwd: &Path,
    given: &[Option<&Value>],
    env: impl Fn(&str) -> Option<String>,
) -> Option<MemoryDir> {
    let skip = |why: &str| {
        log::debug!("no memory dir for {}: {why}", profile.id);
        None
    };
    if profile.tool != Tool::Claude {
        return None;
    }
    let dir = profile.config_dir_env()?;
    let projects = dir.join("projects");
    let Ok(real) = projects.canonicalize() else {
        return skip("no projects dir");
    };
    if real == projects {
        return skip("projects is not a symlink");
    }
    let set = |k: &str| env(k).is_some_and(|v| !v.is_empty());
    if MEMORY_ENV
        .iter()
        .any(|k| set(k) || profile.env.get(*k).is_some_and(|v| !v.is_empty()))
    {
        return skip("memory env var set");
    }
    if given.iter().flatten().any(|v| has_memory_key(v)) {
        return skip("autoMemoryDirectory in --settings or overlay");
    }
    let base = project_base(cwd);
    let mut files = vec![dir.join("settings.json"), dir.join("settings.local.json")];
    for d in [cwd, base.as_path()] {
        files.push(d.join(".claude/settings.json"));
        files.push(d.join(".claude/settings.local.json"));
    }
    if files.iter().any(|f| file_has_memory_key(f)) {
        return skip("autoMemoryDirectory in a settings file");
    }
    let Some(key) = base.to_str().and_then(memory_key) else {
        return skip("cannot compute the project key");
    };
    let at_base = cwd == base;
    if !at_base && !real.join(&key).is_dir() {
        return skip("project dir not found");
    }
    let mem = real.join(&key).join("memory");
    log::info!("memory dir for {}: {}", profile.id, mem.display());
    Some(MemoryDir {
        dir: mem.to_string_lossy().into_owned(),
        key,
        at_base,
    })
}

/// Logs a warning if the transcript dir name differs from agent-prism's key.
pub fn check_memory_key(transcript: &str, key: &str) -> bool {
    let name = Path::new(transcript)
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str());
    match name {
        Some(n) if n != key => {
            log::warn!(
                "memory key {key:?} differs from transcript dir {n:?}; Claude may key projects differently now"
            );
            false
        }
        _ => true,
    }
}

/// How a child starts.
#[derive(Clone, Debug, PartialEq)]
pub enum Start {
    /// A new session: the user's args as given (plus `--session-id` unless
    /// they chose a session).
    Fresh,
    /// Resume `session_id` on this account, optionally with a first prompt.
    Resume {
        session_id: String,
        prompt: Option<String>,
    },
}

/// Everything needed to build child specs for this agent-prism session.
#[derive(Clone, Debug)]
pub struct SpecBuilder {
    pub paths: Paths,
    pub tool: Tool,
    pub program: PathBuf,
    /// The user's claude args without `--settings` (or the codex args).
    pub user_args: Vec<String>,
    pub user_settings: Option<Value>,
    pub run_dir: PathBuf,
    pub exe: PathBuf,
    pub cwd: PathBuf,
    /// The memory key of the last build, when cwd is the base (checked
    /// once against the SessionStart transcript path).
    pub memory_key: Arc<Mutex<Option<String>>>,
}

impl SpecBuilder {
    /// `<run>/ctl.sock`, or `/tmp/agp-<run name>.sock` when that path is
    /// too long for a Unix socket (macOS allows 104 bytes).
    pub fn sock(&self) -> PathBuf {
        let p = self.run_dir.join("ctl.sock");
        if p.as_os_str().len() < 100 {
            return p;
        }
        let name = self
            .run_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        PathBuf::from(format!("/tmp/agp-{name}.sock"))
    }

    /// Writes `<run>/settings.json` for `profile` and returns the spec plus
    /// the session id agent-prism chose (fresh starts without a session flag).
    pub fn build(
        &self,
        config: &Config,
        profile: &Profile,
        start: &Start,
    ) -> Result<(ChildSpec, Option<String>)> {
        // A session never moves between tools.
        anyhow::ensure!(
            profile.tool == self.tool,
            "{} is a {} account; this session runs {}",
            profile.id,
            profile.tool.name(),
            self.tool.name()
        );
        if profile.tool == Tool::Codex {
            return Ok(self.build_codex(config, profile, start));
        }
        // Right before the spawn, so Claude on this account has little time
        // to rewrite the file in between (§4.7).
        if config.share.sync_trust {
            let claude = config.profiles_for(&self.paths, Tool::Claude);
            prism_core::trust::sync(&self.paths, profile, &claude);
        }
        let overlay_path = self.paths.profile_overlay(&profile.id);
        let overlay = match std::fs::read_to_string(&overlay_path) {
            Ok(t) => Some(
                serde_json::from_str::<Value>(&t)
                    .with_context(|| format!("parsing {}", overlay_path.display()))?,
            ),
            Err(_) => None,
        };
        let mut settings = settings_json(&self.exe, overlay.as_ref(), self.user_settings.as_ref());
        let mem = memory_dir(
            profile,
            &self.cwd,
            &[self.user_settings.as_ref(), overlay.as_ref()],
            |k| std::env::var(k).ok(),
        );
        if let Ok(mut k) = self.memory_key.lock() {
            *k = mem.as_ref().filter(|m| m.at_base).map(|m| m.key.clone());
        }
        if let (Some(m), Some(o)) = (mem, settings.as_object_mut()) {
            o.insert("autoMemoryDirectory".into(), Value::String(m.dir));
        }
        std::fs::create_dir_all(&self.run_dir)
            .with_context(|| format!("creating {}", self.run_dir.display()))?;
        let settings_path = self.run_dir.join("settings.json");
        std::fs::write(&settings_path, serde_json::to_vec_pretty(&settings)?)
            .with_context(|| format!("writing {}", settings_path.display()))?;

        let mut args: Vec<String> = config.claude.args.clone();
        args.extend(profile.args.iter().cloned());
        let mut session = None;
        match start {
            Start::Fresh => {
                let mut extra = vec![
                    "--settings".into(),
                    settings_path.to_string_lossy().into_owned(),
                ];
                if !has_session_flag(&self.user_args) {
                    let id = uuid::Uuid::new_v4().to_string();
                    extra.push("--session-id".into());
                    extra.push(id.clone());
                    session = Some(id);
                }
                args.extend(splice_args(&self.user_args, extra));
            }
            Start::Resume { session_id, prompt } => {
                args.extend(strip_for_switch(&self.user_args));
                args.push("--settings".into());
                args.push(settings_path.to_string_lossy().into_owned());
                args.push("--resume".into());
                args.push(session_id.clone());
                if let Some(p) = prompt.as_ref().filter(|p| !p.is_empty()) {
                    args.push(p.clone());
                }
                session = Some(session_id.clone());
            }
        }
        let (env_set, env_remove) = child_env(profile, &self.sock());
        Ok((
            ChildSpec {
                program: self.program.clone(),
                args: args.into_iter().map(OsString::from).collect(),
                env_set,
                env_remove,
                cwd: self.cwd.clone(),
            },
            session,
        ))
    }
}

impl SpecBuilder {
    /// A Codex child: no hooks file and no `--session-id` (Codex has
    /// neither). `--no-daemon` and `sqlite_home` make every account read and
    /// write the same thread database, so `codex resume <id>` works after a
    /// switch. The session id of a fresh start comes later from that
    /// database (see `codex::find_thread`).
    fn build_codex(
        &self,
        config: &Config,
        profile: &Profile,
        start: &Start,
    ) -> (ChildSpec, Option<String>) {
        let (_, config_args) = config.tool_bin_args(Tool::Codex);
        let mut base: Vec<String> = config_args.to_vec();
        base.extend(profile.args.iter().cloned());
        if !crate::codex::uses_remote(&self.user_args) {
            base.push("--no-daemon".into());
        }
        base.push("-c".into());
        base.push(format!(
            "sqlite_home={}",
            toml_string(&codex_sqlite_home(profile, |k| std::env::var(k).ok()).to_string_lossy())
        ));
        let (args, session) = match start {
            Start::Fresh => (
                crate::codex::insert_flags(&self.user_args, base),
                crate::codex::resume_target(&self.user_args)
                    .filter(|id| uuid::Uuid::parse_str(id).is_ok()),
            ),
            Start::Resume { session_id, prompt } => {
                let mut args = vec!["resume".to_string()];
                args.extend(base);
                args.extend(crate::codex::strip_for_switch(&self.user_args));
                args.push(session_id.clone());
                if let Some(p) = prompt.as_ref().filter(|p| !p.is_empty()) {
                    args.push(p.clone());
                }
                (args, Some(session_id.clone()))
            }
        };
        let (env_set, env_remove) = child_env(profile, &self.sock());
        (
            ChildSpec {
                program: self.program.clone(),
                args: args.into_iter().map(OsString::from).collect(),
                env_set,
                env_remove,
                cwd: self.cwd.clone(),
            },
            session,
        )
    }
}

/// A TOML basic string.
fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The Codex database dir for `profile`: the profile env, else a
/// `CODEX_SQLITE_HOME` the user set, else the common Codex home.
pub fn codex_sqlite_home(profile: &Profile, env: impl Fn(&str) -> Option<String>) -> PathBuf {
    if profile.env.contains_key("CODEX_SQLITE_HOME") {
        return profile.codex_sqlite_home();
    }
    env("CODEX_SQLITE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| profile.codex_sqlite_home())
}

/// Env changes for a child on `profile` (DESIGN §7.4). `sock` may be empty
/// (print mode, `agp exec`): then no agent-prism variables are set.
pub fn child_env(profile: &Profile, sock: &Path) -> (Vec<(OsString, OsString)>, Vec<OsString>) {
    let mut set: Vec<(OsString, OsString)> = Vec::new();
    let mut remove = Vec::new();
    let var = profile.tool.home_env();
    match profile.config_dir_env() {
        Some(d) => set.push((var.into(), d.into())),
        None => remove.push(OsString::from(var)),
    }
    if profile.tool == Tool::Codex {
        let db = codex_sqlite_home(profile, |k| std::env::var(k).ok());
        set.push(("CODEX_SQLITE_HOME".into(), db.into()));
    }
    let vars: [(&str, OsString); 4] = [
        ("AGP", "1".into()),
        ("AGP_SOCK", sock.into()),
        ("AGP_PROFILE", profile.id.clone().into()),
        // For status line scripts: the label the dock shows, e.g. "Work".
        ("AGP_LABEL", profile.label.clone().into()),
    ];
    for (k, v) in vars {
        if sock.as_os_str().is_empty() {
            remove.push(k.into());
        } else {
            set.push((k.into(), v));
        }
    }
    for (k, v) in &profile.env {
        set.push((k.into(), v.into()));
    }
    (set, remove)
}

/// `<state>/last_used.json`: profile id → unix time of the last start.
pub fn load_last_used(paths: &Paths) -> HashMap<String, i64> {
    std::fs::read_to_string(paths.state_dir.join("last_used.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Records that a session started on `id` now.
pub fn touch_last_used(paths: &Paths, id: &str, now: i64) {
    let mut m = load_last_used(paths);
    m.insert(id.to_string(), now);
    let file = paths.state_dir.join("last_used.json");
    let tmp = file.with_extension("json.tmp");
    let _ = std::fs::create_dir_all(&paths.state_dir);
    if let Ok(data) = serde_json::to_vec(&m)
        && std::fs::write(&tmp, data).is_ok()
    {
        let _ = std::fs::rename(&tmp, &file);
    }
}

/// Balancer inputs for every profile, in config order.
pub fn account_inputs(
    paths: &Paths,
    profiles: &[Profile],
    tokens: &KeychainTokens,
    active: Option<&str>,
    now: i64,
) -> Vec<AccountInput> {
    let me = std::process::id();
    let live = registry::list_live(paths);
    let last = load_last_used(paths);
    tokens.prefetch();
    profiles
        .iter()
        .map(|p| {
            let mut identity = prism_core::identity::read_identity(p, paths);
            identity.tool = p.tool;
            let token = match tokens.credentials(&p.id) {
                Ok(c) => {
                    if let Some(c) = &c {
                        identity.subscription = c.subscription_type.clone();
                        identity.rate_limit_tier = c.rate_limit_tier.clone();
                    }
                    keychain::token_state(c.as_ref(), now)
                }
                Err(e) => TokenState::Unreadable(format!("{e:#}")),
            };
            AccountInput {
                profile_id: p.id.clone(),
                label: p.label.clone(),
                color: p.color.clone(),
                in_pool: p.pool,
                weight: p.weight,
                reserve_7d: p.reserve_7d,
                identity,
                token,
                sessions_elsewhere: live
                    .iter()
                    .filter(|r| r.pid != me && r.profile == p.id)
                    .count() as u32,
                active_here: active == Some(p.id.as_str()),
                last_used: last.get(&p.id).copied(),
            }
        })
        .collect()
}

/// The cached usage of every profile.
pub fn load_usage(cache: &UsageCache, profiles: &[Profile]) -> HashMap<String, CachedUsage> {
    profiles
        .iter()
        .filter_map(|p| Some((p.id.clone(), cache.load(&p.id)?)))
        .collect()
}

/// Projections from recent samples.
pub fn projections(
    cache: &UsageCache,
    usage: &HashMap<String, CachedUsage>,
    now: i64,
) -> HashMap<String, Projection> {
    usage
        .iter()
        .filter_map(|(id, c)| {
            let snap = c.snapshot.as_ref()?;
            let samples = cache.samples(id, now - infer::LOOKBACK_7D_SECS);
            Some((id.clone(), infer::project(&samples, snap, now)))
        })
        .collect()
}

/// The view context from the config.
pub fn view_ctx(
    config: &Config,
    policy: Option<Policy>,
    model: Option<String>,
    exclude: Option<String>,
    now: i64,
) -> ViewCtx {
    ViewCtx {
        now,
        model,
        settings: BalanceSettings {
            policy: policy.unwrap_or(config.balance.policy),
            soft_5h: config.balance.soft_5h,
            hard: config.balance.hard,
        },
        exclude,
        // Idle accounts are polled every poll_idle_secs; twice that is stale.
        stale_after_secs: 2 * config.usage.poll_idle_secs as i64,
    }
}

/// Fetches usage for profiles whose cache is older than
/// `poll_active_secs`, in parallel, waiting at most `budget`. Fetches that
/// run late finish in the background.
pub fn refresh_stale(
    cache: &UsageCache,
    config: &Config,
    profiles: &[Profile],
    tokens: &Arc<KeychainTokens>,
    budget: Duration,
) {
    if polling_disabled() {
        return;
    }
    let now = unix_now();
    let max_age = config.usage.poll_active_secs as i64;
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let mut started = 0;
    for p in profiles {
        let fresh = cache.load(&p.id).is_some_and(|c| {
            now - c.last_attempt < max_age || c.backoff_until.is_some_and(|b| b > now)
        });
        if fresh {
            continue;
        }
        let Ok(Some(tok)) = tokens.token(&p.id) else {
            continue;
        };
        if tok.is_expired(now) {
            continue;
        }
        let (cache, id, tx) = (cache.clone(), p.id.clone(), tx.clone());
        started += 1;
        std::thread::spawn(move || {
            if let Err(e) = fetch_one(&cache, &id, &tok, now) {
                log::warn!("usage refresh {id}: {e:#}");
            }
            let _ = tx.send(());
        });
    }
    let deadline = Instant::now() + budget;
    for _ in 0..started {
        let left = deadline.saturating_duration_since(Instant::now());
        if rx.recv_timeout(left).is_err() {
            break;
        }
    }
}

fn fetch_one(
    cache: &UsageCache,
    id: &str,
    tok: &prism_usage::tokens::AccessToken,
    now: i64,
) -> Result<()> {
    let Some(_lock) = cache.try_lock(id)? else {
        return Ok(());
    };
    let mut next = cache.load(id).unwrap_or_default();
    next.last_attempt = now;
    next.token_expires_at = tok.expires_at;
    match UreqFetcher::default().fetch(tok, now) {
        Ok(snap) => {
            cache.append_sample(
                id,
                &Sample {
                    ts: now,
                    u5h: snap.five_hour.as_ref().map(|w| w.utilization),
                    u7d: snap.seven_day.as_ref().map(|w| w.utilization),
                },
            )?;
            next.snapshot = Some(snap);
            next.last_error = None;
            next.backoff_until = None;
        }
        Err(prism_usage::api::FetchError::RateLimited { retry_after_secs }) => {
            let secs = retry_after_secs.unwrap_or(prism_usage::api::DEFAULT_BACKOFF_SECS) as i64;
            next.backoff_until = Some(now + secs);
            next.last_error = Some("rate limited".into());
        }
        Err(e) => next.last_error = Some(e.to_string()),
    }
    cache.store(id, &next)
}

/// Inputs, usage and the pick for a new session (after a refresh).
pub struct Snapshot {
    pub inputs: Vec<AccountInput>,
    pub usage: HashMap<String, CachedUsage>,
    pub ctx: ViewCtx,
    pub pick: PickResult,
}

/// Refreshes stale pool usage (2.5 s budget) and picks.
pub fn snapshot(
    app: &AppCtx,
    config: &Config,
    profiles: &[Profile],
    tokens: &Arc<KeychainTokens>,
    policy: Option<Policy>,
    model: Option<String>,
) -> Snapshot {
    let now = unix_now();
    let pool: Vec<Profile> = profiles.iter().filter(|p| p.pool).cloned().collect();
    refresh_stale(
        &app.cache,
        config,
        &pool,
        tokens,
        Duration::from_millis(2500),
    );
    let now = now.max(unix_now());
    let inputs = account_inputs(&app.paths, profiles, tokens, None, now);
    let usage = load_usage(&app.cache, profiles);
    let ctx = view_ctx(config, policy, model, None, now);
    let pick = balance::pick(&inputs, &usage, &ctx);
    Snapshot {
        inputs,
        usage,
        ctx,
        pick,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn session_flags() {
        assert!(has_session_flag(&v(&["-c"])));
        assert!(has_session_flag(&v(&["--resume=abc"])));
        assert!(has_session_flag(&v(&["--from-pr", "12"])));
        assert!(has_session_flag(&v(&["--fork-session"])));
        assert!(!has_session_flag(&v(&["--model", "opus", "hi"])));
    }

    #[test]
    fn strip_all_positionals() {
        assert_eq!(
            strip_positionals(&v(&[
                "-p",
                "the prompt",
                "--model",
                "opus",
                "--output-format=json",
                "-r",
                "old",
                "--verbose",
                "--",
                "x"
            ])),
            v(&["-p", "--model", "opus", "--output-format=json", "--verbose"])
        );
    }

    #[test]
    fn strip_prompt_and_session() {
        assert_eq!(
            strip_for_switch(&v(&["--model", "opus", "fix the bug"])),
            v(&["--model", "opus"])
        );
        // The value of a value-taking flag is not a prompt.
        assert_eq!(
            strip_for_switch(&v(&["--model", "opus"])),
            v(&["--model", "opus"])
        );
        assert_eq!(
            strip_for_switch(&v(&["-r", "abc", "--verbose"])),
            v(&["--verbose"])
        );
        assert_eq!(
            strip_for_switch(&v(&["--session-id", "x", "-c", "--add-dir", "/a"])),
            v(&["--add-dir", "/a"])
        );
        assert_eq!(strip_for_switch(&v(&["hello"])), Vec::<String>::new());
        assert_eq!(
            strip_for_switch(&v(&["--resume=abc", "-c", "hi"])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn user_settings_split() {
        let (rest, s) = take_user_settings(&v(&["a", "--settings", "x.json", "b"]));
        assert_eq!(rest, v(&["a", "b"]));
        assert_eq!(s.as_deref(), Some("x.json"));
        let (_, s) = take_user_settings(&v(&["--settings={}"]));
        assert_eq!(s.as_deref(), Some("{}"));
        assert_eq!(
            user_model(&v(&["--model=sonnet"])).as_deref(),
            Some("sonnet")
        );
    }

    #[test]
    fn settings_merge() {
        let exe = Path::new("/opt/re lay/agp");
        let overlay = serde_json::json!({"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"o"}]}]}});
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("u.json");
        std::fs::write(
            &f,
            r#"{"model":"sonnet","env":{"A":"1"},"hooks":{"Notification":[]}}"#,
        )
        .unwrap();
        let user = load_user_settings(f.to_str().unwrap()).unwrap();
        let s = settings_json(exe, Some(&overlay), Some(&user));
        assert_eq!(s["model"], "sonnet");
        assert_eq!(s["env"]["A"], "1");
        let stop = s["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(
            stop[0]["hooks"][0]["command"],
            "'/opt/re lay/agp' hook Stop"
        );
        assert_eq!(stop[0]["hooks"][0]["timeout"], 5);
        assert_eq!(stop[1]["hooks"][0]["command"], "o");
        assert!(s["hooks"]["Notification"].is_array());
        for ev in HOOK_EVENTS {
            assert!(s["hooks"][ev].is_array(), "{ev}");
        }
        let inline = load_user_settings(r#"{"x":1}"#).unwrap();
        assert_eq!(settings_json(exe, None, Some(&inline))["x"], 1);
        assert!(load_user_settings("[1]").is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }

    fn profile(dir: &str, default: bool) -> Profile {
        let common = if default { dir } else { "/h/.claude" };
        let mut p = Profile::bare("p", Tool::Claude, dir.into(), common.into());
        p.label = "P".into();
        p.is_default_install = default;
        p.pool = true;
        p.args = vec!["--pa".into()];
        p.env = [("X".to_string(), "y".to_string())].into();
        p
    }

    fn builder(root: &Path, user_args: &[&str]) -> SpecBuilder {
        SpecBuilder {
            paths: Paths::under(root),
            tool: Tool::Claude,
            program: "/bin/claude".into(),
            user_args: v(user_args),
            user_settings: None,
            run_dir: root.join("run"),
            exe: "/bin/agp".into(),
            cwd: "/work".into(),
            memory_key: Default::default(),
        }
    }

    fn get<'a>(spec: &'a ChildSpec, k: &str) -> Option<&'a OsString> {
        spec.env_set.iter().find(|(a, _)| a == k).map(|(_, b)| b)
    }

    #[test]
    fn spec_env_default_vs_profile() {
        let dir = tempfile::tempdir().unwrap();
        let b = builder(dir.path(), &["hi"]);
        let mut cfg = Config::default();
        cfg.claude.args = vec!["--ca".into()];
        let (spec, sid) = b
            .build(&cfg, &profile("/h/.claude-x", false), &Start::Fresh)
            .unwrap();
        assert_eq!(get(&spec, "CLAUDE_CONFIG_DIR").unwrap(), "/h/.claude-x");
        assert_eq!(get(&spec, "AGP").unwrap(), "1");
        assert_eq!(get(&spec, "AGP_PROFILE").unwrap(), "p");
        assert_eq!(get(&spec, "X").unwrap(), "y");
        assert!(
            get(&spec, "AGP_SOCK")
                .unwrap()
                .to_string_lossy()
                .ends_with("run/ctl.sock")
        );
        assert!(spec.env_remove.is_empty());
        let args: Vec<String> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into())
            .collect();
        let sid = sid.unwrap();
        assert_eq!(args[..3], v(&["--ca", "--pa", "hi"]));
        assert_eq!(args[3], "--settings");
        assert_eq!(args[5..], v(&["--session-id", &sid]));
        assert!(dir.path().join("run/settings.json").is_file());

        let (spec, _) = b
            .build(&cfg, &profile("/h/.claude", true), &Start::Fresh)
            .unwrap();
        assert!(get(&spec, "CLAUDE_CONFIG_DIR").is_none());
        assert_eq!(spec.env_remove, vec![OsString::from("CLAUDE_CONFIG_DIR")]);
    }

    #[test]
    fn spec_syncs_trust() {
        let dir = tempfile::tempdir().unwrap();
        let b = builder(dir.path(), &[]);
        let mut cfg = Config::default();
        for id in ["a", "b"] {
            cfg.profiles.push(prism_core::config::ProfileConfig {
                id: id.into(),
                dir: format!("~/.claude-{id}"),
                ..Default::default()
            });
        }
        let ps = cfg.profiles_for(&b.paths, Tool::Claude);
        for (p, v) in ps.iter().zip([
            serde_json::json!({"projects": {"/w": {"hasTrustDialogAccepted": true}}}),
            serde_json::json!({"projects": {}}),
        ]) {
            std::fs::create_dir_all(&p.dir).unwrap();
            std::fs::write(p.claude_json_path(&b.paths), v.to_string()).unwrap();
        }
        let trusted = || prism_core::trust::status(&b.paths, &ps[1], &ps).map(|(own, _)| own);
        cfg.share.sync_trust = false;
        b.build(&cfg, &ps[1], &Start::Fresh).unwrap();
        assert_eq!(trusted(), Some(0));
        cfg.share.sync_trust = true;
        b.build(&cfg, &ps[1], &Start::Fresh).unwrap();
        assert_eq!(trusted(), Some(1));
    }

    #[test]
    fn spec_resume_and_user_session() {
        let dir = tempfile::tempdir().unwrap();
        let b = builder(dir.path(), &["-c", "--model", "opus", "hi"]);
        let cfg = Config::default();
        let p = profile("/h/.claude-x", false);
        let (spec, sid) = b.build(&cfg, &p, &Start::Fresh).unwrap();
        assert!(sid.is_none());
        assert!(!spec.args.iter().any(|a| a == "--session-id"));
        let start = Start::Resume {
            session_id: "abc".into(),
            prompt: Some("continue".into()),
        };
        let (spec, _) = b.build(&cfg, &p, &start).unwrap();
        let args: Vec<String> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into())
            .collect();
        assert_eq!(args[..3], v(&["--pa", "--model", "opus"]));
        assert_eq!(args[5..], v(&["--resume", "abc", "continue"]));
    }

    #[test]
    fn dashdash_is_positional() {
        let a = v(&[
            "--model",
            "opus",
            "--",
            "--settings",
            "x",
            "-c",
            "--model=y",
        ]);
        assert!(!has_session_flag(&a));
        assert_eq!(user_model(&a).as_deref(), Some("opus"));
        assert_eq!(user_model(&v(&["--", "--model", "y"])), None);
        let (rest, s) = take_user_settings(&a);
        assert_eq!(rest, a);
        assert_eq!(s, None);
        assert_eq!(
            strip_for_switch(&v(&["--verbose", "--", "-x prompt"])),
            v(&["--verbose"])
        );
        assert_eq!(
            splice_args(&v(&["-p", "--", "-x"]), v(&["--settings", "s"])),
            v(&["-p", "--settings", "s", "--", "-x"])
        );
        assert_eq!(splice_args(&v(&["-p"]), v(&["--y"])), v(&["-p", "--y"]));
    }

    #[test]
    fn spec_args_before_user_dashdash() {
        let dir = tempfile::tempdir().unwrap();
        let b = builder(dir.path(), &["--verbose", "--", "-x prompt"]);
        let (spec, sid) = b
            .build(
                &Config::default(),
                &profile("/h/.claude-x", false),
                &Start::Fresh,
            )
            .unwrap();
        let args: Vec<String> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into())
            .collect();
        let sid = sid.unwrap();
        assert_eq!(args[..2], v(&["--pa", "--verbose"]));
        assert_eq!(args[2], "--settings");
        assert_eq!(args[4..], v(&["--session-id", &sid, "--", "-x prompt"]));
    }

    #[test]
    fn memory_keys() {
        assert_eq!(
            memory_key("/private/tmp/claude-501/-Users-x").as_deref(),
            Some("-private-tmp-claude-501--Users-x")
        );
        assert_eq!(memory_key("/tmp/é"), None);
        let ok = format!("/{}", "a".repeat(199));
        assert_eq!(memory_key(&ok).unwrap().len(), 200);
        assert_eq!(memory_key(&format!("{ok}b")), None);
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn base_detection() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(project_base(&plain), plain);
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "i",
            ],
        );
        assert_eq!(project_base(&repo), repo);
        assert_eq!(project_base(&repo.join("sub")), repo);
        let wt = root.join("wt");
        git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        assert_eq!(project_base(&wt), repo);
    }

    /// A profile whose `projects` links to `<root>/real`, and a cwd.
    fn mem_setup() -> (tempfile::TempDir, Profile, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let real = root.join("real");
        let cwd = root.join("work");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let dir = root.join(".claude-x");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&real, dir.join("projects")).unwrap();
        let mut p = profile(dir.to_str().unwrap(), false);
        p.env.clear();
        (t, p, real, cwd)
    }

    #[test]
    fn memory_dir_rules() {
        let none = |_: &str| None;
        let (_t, p, real, cwd) = mem_setup();
        let key = memory_key(cwd.to_str().unwrap()).unwrap();
        let m = memory_dir(&p, &cwd, &[], none).unwrap();
        assert_eq!(m.dir, real.join(&key).join("memory").to_string_lossy());
        assert!(m.at_base);
        // Default install.
        let mut d = p.clone();
        d.is_default_install = true;
        assert!(memory_dir(&d, &cwd, &[], none).is_none());
        // Env vars.
        assert!(memory_dir(&p, &cwd, &[], |k| (k == MEMORY_ENV[0]).then(|| "x".into())).is_none());
        assert!(memory_dir(&p, &cwd, &[], |_| Some(String::new())).is_some());
        let mut e = p.clone();
        e.env.insert(MEMORY_ENV[1].into(), "y".into());
        assert!(memory_dir(&e, &cwd, &[], none).is_none());
        // Given settings.
        let set = serde_json::json!({"autoMemoryDirectory":"/x"});
        assert!(memory_dir(&p, &cwd, &[None, Some(&set)], none).is_none());
        // Settings files: profile dir and project; invalid files ignored.
        std::fs::create_dir_all(cwd.join(".claude")).unwrap();
        std::fs::write(cwd.join(".claude/settings.json"), "{bad").unwrap();
        assert!(memory_dir(&p, &cwd, &[], none).is_some());
        std::fs::write(
            cwd.join(".claude/settings.local.json"),
            r#"{"autoMemoryDirectory":"/x"}"#,
        )
        .unwrap();
        assert!(memory_dir(&p, &cwd, &[], none).is_none());
        std::fs::remove_file(cwd.join(".claude/settings.local.json")).unwrap();
        std::fs::write(
            p.dir.join("settings.json"),
            r#"{"autoMemoryDirectory":"/x"}"#,
        )
        .unwrap();
        assert!(memory_dir(&p, &cwd, &[], none).is_none());
        std::fs::remove_file(p.dir.join("settings.json")).unwrap();
        // Not at the base: needs the project dir.
        let sub = cwd.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        git(&cwd, &["init", "-q"]);
        let m = memory_dir(&p, &sub, &[], none);
        assert!(m.is_none());
        std::fs::create_dir_all(real.join(&key)).unwrap();
        let m = memory_dir(&p, &sub, &[], none).unwrap();
        assert!(!m.at_base);
        assert_eq!(m.key, key);
    }

    #[test]
    fn memory_dir_no_symlink() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let dir = root.join(".claude-x");
        std::fs::create_dir_all(dir.join("projects")).unwrap();
        let p = profile(dir.to_str().unwrap(), false);
        assert!(memory_dir(&p, &root, &[], |_| None).is_none());
    }

    #[test]
    fn memory_key_check() {
        assert!(check_memory_key("/c/projects/-a-b/s.jsonl", "-a-b"));
        assert!(!check_memory_key("/c/projects/-a-c/s.jsonl", "-a-b"));
    }

    #[test]
    fn spec_memory_dir() {
        let (_t, p, real, cwd) = mem_setup();
        let mut b = builder(cwd.parent().unwrap(), &["hi"]);
        b.cwd = cwd.clone();
        let read = |b: &SpecBuilder| -> Value {
            let t = std::fs::read_to_string(b.run_dir.join("settings.json")).unwrap();
            serde_json::from_str(&t).unwrap()
        };
        let key = memory_key(cwd.to_str().unwrap()).unwrap();
        b.build(&Config::default(), &p, &Start::Fresh).unwrap();
        assert_eq!(
            read(&b)["autoMemoryDirectory"],
            real.join(&key).join("memory").to_string_lossy().as_ref()
        );
        assert_eq!(b.memory_key.lock().unwrap().as_deref(), Some(key.as_str()));
        let start = Start::Resume {
            session_id: "s".into(),
            prompt: None,
        };
        b.build(&Config::default(), &p, &start).unwrap();
        assert!(read(&b)["autoMemoryDirectory"].is_string());
        // A user value wins and agp adds nothing.
        b.user_settings = Some(serde_json::json!({"autoMemoryDirectory":"/u"}));
        b.build(&Config::default(), &p, &Start::Fresh).unwrap();
        assert_eq!(read(&b)["autoMemoryDirectory"], "/u");
        assert!(b.memory_key.lock().unwrap().is_none());
        // So does the overlay.
        b.user_settings = None;
        let o = b.paths.profile_overlay(&p.id);
        std::fs::create_dir_all(o.parent().unwrap()).unwrap();
        std::fs::write(&o, r#"{"autoMemoryDirectory":"/o"}"#).unwrap();
        b.build(&Config::default(), &p, &Start::Fresh).unwrap();
        assert_eq!(read(&b)["autoMemoryDirectory"], "/o");
    }
}
