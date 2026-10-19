//! The menu the menu key opens inside the dock (DESIGN §10).

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use prism_core::model::{AccountView, DockMode, UsageStatus};
use prism_term::dock::account_color;
use prism_term::panel::{Panel, PanelOutcome};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::data::{now, usage_pcts};
use super::theme as t;
use super::{Tab, settings_panel};
use crate::actions::UiAction;
use crate::app::AppCtx;
use crate::live::Live;

/// The literal menu key byte (Ctrl+\).
pub const MENU_BYTE: u8 = 0x1c;

/// True for the menu key as crossterm or agent-prism's key parser report it.
pub fn is_menu_key(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4'))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Item {
    Switch,
    Settings,
    Accounts,
    Refresh,
    Dock,
    Log,
    Help,
    Forward,
}

const ITEMS: [(Item, &str, &str); 8] = [
    (Item::Switch, "w", "Switch account now ▸"),
    (Item::Settings, "s", "Settings"),
    (Item::Accounts, "a", "Accounts"),
    (Item::Refresh, "r", "Refresh usage"),
    (Item::Dock, "d", "Dock"),
    (Item::Log, "l", "Log"),
    (Item::Help, "?", "Help"),
    (Item::Forward, "⌃\\", "Send ⌃\\ to Claude"),
];

fn item_help(item: Item) -> &'static str {
    match item {
        Item::Switch => "Move this conversation to another account. It resumes there.",
        Item::Settings => "Accounts, balancing, dock, sharing.",
        Item::Accounts => "Add accounts, choose the pool, fix sharing.",
        Item::Refresh => "Fetch usage for all accounts now.",
        Item::Dock => "Cycle the dock: full · compact · hidden · auto.",
        Item::Log => "Recent agent-prism messages.",
        Item::Help => "Keys and what agent-prism does.",
        Item::Forward => "Claude gets a literal ⌃\\.",
    }
}

enum View {
    Main,
    Switch { sel: usize },
    Confirm { target: Option<String> },
    Log { from_bottom: usize },
    Help,
}

/// One row of the switch sub-list.
struct Target {
    target: Option<String>,
    line: Line<'static>,
    enabled: bool,
}

/// The in-dock menu.
pub struct MenuPanel {
    ctx: Arc<AppCtx>,
    live: Live,
    sel: usize,
    view: View,
    dock_mode: DockMode,
    flash: Option<String>,
}

impl MenuPanel {
    /// A menu reading `live`.
    pub fn new(ctx: Arc<AppCtx>, live: Live) -> Self {
        let dock_mode = live.lock().config.dock.mode;
        Self {
            ctx,
            live,
            sel: 0,
            view: View::Main,
            dock_mode,
            flash: None,
        }
    }

