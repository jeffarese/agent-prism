//! The login flow (DESIGN §4.6): for a new account ask the tool, id and
//! label and create the dir; then run `claude auth login` (or `codex login`)
//! in an embedded terminal and read who logged in.

use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use prism_core::config::{Config, ProfileConfig};
use prism_core::identity::read_identity;
use prism_core::model::{AccountIdentity, Tool};
use prism_core::profile::Profile;
use prism_core::share::create_profile_dir;
use prism_term::pty::{ChildSpec, ChildStatus, PtyChild};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use tui_term::vt100;
use tui_term::widget::PseudoTerminal;

use super::adopt::FlowOutcome;
use super::data::{TextField, valid_slug};
use super::theme as t;
use crate::app::AppCtx;

/// The program that performs the login (claude, codex, or a fake in tests).
#[derive(Clone, Debug, PartialEq)]
pub struct LoginCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

/// A finished login.
#[derive(Clone, Debug, PartialEq)]
pub struct LoginDone {
    /// Profile id.
    pub id: String,
    pub label: String,
    pub tool: Tool,
    /// Dir as written in the config (`~/.claude-<id>`, `~/.codex-<id>`).
    pub dir: String,
    /// True for a new account, false for "log in again".
    pub is_new: bool,
    pub identity: AccountIdentity,
}

impl LoginDone {
    /// The `[[profile]]` entry for a new account.
    pub fn profile_config(&self) -> ProfileConfig {
        ProfileConfig {
            id: self.id.clone(),
            label: self.label.clone(),
            dir: self.dir.clone(),
            tool: self.tool,
            pool: true,
            ..Default::default()
        }
    }
}

struct Running {
    child: PtyChild,
    parser: Arc<Mutex<vt100::Parser>>,
    status: Arc<Mutex<Option<i32>>>,
    size: (u16, u16),
}

/// A row of the form.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Field {
    Tool,
    Id,
    Label,
}

enum Stage {
    /// Asking tool, id and label (new accounts only). `focus` is an index
    /// into [`LoginView::fields`].
    Form {
        focus: usize,
        error: Option<String>,
    },
    Running(Running),
    Success(AccountIdentity),
    Failed {
        error: String,
        tail: Vec<String>,
    },
}

/// The embedded login flow.
pub struct LoginView {
    ctx: Arc<AppCtx>,
    config: Config,
    id: TextField,
    label: TextField,
    tool: Tool,
    /// The form shows the tool row (`agp add` without `--claude` or
    /// `--codex`).
    choose_tool: bool,
    is_new: bool,
    /// Expanded dir; set when the login starts.
    dir: Option<PathBuf>,
    is_default_install: bool,
    /// The login command; `None` = `<claude> auth login` or `<codex> login`.
    pub command: Option<LoginCommand>,
    stage: Stage,
    /// Set once the login succeeded and the user continued.
    pub result: Option<LoginDone>,
}

impl LoginView {
    /// Add a new account, with the id prefilled if given. With `tool`, the
    /// form does not ask for the tool.
    pub fn add(ctx: Arc<AppCtx>, config: &Config, id: Option<String>, tool: Option<Tool>) -> Self {
        let id = TextField::new(id.as_deref().unwrap_or(""));
        let label = TextField::new("");
        Self {
            ctx,
            config: config.clone(),
            id,
            label,
            tool: tool.unwrap_or_default(),
            choose_tool: tool.is_none(),
            is_new: true,
            dir: None,
            is_default_install: false,
            command: None,
            stage: Stage::Form {
                focus: 0,
                error: None,
            },
            result: None,
        }
    }

    /// Log in again on an existing dir (`id` and `label` are kept). The
    /// login starts at once.
    pub fn relogin(
        ctx: Arc<AppCtx>,
        config: &Config,
        profile: &Profile,
        cmd: Option<LoginCommand>,
    ) -> Self {
        let mut v = Self {
            ctx,
            config: config.clone(),
            id: TextField::new(&profile.id),
            label: TextField::new(&profile.label),
            tool: profile.tool,
            choose_tool: false,
            is_new: false,
            dir: Some(profile.dir.clone()),
            is_default_install: profile.is_default_install,
            command: cmd,
            stage: Stage::Form {
                focus: 0,
                error: None,
            },
            result: None,
        };
        v.start();
        v
    }