    /// The tool of this session (Claude when there is none).
    fn tool_name(&self) -> &'static str {
        self.live
            .lock()
            .session
            .as_ref()
            .map_or("Claude", |s| s.tool.name())
    }

    /// `text` with "Claude" changed to the tool of this session.
    fn named(&self, text: &str) -> String {
        text.replace("Claude", self.tool_name())
    }

    /// Why switching is not possible now, if it is not.
    fn switch_blocked(&self) -> Option<String> {
        let tool = self.tool_name();
        let d = self.live.lock();
        match &d.session {
            None => Some("No Claude or Codex session is running here.".into()),
            Some(s) if !s.can_switch => Some(format!(
                "Sessions are not shared yet — run agp adopt {} to share sessions.",
                s.profile_id
            )),
            Some(s) if s.session_id.is_none() => Some(format!(
                "The session id is not known yet — send {tool} a message first."
            )),
            _ => None,
        }
    }

    fn targets(&self) -> Vec<Target> {
        let d = self.live.lock();
        let blocked = self.switch_blocked_locked(&d);
        let current = d.session.as_ref().map(|s| s.profile_id.clone());
        let mut out = Vec::new();
        let label_of = |id: &str| {
            d.views
                .iter()
                .find(|v| v.profile_id == id)
                .map_or(id.to_string(), |v| v.label.clone())
        };
        let (best, best_ok) = match &d.pick {
            Some(p) => match &p.chosen {
                Some(id) => (format!("Best: {} — {}", label_of(id), p.reason), true),
                None => (format!("Best: none — {}", p.reason), false),
            },
            None => ("Best: waiting for usage…".to_string(), false),
        };
        out.push(Target {
            target: None,
            line: Line::from(Span::raw(best)),
            enabled: best_ok && blocked.is_none(),
        });
        for (i, v) in d.views.iter().enumerate().filter(|(_, v)| v.in_pool) {
            let is_current = current.as_deref() == Some(v.profile_id.as_str());
            out.push(Target {
                target: Some(v.profile_id.clone()),
                line: account_line(v, i, is_current),
                enabled: blocked.is_none() && !is_current && v.limited.is_none(),
            });
        }
        out
    }

    fn switch_blocked_locked(&self, d: &crate::live::LiveData) -> Option<String> {
        match &d.session {
            None => Some(String::new()),
            Some(s) if !s.can_switch || s.session_id.is_none() => Some(String::new()),
            _ => None,
        }
    }

    fn activate(&mut self, item: Item) -> PanelOutcome<UiAction> {
        self.flash = None;
        match item {
            Item::Switch => {
                self.view = View::Switch { sel: 0 };
                PanelOutcome::Continue
            }
            Item::Settings => PanelOutcome::Replace(settings_panel(
                self.ctx.clone(),
                self.live.clone(),
                Tab::Accounts,
            )),
            Item::Accounts => PanelOutcome::Replace(settings_panel(
                self.ctx.clone(),
                self.live.clone(),
                Tab::Accounts,
            )),
            Item::Refresh => {
                self.flash = Some("Refreshing usage…".into());
                PanelOutcome::Action(UiAction::RefreshUsage(None))
            }
            Item::Dock => {
                self.dock_mode = next_mode(self.dock_mode);
                PanelOutcome::Action(UiAction::SetDockMode(self.dock_mode))
            }
            Item::Log => {
                self.view = View::Log { from_bottom: 0 };
                PanelOutcome::Continue
            }
            Item::Help => {
                self.view = View::Help;
                PanelOutcome::Continue
            }
            Item::Forward => PanelOutcome::CloseAndForward(vec![MENU_BYTE]),
        }
    }

    /// The pending limit switch and the current account's label.
    fn pending(&self) -> Option<(crate::live::PendingSwitch, String)> {
        let d = self.live.lock();
        let s = d.session.as_ref()?;
        let p = s.pending.clone()?;
        let from = d
            .views
            .iter()
            .find(|v| v.profile_id == s.profile_id)
            .map_or_else(|| s.profile_id.clone(), |v| v.label.clone());
        Some((p, from))
    }

    /// Rows above the regular items: switch now / stay when a switch waits.
    fn extra(&self) -> usize {
        if self.pending().is_some() { 2 } else { 0 }
    }

    fn key_main(&mut self, key: KeyEvent) -> PanelOutcome<UiAction> {
        let extra = self.extra();
        let n = ITEMS.len() + extra;
        self.sel = self.sel.min(n - 1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => PanelOutcome::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.sel = (self.sel + n - 1) % n;
                PanelOutcome::Continue
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.sel = (self.sel + 1) % n;
                PanelOutcome::Continue
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => match self.sel {
                0 if extra > 0 => PanelOutcome::Action(UiAction::ConfirmSwitch),
                1 if extra > 0 => PanelOutcome::Action(UiAction::CancelSwitch),
                i => self.activate(ITEMS[i - extra].0),
            },
            KeyCode::Char(c) => match ITEMS.iter().position(|(_, k, _)| k.starts_with(c)) {
                Some(i) => {
                    self.sel = i + extra;
                    self.activate(ITEMS[i].0)
                }
                None => PanelOutcome::Continue,
            },
            _ => PanelOutcome::Continue,
        }
    }

    fn key_switch(&mut self, key: KeyEvent, sel: usize) -> PanelOutcome<UiAction> {
        let targets = self.targets();
        let n = targets.len().max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Left => self.view = View::Main,
            KeyCode::Up | KeyCode::Char('k') => {
                self.view = View::Switch {
                    sel: (sel + n - 1) % n,
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.view = View::Switch { sel: (sel + 1) % n },
            KeyCode::Enter => {
                let Some(t) = targets.get(sel) else {
                    return PanelOutcome::Continue;
                };
                if !t.enabled {
                    return PanelOutcome::Continue;
                }
                let busy = self.live.lock().session.as_ref().is_some_and(|s| s.busy);
                if busy {
                    self.view = View::Confirm {
                        target: t.target.clone(),
                    };
                } else {
                    return PanelOutcome::Action(UiAction::SwitchTo(t.target.clone()));
                }
            }
            _ => {}
        }
        PanelOutcome::Continue
    }

    fn render_main(&self, area: Rect, buf: &mut Buffer) {
        let wide = area.width >= 64;
        let mut lines = Vec::new();
        let mut extra = 0;
        if let Some((p, from)) = self.pending() {
            extra = 2;
            lines.push(Line::from(Span::styled(
                format!(" ⚠ {from} {}", p.reason),
                t::warn().add_modifier(ratatui::style::Modifier::BOLD),
            )));
            for (i, text) in [
                format!("⏎ Switch to {} now", p.target_label),
                format!("Stay on {from}"),
            ]
            .into_iter()
            .enumerate()
            {
                let sel = i == self.sel;
                lines.push(Line::from(vec![
                    t::marker(sel),
                    Span::styled(text, t::row(sel)),
                ]));
            }
            lines.push(Line::from(""));
        }
        for (i, (item, k, label)) in ITEMS.iter().enumerate() {
            let sel = i + extra == self.sel;
            let mut label = self.named(label);
            if *item == Item::Dock {
                label = format!("Dock: {}", mode_name(self.dock_mode));
            }
            let mut spans = vec![
                t::marker(sel),
                Span::styled(format!("{k:<3}"), Style::default().fg(t::ACCENT)),
                Span::styled(t::fit(&label, 26), t::row(sel)),
            ];
            if wide && sel {
                spans.push(Span::styled(self.named(item_help(*item)), t::dim()));
            }
            lines.push(Line::from(spans));
        }
        if let Some(f) = &self.flash {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(format!("  {f}"), t::dim())));
        }
        t::draw_lines(buf, area, lines);
    }

    fn render_switch(&self, area: Rect, buf: &mut Buffer, sel: usize) {
        let mut lines = vec![Line::from(Span::styled(
            " Switch this session to",
            t::bold(),
        ))];
        if let Some(reason) = self.switch_blocked() {
            lines.push(Line::from(Span::styled(format!(" {reason}"), t::warn())));
        }
        lines.push(Line::from(""));
        let targets = self.targets();
        let top = t::scroll_top(
            sel,
            targets.len(),
            (area.height as usize).saturating_sub(lines.len()),
        );
        for (i, tg) in targets.into_iter().enumerate().skip(top) {
            let s = i == sel;
            let mut spans = vec![t::marker(s)];
            let style = if !tg.enabled {
                t::dim()
            } else if s {
                t::selected()
            } else {
                Style::default()
            };
            for sp in tg.line.spans {
                spans.push(if tg.enabled && !s {
                    sp
                } else {
                    sp.patch_style(style)
                });
            }
            lines.push(Line::from(spans));
        }
        t::draw_lines(buf, area, lines);
    }

    fn render_log(&self, area: Rect, buf: &mut Buffer, from_bottom: usize) {
        let log = self.live.lock().log.clone();
        let h = area.height as usize;
        if log.is_empty() {
            t::draw_lines(
                buf,
                area,
                vec![Line::from(Span::styled(" No messages yet.", t::dim()))],
            );
            return;
        }
        let end = log
            .len()
            .saturating_sub(from_bottom.min(log.len().saturating_sub(1)));
        let start = end.saturating_sub(h);
        let lines = log[start..end]
            .iter()
            .map(|l| Line::from(format!(" {l}")))
            .collect();
        t::draw_lines(buf, area, lines);
    }

    fn render_help(&self, area: Rect, buf: &mut Buffer) {
        let k = |key: &str, what: &str| {
            Line::from(vec![
                Span::styled(format!("  {key:<10}"), Style::default().fg(t::ACCENT)),
                Span::raw(what.to_string()),
            ])
        };
        let lines = vec![
            Line::from(
                self.named(" agent-prism runs Claude on the account with the most usage left."),
            ),
            Line::from(" When an account hits a limit, the same conversation moves on."),
            Line::from(
                self.named(" The dock below Claude shows every account's 5h and weekly usage."),
            ),
            Line::from(""),
            k(
                "⌃\\",
                &self.named("open this menu · twice sends ⌃\\ to Claude"),
            ),
            k("↑↓ ⏎", "move and choose · letters jump"),
            k("esc", "back / close"),
            k("agp add", "add another account from your shell"),
        ];
        t::draw_wrapped(buf, area, lines);
    }
}