    /// The rows of the form.
    fn fields(&self) -> Vec<Field> {
        if self.choose_tool {
            vec![Field::Tool, Field::Id, Field::Label]
        } else {
            vec![Field::Id, Field::Label]
        }
    }

    fn field_at(&self, focus: usize) -> Field {
        let f = self.fields();
        f[focus.min(f.len() - 1)]
    }

    /// The focused row while the form is shown.
    fn form_field(&self) -> Option<Field> {
        match &self.stage {
            Stage::Form { focus, .. } => Some(self.field_at(*focus)),
            _ => None,
        }
    }

    fn index_of(&self, field: Field) -> usize {
        self.fields().iter().position(|f| *f == field).unwrap_or(0)
    }

    /// A new account's dir string: `~/.claude-<id>` or `~/.codex-<id>`.
    fn dir_string(&self) -> String {
        if let Some(d) = &self.dir
            && let Ok(rest) = d.strip_prefix(&self.ctx.paths.home)
        {
            return format!("~/{}", rest.display());
        }
        self.new_dir(self.id.text.trim())
    }

    fn new_dir(&self, id: &str) -> String {
        format!("~/{}-{id}", self.tool.dir_name())
    }

    /// Check the form; returns an error message for the user.
    pub fn validate(&self) -> Result<PathBuf, String> {
        let id = self.id.text.trim();
        if id.is_empty() {
            return Err("Choose a short id, e.g. work or acme-al.".into());
        }
        if !valid_slug(id) {
            return Err("Use lowercase letters, digits and - only.".into());
        }
        if self.config.profiles.iter().any(|p| p.id == id) {
            return Err(format!("An account with id {id} already exists."));
        }
        let shown = self.new_dir(id);
        let dir = self.ctx.paths.expand(&shown);
        let empty = std::fs::read_dir(&dir).map(|mut r| r.next().is_none());
        match empty {
            Ok(false) => Err(format!(
                "{shown} already exists and is not empty — add it from the Found list instead."
            )),
            _ if dir.exists() && !dir.is_dir() => Err(format!("{shown} exists and is a file.")),
            _ => Ok(dir),
        }
    }

    fn submit_form(&mut self) {
        match self.validate() {
            Ok(dir) => {
                let common = self.config.common_home_for(&self.ctx.paths, self.tool);
                let rules = self.config.share_rules(self.tool);
                if let Err(e) = create_profile_dir(&dir, &common, &rules) {
                    self.stage = Stage::Form {
                        focus: self.index_of(Field::Id),
                        error: Some(format!("Could not create the folder: {e:#}")),
                    };
                    return;
                }
                if self.label.text.trim().is_empty() {
                    self.label = TextField::new(self.id.text.trim());
                }
                self.dir = Some(dir);
                self.start();
            }
            Err(e) => {
                let focus = self.index_of(Field::Id);
                self.stage = Stage::Form {
                    focus,
                    error: Some(e),
                };
            }
        }
    }

    fn spec(&self) -> Result<ChildSpec, String> {
        let dir = self.dir.clone().ok_or("no folder")?;
        let cmd = match &self.command {
            Some(c) => c.clone(),
            None => {
                let bin = self
                    .ctx
                    .tool_bin(&self.config, self.tool)
                    .map_err(|e| format!("{e:#}"))?;
                let args = match self.tool {
                    Tool::Claude => vec!["auth".into(), "login".into()],
                    Tool::Codex => vec!["login".into()],
                };
                LoginCommand { program: bin, args }
            }
        };
        let mut spec = ChildSpec {
            program: cmd.program,
            args: cmd.args,
            env_set: vec![],
            env_remove: vec![],
            cwd: self.ctx.paths.home.clone(),
        };
        let var = self.tool.home_env();
        if self.is_default_install {
            spec.env_remove.push(var.into());
        } else {
            spec.env_set.push((var.into(), dir.into_os_string()));
        }
        Ok(spec)
    }