/// The next dock mode in the menu's cycle.
pub fn next_mode(m: DockMode) -> DockMode {
    match m {
        DockMode::Full => DockMode::Compact,
        DockMode::Compact => DockMode::Hidden,
        DockMode::Hidden => DockMode::Auto,
        DockMode::Auto => DockMode::Full,
    }
}

/// Name of a dock mode.
pub fn mode_name(m: DockMode) -> &'static str {
    match m {
        DockMode::Auto => "auto",
        DockMode::Full => "full",
        DockMode::Compact => "compact",
        DockMode::Hidden => "hidden",
    }
}

/// `● Acme Al   5h 48%  7d 31%  limit · resets 1h12m`
pub fn account_line(v: &AccountView, index: usize, current: bool) -> Line<'static> {
    let color = account_color(v.color.as_deref(), index);
    let (glyph, gstyle) = if current {
        ("●", Style::default().fg(color))
    } else if v.limited.is_some() {
        ("✕", t::error())
    } else if v.active_elsewhere > 0 {
        ("◐", Style::default().fg(color))
    } else if matches!(v.usage_status, UsageStatus::Stale { .. }) {
        ("⚠", t::dim())
    } else {
        ("○", Style::default().fg(color))
    };
    let (u5, u7) = usage_pcts(v);
    let mut spans = vec![
        Span::styled(format!("{glyph} "), gstyle),
        Span::raw(t::fit(&v.label, 16)),
        Span::styled("5h ", t::dim()),
        t::pct_span(u5),
        Span::styled("  7d ", t::dim()),
        t::pct_span(u7),
    ];
    let note = if current {
        Some(("  current".to_string(), t::dim()))
    } else if let Some(l) = &v.limited {
        Some((
            match l.until {
                Some(u) => format!(
                    "  limit · resets {}",
                    prism_term::dock::fmt_duration(u - now())
                ),
                None => "  limit".into(),
            },
            t::error(),
        ))
    } else {
        super::data::usage_note(v).map(|n| (format!("  {n}"), t::dim()))
    };
    if let Some((n, s)) = note {
        spans.push(Span::styled(n, s));
    }
    Line::from(spans)
}