    /// Start (or restart) the login command.
    fn start(&mut self) {
        let (rows, cols) = (20, 80);
        let spec = match self.spec() {
            Ok(s) => s,
            Err(e) => {
                self.stage = Stage::Failed {
                    error: e,
                    tail: vec![],
                };
                return;
            }
        };
        let status = Arc::new(Mutex::new(None));
        let st = status.clone();
        let child = PtyChild::spawn(&spec, rows, cols, move |s| {
            if let ChildStatus::Exited(code) = s {
                *st.lock().unwrap_or_else(|e| e.into_inner()) = Some(code);
            }
        });
        let child = match child {
            Ok(c) => c,
            Err(e) => {
                self.stage = Stage::Failed {
                    error: format!("Could not start {}: {e}", spec.program.display()),
                    tail: vec![],
                };
                return;
            }
        };
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        if let Ok(mut reader) = child.reader() {
            let p = parser.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => p
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .process(&buf[..n]),
                    }
                }
            });
        }
        self.stage = Stage::Running(Running {
            child,
            parser,
            status,
            size: (rows, cols),
        });
    }

    /// Check whether the login command finished.
    pub fn tick(&mut self) {
        let Stage::Running(r) = &self.stage else {
            return;
        };
        let code = *r.status.lock().unwrap_or_else(|e| e.into_inner());
        let Some(code) = code else {
            return;
        };
        // Let the reader drain the last output.
        std::thread::sleep(Duration::from_millis(30));
        let tail = screen_tail(&r.parser);
        if code != 0 {
            self.stage = Stage::Failed {
                error: format!("The login did not finish (exit code {code})."),
                tail,
            };
            return;
        }
        let identity = self.identity();
        if identity.email.is_none() && identity.display_name.is_none() {
            let error = match self.tool {
                Tool::Claude => "The login ended, but no account was found in the folder.".into(),
                Tool::Codex => match self.dir.as_deref().map(prism_core::codex::read_login) {
                    Some(Ok(prism_core::codex::CodexLogin::ApiKey)) => {
                        "This is an API key. agent-prism uses ChatGPT plans only: log in with ChatGPT."
                            .to_string()
                    }
                    _ => "The login ended, but auth.json has no ChatGPT login. If \
                          cli_auth_credentials_store is set in config.toml, set it to \"file\"."
                        .to_string(),
                },
            };
            self.stage = Stage::Failed { error, tail };
        } else {
            self.stage = Stage::Success(identity);
        }
    }

    fn identity(&self) -> AccountIdentity {
        let Some(dir) = self.dir.clone() else {
            return AccountIdentity::default();
        };
        let common = self.config.common_home_for(&self.ctx.paths, self.tool);
        let mut p = Profile::bare(self.id.text.trim(), self.tool, dir, common);
        p.is_default_install = self.is_default_install;
        let mut id = read_identity(&p, &self.ctx.paths);
        if let Ok(Some(c)) = prism_core::keychain::read_credentials(self.ctx.store.as_ref(), &p) {
            id.subscription = c.subscription_type;
            id.rate_limit_tier = c.rate_limit_tier;
        }
        id
    }

    fn stop_child(&mut self) {
        if let Stage::Running(r) = &self.stage {
            r.child.terminate(Duration::from_millis(500));
        }
    }

    /// Handle a key.
    pub fn handle_key(&mut self, key: KeyEvent) -> FlowOutcome {
        match &mut self.stage {
            Stage::Form { focus, .. } => {
                let at = *focus;
                let n = self.fields().len();
                let set_focus = |v: &mut Self, f: usize, e: Option<String>| {
                    v.stage = Stage::Form { focus: f, error: e };
                };
                match (self.field_at(at), key.code) {
                    (_, KeyCode::Esc) => return FlowOutcome::Done(false),
                    (_, KeyCode::Tab | KeyCode::Down) => set_focus(self, (at + 1) % n, None),
                    (_, KeyCode::BackTab | KeyCode::Up) => set_focus(self, (at + n - 1) % n, None),
                    (Field::Tool, KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')) => {
                        self.tool = match self.tool {
                            Tool::Claude => Tool::Codex,
                            Tool::Codex => Tool::Claude,
                        };
                        set_focus(self, at, None);
                    }
                    (Field::Tool, KeyCode::Enter) => set_focus(self, at + 1, None),
                    (Field::Tool, _) => {}
                    (Field::Id, KeyCode::Enter) => {
                        let e = self.validate().err();
                        let next = if e.is_some() { at } else { at + 1 };
                        set_focus(self, next, e);
                    }
                    (Field::Label, KeyCode::Enter) => self.submit_form(),
                    (f, _) => {
                        let field = if f == Field::Id {
                            &mut self.id
                        } else {
                            &mut self.label
                        };
                        if field.handle_key(key) {
                            set_focus(self, at, None);
                        }
                    }
                }
            }
            Stage::Running(r) => {
                if key.code == KeyCode::Esc {
                    self.stop_child();
                    self.stage = Stage::Failed {
                        error: "Login cancelled.".into(),
                        tail: vec![],
                    };
                } else if let Some(bytes) = key_bytes(&key) {
                    let _ = r.child.write_all(&bytes);
                }
            }
            Stage::Success(identity) => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Esc) {
                    let identity = identity.clone();
                    self.result = Some(LoginDone {
                        id: self.id.text.trim().to_string(),
                        label: self.label.text.trim().to_string(),
                        tool: self.tool,
                        dir: self.dir_string(),
                        is_new: self.is_new,
                        identity,
                    });
                    return FlowOutcome::Done(true);
                }
            }
            Stage::Failed { .. } => match key.code {
                KeyCode::Enter | KeyCode::Char('r') if self.dir.is_some() => self.start(),
                KeyCode::Enter | KeyCode::Char('r') => {
                    self.stage = Stage::Form {
                        focus: 0,
                        error: None,
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => return FlowOutcome::Done(false),
                _ => {}
            },
        }
        FlowOutcome::Continue
    }

    /// Paste text into the focused field or the login terminal.
    pub fn handle_paste(&mut self, text: &str) {
        let form_field = self.form_field();
        match &mut self.stage {
            Stage::Form { .. } => match form_field {
                Some(Field::Id) => self.id.insert(text),
                Some(Field::Label) => self.label.insert(text),
                _ => {}
            },
            Stage::Running(r) => {
                let bracketed = r
                    .parser
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .screen()
                    .bracketed_paste();
                let mut bytes = Vec::new();
                if bracketed {
                    bytes.extend_from_slice(b"\x1b[200~");
                }
                bytes.extend_from_slice(text.as_bytes());
                if bracketed {
                    bytes.extend_from_slice(b"\x1b[201~");
                }
                let _ = r.child.write_all(&bytes);
            }
            _ => {}
        }
    }

    /// Key hints for the current stage.
    pub fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.stage {
            Stage::Form { focus, .. } => match self.field_at(*focus) {
                Field::Tool => vec![("←→", "Claude or Codex"), ("⏎", "next"), ("esc", "cancel")],
                Field::Id => vec![("⏎", "next"), ("tab", "next field"), ("esc", "cancel")],
                Field::Label => vec![("⏎", "log in"), ("tab", "next field"), ("esc", "cancel")],
            },
            Stage::Running(_) => vec![("keys", "go to the login"), ("esc", "cancel")],
            Stage::Success(_) => vec![("⏎", "continue")],
            Stage::Failed { .. } => vec![("⏎", "try again"), ("esc", "back")],
        }
    }

    /// Draw the flow (no frame).
    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let focused = self.form_field().unwrap_or(Field::Id);
        let folder = self.new_dir(self.id.text.trim());
        let (tool, choose_tool) = (self.tool, self.choose_tool);
        match &mut self.stage {
            Stage::Form { error, .. } => {
                let field = |name: &str, f: &TextField, on: bool, hint: &str| {
                    let mut spans = vec![
                        t::marker(on),
                        Span::styled(format!("{name:<7}"), t::row(on)),
                    ];
                    spans.extend(f.spans(on));
                    if f.text.is_empty() && !on {
                        spans.push(Span::styled(hint.to_string(), t::dim()));
                    }
                    Line::from(spans)
                };
                let title = if choose_tool {
                    " Add a Claude or Codex account".to_string()
                } else {
                    format!(" Add a {} account", tool.name())
                };
                let mut lines = vec![
                    Line::from(Span::styled(title, t::title())),
                    Line::from(Span::styled(
                        " Each account gets its own folder; your settings and sessions stay shared.",
                        t::dim(),
                    )),
                    Line::from(""),
                ];
                if choose_tool {
                    let on = focused == Field::Tool;
                    let choice = |t2: Tool| {
                        let text = format!(" {} ({} plan) ", t2.name(), t2.plan_name());
                        if t2 == tool {
                            Span::styled(format!("[{text}]"), t::bold())
                        } else {
                            Span::styled(format!(" {text} "), t::dim())
                        }
                    };
                    lines.push(Line::from(vec![
                        t::marker(on),
                        Span::styled(format!("{:<7}", "Tool"), t::row(on)),
                        choice(Tool::Claude),
                        choice(Tool::Codex),
                    ]));
                }
                lines.push(field(
                    "Id",
                    &self.id,
                    focused == Field::Id,
                    "short name, e.g. work",
                ));
                lines.push(field(
                    "Label",
                    &self.label,
                    focused == Field::Label,
                    "shown in the dock (optional)",
                ));
                lines.push(Line::from(""));
                let id = self.id.text.trim();
                if !id.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled(" Folder  ", t::dim()),
                        Span::raw(folder),
                    ]));
                }
                if let Some(e) = error {
                    lines.push(Line::from(Span::styled(format!(" {e}"), t::error())));
                }
                t::draw_wrapped(buf, area, lines);
            }
            Stage::Running(r) => {
                let head = vec![
                    Line::from(Span::styled(
                        " A browser window opens. Finish the login there, then come back.",
                        t::title(),
                    )),
                    Line::from(Span::styled(
                        " If asked for a code, paste it below.",
                        t::dim(),
                    )),
                ];
                let hh = (head.len() as u16 + 1).min(area.height);
                t::draw_lines(buf, area, head);
                let term = Rect {
                    x: area.x + 1,
                    y: area.y + hh,
                    width: area.width.saturating_sub(2),
                    height: area.height.saturating_sub(hh),
                };
                if term.width < 2 || term.height < 2 {
                    return;
                }
                let size = (term.height, term.width);
                let mut parser = r.parser.lock().unwrap_or_else(|e| e.into_inner());
                if r.size != size {
                    r.size = size;
                    parser.screen_mut().set_size(size.0, size.1);
                    let _ = r.child.resize(size.0, size.1);
                }
                PseudoTerminal::new(parser.screen()).render(term, buf);
            }
            Stage::Success(id) => {
                let who = [id.display_name.clone(), id.email.clone()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ");
                let org = id
                    .org
                    .clone()
                    .map(|o| format!(" ({o})"))
                    .unwrap_or_default();
                let mut lines = vec![
                    Line::from(""),
                    Line::from(vec![
                        Span::styled(" ✓ Logged in as ", t::ok()),
                        Span::styled(who, t::bold()),
                        Span::raw(org),
                    ]),
                    Line::from(""),
                ];
                if self.is_new {
                    let next = match tool {
                        Tool::Claude => "the next session",
                        Tool::Codex => "the next agp codex session",
                    };
                    lines.push(Line::from(format!(
                        " {} is added to the pool. agent-prism can use it from {next}.",
                        self.label.text.trim()
                    )));
                }
                t::draw_wrapped(buf, area, lines);
            }
            Stage::Failed { error, tail } => {
                let mut lines = vec![
                    Line::from(""),
                    Line::from(Span::styled(format!(" ✕ {error}"), t::error())),
                    Line::from(""),
                ];
                for l in tail.iter() {
                    lines.push(Line::from(Span::styled(format!("   {l}"), t::dim())));
                }
                t::draw_wrapped(buf, area, lines);
            }
        }
    }
}

impl Drop for LoginView {
    fn drop(&mut self) {
        self.stop_child();
    }
}

fn screen_tail(parser: &Arc<Mutex<vt100::Parser>>) -> Vec<String> {
    let p = parser.lock().unwrap_or_else(|e| e.into_inner());
    let lines: Vec<String> = p
        .screen()
        .contents()
        .lines()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    lines[lines.len().saturating_sub(4)..].to_vec()
}

/// Encode a key as the bytes a terminal would send.
pub fn key_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    match key.code {
        KeyCode::Char(c) if ctrl => {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() {
                out.push(c as u8 - b'a' + 1);
            } else {
                return None;
            }
        }
        KeyCode::Char(c) => {
            let mut b = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        _ => return None,
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testutil::*;

    fn draw(v: &mut LoginView, w: u16, h: u16) -> String {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf);
        text(&buf)
    }

    fn typing(v: &mut LoginView, s: &str) {
        for c in s.chars() {
            v.handle_key(ch(c));
        }
    }

    fn fake(script: &str) -> Option<LoginCommand> {
        Some(LoginCommand {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
        })
    }

    fn wait(v: &mut LoginView) {
        for _ in 0..300 {
            v.tick();
            if !matches!(v.stage, Stage::Running(_)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("login did not finish");
    }

    const OK_SCRIPT: &str = r#"echo "Open this URL"; read x; printf '{"oauthAccount":{"displayName":"Neo","emailAddress":"neo@example.com","organizationName":"Zion"}}' > "$CLAUDE_CONFIG_DIR/.claude.json"; exit 0"#;

    #[test]
    fn slug_validation() {
        let f = Fixture::new();
        let mut v = LoginView::add(f.ctx.clone(), &config(), None, Some(Tool::Claude));
        assert!(v.validate().unwrap_err().contains("short id"));
        typing(&mut v, "Bad_Id");
        assert!(v.validate().unwrap_err().contains("lowercase"));
        v.id = TextField::new("a");
        assert!(v.validate().unwrap_err().contains("already exists"));
        v.id = TextField::new("c");
        std::fs::write(f.paths().home.join(".claude-c/x"), "").unwrap();
        assert!(v.validate().unwrap_err().contains("not empty"));
        v.id = TextField::new("new-1");
        assert!(v.validate().is_ok());
        for (w, h) in SIZES {
            assert!(draw(&mut v, w, h).contains("~/.claude-new-1"));
        }
    }

    #[test]
    fn add_with_fake_login() {
        let f = Fixture::new();
        let mut v = LoginView::add(
            f.ctx.clone(),
            &config(),
            Some("work".into()),
            Some(Tool::Claude),
        );
        v.command = fake(OK_SCRIPT);
        v.handle_key(key(KeyCode::Enter));
        typing(&mut v, "Work");
        v.handle_key(key(KeyCode::Enter));
        assert!(matches!(v.stage, Stage::Running(_)));
        let mut seen = false;
        for _ in 0..200 {
            if draw(&mut v, 80, 24).contains("Open this URL") {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(seen, "embedded output shown");
        assert!(draw(&mut v, 60, 14).contains("A browser window opens"));
        v.handle_key(key(KeyCode::Enter));
        wait(&mut v);
        for (w, h) in SIZES {
            let s = draw(&mut v, w, h);
            assert!(s.contains("Logged in as Neo neo@example.com (Zion)"), "{s}");
        }
        assert_eq!(v.handle_key(key(KeyCode::Enter)), FlowOutcome::Done(true));
        let done = v.result.clone().unwrap();
        assert_eq!(done.id, "work");
        assert_eq!(done.label, "Work");
        assert_eq!(done.dir, "~/.claude-work");
        assert!(done.profile_config().pool);
        assert!(f.paths().home.join(".claude-work/CLAUDE.md").exists());
    }

    const CODEX_SCRIPT: &str = r#"printf '{"auth_mode":"chatgpt","tokens":{"id_token":"eyJhbGciOiJub25lIn0.eyJlbWFpbCI6InRyaW5AZXhhbXBsZS5jb20iLCJuYW1lIjoiVHJpbml0eSIsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X3BsYW5fdHlwZSI6InBsdXMiLCJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2MtdCJ9fQ.x","access_token":"eyJhbGciOiJub25lIn0.eyJleHAiOjQxMDI0NDQ4MDB9.x","refresh_token":"r","account_id":"acc-t"}}' > "$CODEX_HOME/auth.json"; exit 0"#;

    #[test]
    fn add_codex_account() {
        let f = Fixture::new();
        std::fs::create_dir_all(f.paths().home.join(".codex/skills")).unwrap();
        let mut v = LoginView::add(f.ctx.clone(), &config(), Some("cx".into()), None);
        for (w, h) in SIZES {
            let s = draw(&mut v, w, h);
            assert!(s.contains("Add a Claude or Codex account"), "{s}");
        }
        v.handle_key(key(KeyCode::Right));
        assert!(draw(&mut v, 80, 24).contains("~/.codex-cx"));
        v.command = fake(CODEX_SCRIPT);
        v.handle_key(key(KeyCode::Enter));
        v.handle_key(key(KeyCode::Enter));
        v.handle_key(key(KeyCode::Enter));
        wait(&mut v);
        let s = draw(&mut v, 80, 24);
        assert!(s.contains("Logged in as Trinity trin@example.com"), "{s}");
        assert!(s.contains("agp codex"), "{s}");
        v.handle_key(key(KeyCode::Enter));
        let done = v.result.clone().unwrap();
        assert_eq!(done.dir, "~/.codex-cx");
        assert_eq!(done.identity.subscription.as_deref(), Some("plus"));
        assert_eq!(done.profile_config().tool, Tool::Codex);
        let skills = f.paths().home.join(".codex-cx/skills");
        assert!(skills.symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[test]
    fn codex_api_key_is_refused() {
        let f = Fixture::new();
        let mut v = LoginView::add(
            f.ctx.clone(),
            &config(),
            Some("k".into()),
            Some(Tool::Codex),
        );
        v.command = fake(r#"printf '{"OPENAI_API_KEY":"sk-x"}' > "$CODEX_HOME/auth.json""#);
        v.handle_key(key(KeyCode::Enter));
        v.handle_key(key(KeyCode::Enter));
        wait(&mut v);
        assert!(draw(&mut v, 80, 24).contains("API key"));
    }

    #[test]
    fn failure_offers_retry() {
        let f = Fixture::new();
        let mut v = LoginView::add(
            f.ctx.clone(),
            &config(),
            Some("z".into()),
            Some(Tool::Claude),
        );
        v.command = fake("echo nope; exit 3");
        v.handle_key(key(KeyCode::Enter));
        v.handle_key(key(KeyCode::Enter));
        wait(&mut v);
        for (w, h) in SIZES {
            let s = draw(&mut v, w, h);
            assert!(s.contains("exit code 3"), "{s}");
        }
        v.command = fake(OK_SCRIPT);
        v.handle_key(key(KeyCode::Enter));
        assert!(matches!(v.stage, Stage::Running(_)));
        v.handle_paste("x\n");
        wait(&mut v);
        assert!(matches!(v.stage, Stage::Success(_)));
    }

    #[test]
    fn esc_cancels_running_login() {
        let f = Fixture::new();
        let cfg = config();
        let p = cfg.profiles(f.paths())[0].clone();
        let mut v = LoginView::relogin(f.ctx.clone(), &cfg, &p, fake("read x"));
        assert!(matches!(v.stage, Stage::Running(_)));
        v.handle_key(key(KeyCode::Esc));
        assert!(draw(&mut v, 80, 24).contains("Login cancelled"));
        assert_eq!(v.handle_key(key(KeyCode::Esc)), FlowOutcome::Done(false));
    }

    #[test]
    fn keys_encode() {
        assert_eq!(key_bytes(&ctrl('c')), Some(vec![3]));
        assert_eq!(key_bytes(&key(KeyCode::Enter)), Some(vec![b'\r']));
        assert_eq!(key_bytes(&key(KeyCode::Up)), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes(&ch('é')), Some("é".as_bytes().to_vec()));
    }
}