impl Panel<UiAction> for MenuPanel {
    fn preferred_height(&self, rows: u16) -> u16 {
        (rows * 6 / 10).max(12)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let inner = t::frame(buf, area, "agent-prism");
        let (body, foot) = t::split_footer(inner);
        let body = Rect {
            y: body.y + u16::from(body.height > 9),
            height: body.height.saturating_sub(u16::from(body.height > 9)),
            ..body
        };
        let to_tool = format!("to {}", self.tool_name());
        let hints: &[(&str, &str)] = match self.view {
            View::Main => {
                self.render_main(body, buf);
                &[
                    ("↑↓", "move"),
                    ("⏎", "select"),
                    ("⌃\\", to_tool.as_str()),
                    ("esc", "close"),
                ]
            }
            View::Switch { sel } => {
                self.render_switch(body, buf, sel);
                &[("↑↓", "move"), ("⏎", "switch"), ("esc", "back")]
            }
            View::Confirm { ref target } => {
                let who = match target {
                    Some(id) => self
                        .live
                        .lock()
                        .views
                        .iter()
                        .find(|v| &v.profile_id == id)
                        .map_or(id.clone(), |v| v.label.clone()),
                    None => "the best account".into(),
                };
                t::draw_wrapped(
                    buf,
                    body,
                    vec![
                        Line::from(Span::styled(
                            self.named(" Claude is working — switch anyway?"),
                            t::warn(),
                        )),
                        Line::from(""),
                        Line::from(format!(
                            " The current turn stops, then the conversation resumes on {who}."
                        )),
                    ],
                );
                &[("y", "switch"), ("n", "wait")]
            }
            View::Log { from_bottom } => {
                self.render_log(body, buf, from_bottom);
                &[("↑↓", "scroll"), ("esc", "back")]
            }
            View::Help => {
                self.render_help(body, buf);
                &[("esc", "back")]
            }
        };
        t::draw_lines(buf, foot, vec![t::hints(hints)]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> PanelOutcome<UiAction> {
        if is_menu_key(&key) {
            return PanelOutcome::CloseAndForward(vec![MENU_BYTE]);
        }
        match self.view {
            View::Main => self.key_main(key),
            View::Switch { sel } => self.key_switch(key, sel),
            View::Confirm { ref target } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    let t = target.clone();
                    self.view = View::Main;
                    PanelOutcome::Action(UiAction::SwitchTo(t))
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.view = View::Switch { sel: 0 };
                    PanelOutcome::Continue
                }
                _ => PanelOutcome::Continue,
            },
            View::Log { from_bottom } => {
                let n = self.live.lock().log.len();
                let fb = match key.code {
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => {
                        self.view = View::Main;
                        return PanelOutcome::Continue;
                    }
                    KeyCode::Up | KeyCode::Char('k') => from_bottom + 1,
                    KeyCode::Down | KeyCode::Char('j') => from_bottom.saturating_sub(1),
                    KeyCode::PageUp => from_bottom + 10,
                    KeyCode::PageDown => from_bottom.saturating_sub(10),
                    KeyCode::End => 0,
                    _ => from_bottom,
                };
                self.view = View::Log {
                    from_bottom: fb.min(n.saturating_sub(1)),
                };
                PanelOutcome::Continue
            }
            View::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Left | KeyCode::Char('q')) {
                    self.view = View::Main;
                }
                PanelOutcome::Continue
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testutil::*;

    fn menu() -> (Fixture, MenuPanel) {
        let f = Fixture::new();
        let m = MenuPanel::new(f.ctx.clone(), live());
        (f, m)
    }

    #[test]
    fn renders_at_all_sizes() {
        let (_f, mut m) = menu();
        for (w, h) in SIZES {
            let s = render(&mut m, w, h);
            assert!(s.contains("Switch account now"), "{s}");
            assert!(s.contains("Refresh usage"));
        }
    }

    #[test]
    fn menu_key_twice_forwards() {
        let (_f, mut m) = menu();
        let k = KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL);
        assert!(matches!(m.handle_key(k), PanelOutcome::CloseAndForward(b) if b == vec![0x1c]));
        let k4 = KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL);
        assert!(matches!(m.handle_key(k4), PanelOutcome::CloseAndForward(_)));
    }

    #[test]
    fn esc_closes_and_letters_work() {
        let (_f, mut m) = menu();
        assert!(matches!(
            m.handle_key(ch('r')),
            PanelOutcome::Action(UiAction::RefreshUsage(None))
        ));
        assert!(matches!(
            m.handle_key(ch('d')),
            PanelOutcome::Action(UiAction::SetDockMode(DockMode::Hidden))
        ));
        assert!(matches!(
            m.handle_key(ch('d')),
            PanelOutcome::Action(UiAction::SetDockMode(DockMode::Auto))
        ));
        assert!(matches!(m.handle_key(ch('s')), PanelOutcome::Replace(_)));
        assert!(matches!(
            m.handle_key(key(KeyCode::Esc)),
            PanelOutcome::Close
        ));
    }

    #[test]
    fn arrows_and_enter() {
        let (_f, mut m) = menu();
        m.handle_key(key(KeyCode::Down));
        m.handle_key(key(KeyCode::Down));
        m.handle_key(key(KeyCode::Down));
        assert!(matches!(
            m.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Action(UiAction::RefreshUsage(None))
        ));
        m.handle_key(key(KeyCode::Up));
        m.handle_key(key(KeyCode::Up));
        m.handle_key(key(KeyCode::Up));
        m.handle_key(key(KeyCode::Up));
        assert_eq!(m.sel, ITEMS.len() - 1);
    }

    #[test]
    fn switch_list_and_pick() {
        let (_f, mut m) = menu();
        m.handle_key(ch('w'));
        for (w, h) in SIZES {
            let s = render(&mut m, w, h);
            assert!(s.contains("Best: Bob — most 5h headroom"), "{s}");
            assert!(s.contains("current"), "{s}");
        }
        assert!(matches!(
            m.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Action(UiAction::SwitchTo(None))
        ));
        m.handle_key(key(KeyCode::Down)); // Ann (current): disabled
        assert!(matches!(
            m.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Continue
        ));
        m.handle_key(key(KeyCode::Down));
        assert!(matches!(
            m.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Action(UiAction::SwitchTo(Some(id))) if id == "b"
        ));
        m.handle_key(key(KeyCode::Esc));
        assert!(matches!(m.view, View::Main));
    }

    #[test]
    fn switch_disabled_reasons() {
        let (_f, mut m) = menu();
        m.live.lock().session.as_mut().unwrap().can_switch = false;
        m.handle_key(ch('w'));
        let s = render(&mut m, 120, 40);
        assert!(s.contains("run agp adopt a to share sessions"), "{s}");
        assert!(matches!(
            m.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Continue
        ));
        m.live.lock().session = None;
        assert!(render(&mut m, 120, 40).contains("No Claude or Codex session"));
        m.live.lock().session = Some(crate::live::SessionInfo {
            tool: Default::default(),
            profile_id: "a".into(),
            session_id: None,
            busy: false,
            can_switch: true,
            pending: None,
        });
        assert!(render(&mut m, 120, 40).contains("session id is not known"));
    }

    #[test]
    fn busy_asks_first() {
        let (_f, mut m) = menu();
        m.live.lock().session.as_mut().unwrap().busy = true;
        m.handle_key(ch('w'));
        assert!(matches!(
            m.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Continue
        ));
        assert!(render(&mut m, 80, 24).contains("Claude is working — switch anyway?"));
        assert!(matches!(
            m.handle_key(ch('y')),
            PanelOutcome::Action(UiAction::SwitchTo(None))
        ));
    }

    #[test]
    fn log_and_help() {
        let (_f, mut m) = menu();
        m.handle_key(ch('l'));
        assert!(render(&mut m, 80, 24).contains("log line 29"));
        for _ in 0..40 {
            m.handle_key(key(KeyCode::Up));
        }
        let s = render(&mut m, 80, 24);
        assert!(
            s.contains("log line 0") && !s.contains("log line 29"),
            "{s}"
        );
        m.handle_key(key(KeyCode::Esc));
        m.handle_key(ch('?'));
        for (w, h) in SIZES {
            assert!(render(&mut m, w, h).contains("agent-prism runs Claude"));
        }
    }

    #[test]
    fn pending_switch_confirm_and_stay() {
        let (_f, mut m) = menu();
        m.live.lock().session.as_mut().unwrap().pending = Some(crate::live::PendingSwitch {
            target_id: "b".into(),
            target_label: "Bob".into(),
            reason: "hit the 5h limit".into(),
        });
        for (w, h) in SIZES {
            let s = render(&mut m, w, h);
            assert!(s.contains("Ann hit the 5h limit"), "{s}");
            assert!(
                s.contains("▸ ⏎ Switch to Bob now") && s.contains("Stay on Ann"),
                "{s}"
            );
        }
        assert!(matches!(
            m.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Action(UiAction::ConfirmSwitch)
        ));
        m.handle_key(key(KeyCode::Down));
        assert!(matches!(
            m.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Action(UiAction::CancelSwitch)
        ));
        m.handle_key(key(KeyCode::Down));
        assert!(matches!(
            m.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Continue
        ));
        assert!(matches!(m.view, View::Switch { .. }));
    }

    #[test]
    fn preferred_height() {
        let (_f, m) = menu();
        assert_eq!(m.preferred_height(10), 12);
        assert_eq!(m.preferred_height(50), 30);
    }
}
