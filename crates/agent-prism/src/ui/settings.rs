//! The settings panel (DESIGN §10): tabs Accounts · Balancing · Dock ·
//! Sharing · Keys · About. Works inside the dock and full screen. Every
//! change saves the config at once and reports `ConfigChanged`.

use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use prism_core::config::{Config, DockShow, ProfileConfig};
use prism_core::discover::{DirKind, DiscoveredDir, discover};
use prism_core::model::{AccountView, DockIcons, DockMode, OnLimit, Policy, Tool};
use prism_core::share::{ShareHealth, share_health};
use prism_term::dock::{account_color, render_dock};
use prism_term::panel::{Panel, PanelOutcome};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::Tab;
use super::adopt::{AdoptView, FlowOutcome, plan_for, plan_summary, tilde};
use super::data::{
    TextField, dock_model, label_of, login_state, pick_line, preview_pick, usage_pcts,
};
use super::login::{LoginCommand, LoginView};
use super::menu::{is_menu_key, mode_name};
use super::theme as t;
use crate::actions::UiAction;
use crate::app::AppCtx;
use crate::live::Live;

const TABS: [(Tab, &str); 6] = [
    (Tab::Accounts, "Accounts"),
    (Tab::Balancing, "Balancing"),
    (Tab::Dock, "Dock"),
    (Tab::Sharing, "Sharing"),
    (Tab::Keys, "Keys"),
    (Tab::About, "About"),
];

/// Colors offered for accounts, in cycle order.
pub const COLORS: [&str; 7] = ["cyan", "magenta", "blue", "green", "yellow", "red", "white"];

const POLICIES: [(Policy, &str, &str); 4] = [
    (
        Policy::Smart,
        "smart",
        "weighs 5h room, weekly room, plan and weight",
    ),
    (
        Policy::Priority,
        "priority",
        "fills accounts in list order, then spills over",
    ),
    (
        Policy::RoundRobin,
        "round-robin",
        "takes turns: least recently used first",
    ),
    (
        Policy::Headroom,
        "headroom",
        "the account with the most room right now",
    ),
];

const ON_LIMIT: [(OnLimit, &str, &str); 4] = [
    (OnLimit::Auto, "auto", "move on after a short countdown"),
    (OnLimit::Ask, "ask", "ask before moving"),
    (OnLimit::Stay, "stay", "only tell me"),
    (OnLimit::Wait, "wait", "wait for the reset, then go on"),
];

const MODES: [(DockMode, &str); 4] = [
    (DockMode::Auto, "full on tall terminals, else one line"),
    (DockMode::Full, "one row per account with bars"),
    (DockMode::Compact, "a single line"),
    (DockMode::Hidden, "nothing; the menu key still works"),
];

fn tab_index(tab: Tab) -> usize {
    TABS.iter().position(|(t, _)| *t == tab).unwrap_or(0)
}

/// A focusable field.
#[derive(Clone, Copy, Debug, PartialEq)]
enum F {
    Add,
    Account(usize),
    Found(usize),
    Pool(usize),
    Label(usize),
    Weight(usize),
    Reserve(usize),
    Color(usize),
    Fix(usize),
    Relogin(usize),
    Remove(usize),
    Policy(Policy),
    OnLimit(OnLimit),
    Countdown,
    ResumePrompt,
    Soft,
    Hard,
    Mode(DockMode),
    Show(DockShow),
    Projections,
    Icons(DockIcons),
    Share(usize),
    Backup,
    MenuKey,
}

struct Row {
    f: F,
    section: Option<&'static str>,
    spans: Vec<Span<'static>>,
    help: String,
}

impl Row {
    fn new(f: F, spans: Vec<Span<'static>>, help: impl Into<String>) -> Self {
        Row {
            f,
            section: None,
            spans,
            help: help.into(),
        }
    }

    fn in_section(mut self, s: &'static str) -> Self {
        self.section = Some(s);
        self
    }
}

enum Overlay {
    None,
    Edit(F, TextField),
    ConfirmRemove(usize),
    Adopt(Box<AdoptView>),
    Login(Box<LoginView>),
}

/// The settings panel.
pub struct SettingsPanel {
    ctx: Arc<AppCtx>,
    live: Live,
    tab: Tab,
    config: Config,
    seen: Config,
    sel: [usize; 6],
    detail: Option<usize>,
    detail_sel: usize,
    overlay: Overlay,
    found: Vec<DiscoveredDir>,
    health: Vec<Result<(ShareHealth, String), String>>,
    message: Option<(String, bool)>,
    /// Login command override (tests).
    pub login_cmd: Option<LoginCommand>,
    /// Close instead of going back when Esc is pressed at the top level.
    full_screen_hint: bool,
    /// Clock for the About tab animation.
    started: Instant,
}

fn label_value(name: &str, w: usize) -> Span<'static> {
    Span::raw(t::fit(name, w))
}

impl SettingsPanel {
    /// A settings panel on `tab`.
    pub fn new(ctx: Arc<AppCtx>, live: Live, tab: Tab) -> Self {
        let config = live.lock().config.clone();
        let mut p = Self {
            ctx,
            live,
            tab,
            seen: config.clone(),
            config,
            sel: [0; 6],
            detail: None,
            detail_sel: 0,
            overlay: Overlay::None,
            found: Vec::new(),
            health: Vec::new(),
            message: None,
            login_cmd: None,
            full_screen_hint: false,
            started: Instant::now(),
        };
        p.refresh_found();
        p.refresh_health();
        p
    }

    /// Mark the panel as the full-screen app (changes the Esc hint only).
    pub fn set_full_screen(&mut self) {
        self.full_screen_hint = true;
    }

    fn refresh_found(&mut self) {
        let configured: Vec<_> = self
            .config
            .profiles(&self.ctx.paths)
            .into_iter()
            .map(|p| p.dir)
            .collect();
        self.found = discover(&self.ctx.paths, &self.config, self.ctx.store.as_ref())
            .into_iter()
            .filter(|d| d.kind != DirKind::Proxy && !configured.contains(&d.dir))
            .collect();
    }

    /// The common home of `tool` as shown, e.g. `~/.codex`.
    fn common_text(&self, tool: Tool) -> String {
        tilde(
            &self.ctx,
            &self.config.common_home_for(&self.ctx.paths, tool),
        )
    }

    fn refresh_health(&mut self) {
        self.health = self
            .config
            .profiles(&self.ctx.paths)
            .iter()
            .map(|p| {
                let h = share_health(&p.dir, &p.common_home, &self.config.share_rules(p.tool))
                    .map_err(|e| format!("{e:#}"))?;
                let plan = if h.to_fix > 0 {
                    plan_for(&self.ctx, &self.config, p)
                        .map(|p| plan_summary(&p))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                Ok((h, plan))
            })
            .collect();
    }

    fn view_for(&self, id: &str) -> Option<AccountView> {
        self.live
            .lock()
            .views
            .iter()
            .find(|v| v.profile_id == id)
            .cloned()
    }

    fn save(&mut self, what: &str) -> PanelOutcome<UiAction> {
        match self.config.save(&self.ctx.paths) {
            Ok(()) => {
                self.message = Some((format!("Saved · {what}"), false));
                PanelOutcome::Action(UiAction::ConfigChanged)
            }
            Err(e) => {
                self.message = Some((format!("Could not save: {e:#}"), true));
                PanelOutcome::Continue
            }
        }
    }

    fn health_text(&self, i: usize) -> (String, Style) {
        let is_default = self
            .config
            .profiles(&self.ctx.paths)
            .get(i)
            .is_some_and(|p| p.is_default_install);
        match self.health.get(i) {
            _ if is_default => ("common home".into(), t::dim()),
            Some(Ok((h, _))) if h.to_fix == 0 && h.sessions_shared => ("✓ shared".into(), t::ok()),
            Some(Ok((h, _))) if h.to_fix == 0 => ("sessions not shared".into(), t::warn()),
            Some(Ok((h, plan))) => {
                let mut s = format!("{} to fix", h.to_fix);
                if !plan.is_empty() {
                    s = plan.clone();
                }
                if !h.sessions_shared {
                    s.push_str(" · sessions not shared");
                }
                (s, t::warn())
            }
            Some(Err(e)) => (e.clone(), t::error()),
            None => ("—".into(), t::dim()),
        }
    }

    // ----- rows per tab -------------------------------------------------

    fn rows(&self, width: u16) -> Vec<Row> {
        match self.tab {
            Tab::Accounts => match self.detail {
                Some(i) => self.detail_rows(i),
                None => self.account_rows(width),
            },
            Tab::Balancing => self.balancing_rows(),
            Tab::Dock => self.dock_rows(),
            Tab::Sharing => self.sharing_rows(),
            Tab::Keys => vec![Row::new(
                F::MenuKey,
                vec![label_value("Menu key", 16), Span::raw("⌃\\  (ctrl+\\)")],
                "Opens the menu. Press it twice to send ⌃\\ to Claude or Codex. Changing it is not supported yet.",
            )],
            Tab::About => Vec::new(),
        }
    }

    fn account_rows(&self, width: u16) -> Vec<Row> {
        let mut rows = vec![Row::new(
            F::Add,
            vec![Span::styled(
                "+ Add account",
                Style::default().fg(t::ACCENT).add_modifier(Modifier::BOLD),
            )],
            "Log in to another Claude or ChatGPT (Codex) account. It joins the pool.",
        )];
        let wide = width >= 96;
        let any_codex = self.config.profiles.iter().any(|p| p.tool == Tool::Codex);
        for (i, p) in self.config.profiles.iter().enumerate() {
            let v = self.view_for(&p.id);
            let color = account_color(p.color.as_deref(), i);
            let mut spans = vec![
                Span::styled("● ", Style::default().fg(color)),
                Span::raw(t::fit(&label_of(p), 16)),
            ];
            if any_codex {
                spans.push(Span::styled(format!("{:<7}", p.tool.name()), t::dim()));
            }
            if wide {
                let email = v
                    .as_ref()
                    .and_then(|v| v.identity.email.clone())
                    .unwrap_or_default();
                let plan = v
                    .as_ref()
                    .and_then(|v| v.identity.subscription.clone())
                    .unwrap_or_default();
                spans.push(Span::styled(t::fit(&email, 26), t::dim()));
                spans.push(Span::styled(t::fit(&plan, 6), t::dim()));
            }
            let (state, good) = v
                .as_ref()
                .map_or(("unknown", false), |v| login_state(&v.token));
            if width >= 72 {
                spans.push(Span::styled(
                    t::fit(state, 17),
                    if good { t::dim() } else { t::error() },
                ));
            }
            spans.push(t::check(p.pool));
            spans.push(Span::styled(" pool  ", t::dim()));
            let (u5, u7) = v.as_ref().map(usage_pcts).unwrap_or((None, None));
            spans.extend([
                Span::styled("5h ", t::dim()),
                t::pct_span(u5),
                Span::styled("  7d ", t::dim()),
                t::pct_span(u7),
            ]);
            rows.push(Row::new(
                F::Account(i),
                spans,
                "⏎ edit · ␣ in pool or not. Accounts out of the pool only run with agp --as.",
            ));
        }
        for (i, d) in self.found.iter().enumerate() {
            let who = d
                .identity
                .email
                .clone()
                .unwrap_or_else(|| d.suggested_label.clone());
            let state = if d.kind == DirKind::LoggedIn {
                ""
            } else {
                " · not logged in"
            };
            rows.push(Row::new(
                F::Found(i),
                vec![
                    Span::styled("+ Found: ", Style::default().fg(t::ACCENT)),
                    Span::raw(format!("{who} ({}){state}", tilde(&self.ctx, &d.dir))),
                    Span::styled(" — add", t::dim()),
                ],
                format!(
                    "A {} folder agent-prism does not use yet. ⏎ adds it to agent-prism.",
                    d.tool.name()
                ),
            ));
        }
        rows
    }

    fn detail_rows(&self, i: usize) -> Vec<Row> {
        let Some(p) = self.config.profiles.get(i) else {
            return Vec::new();
        };
        let w = 16;
        let color_name = p.color.clone().unwrap_or_else(|| "auto".to_string());
        let (health, hstyle) = self.health_text(i);
        vec![
            Row::new(
                F::Pool(i),
                vec![label_value("In pool", w), t::check(p.pool)],
                "Pool accounts are balanced across. ␣ toggles.",
            ),
            Row::new(
                F::Label(i),
                vec![label_value("Label", w), Span::raw(label_of(p))],
                "The name in the dock and menus. ⏎ edits.",
            ),
            Row::new(
                F::Weight(i),
                vec![
                    label_value("Weight", w),
                    Span::raw(format!("{:.1}", p.weight)),
                ],
                "How strongly smart balancing prefers this account (0.1–5). +/- change.",
            ),
            Row::new(
                F::Reserve(i),
                vec![
                    label_value("Weekly reserve", w),
                    Span::raw(format!("{}%", p.reserve_7d)),
                ],
                "Weekly % agent-prism leaves unused, e.g. for someone else (0–90). +/- change.",
            ),
            Row::new(
                F::Color(i),
                vec![
                    label_value("Color", w),
                    Span::styled(
                        "██ ",
                        Style::default().fg(account_color(p.color.as_deref(), i)),
                    ),
                    Span::raw(color_name),
                ],
                "Dock color. ⏎ or +/- cycle.",
            ),
            Row::new(
                F::Fix(i),
                vec![label_value("Fix sharing…", w), Span::styled(health, hstyle)],
                format!(
                    "Share sessions, settings and memory with {}. Shows the plan first.",
                    self.common_text(p.tool)
                ),
            ),
            Row::new(
                F::Relogin(i),
                vec![label_value("Log in again", w)],
                format!("Run the {} login for this account's folder.", p.tool.name()),
            ),
            Row::new(
                F::Remove(i),
                vec![Span::styled("Remove from agent-prism", t::error())],
                "Removes the entry from agent-prism's config. The folder and login stay.",
            ),
        ]
    }

    fn balancing_rows(&self) -> Vec<Row> {
        let b = &self.config.balance;
        let mut rows = Vec::new();
        for (i, (p, name, desc)) in POLICIES.iter().enumerate() {
            let r = Row::new(
                F::Policy(*p),
                vec![
                    t::radio(b.policy == *p),
                    Span::raw(format!(" {}", t::fit(name, 13))),
                    Span::styled(*desc, t::dim()),
                ],
                format!("New sessions go to the account {name} picks."),
            );
            rows.push(if i == 0 { r.in_section("Policy") } else { r });
        }
        for (i, (o, name, desc)) in ON_LIMIT.iter().enumerate() {
            let r = Row::new(
                F::OnLimit(*o),
                vec![
                    t::radio(b.on_limit == *o),
                    Span::raw(format!(" {}", t::fit(name, 13))),
                    Span::styled(*desc, t::dim()),
                ],
                "What agent-prism does when the account in use hits a limit.",
            );
            rows.push(if i == 0 {
                r.in_section("When a limit hits")
            } else {
                r
            });
        }
        let w = 18;
        rows.push(
            Row::new(
                F::Countdown,
                vec![
                    label_value("Countdown", w),
                    Span::raw(format!("{}s", b.countdown_secs)),
                ],
                "Seconds before an automatic switch (0–60). +/- change.",
            )
            .in_section("Details"),
        );
        rows.push(Row::new(
            F::ResumePrompt,
            vec![
                label_value("Resume prompt", w),
                if b.resume_prompt.is_empty() {
                    Span::styled("(none)", t::dim())
                } else {
                    Span::raw(format!("\"{}\"", b.resume_prompt))
                },
            ],
            "Sent to Claude after a limit switch so it carries on. ⏎ edits; empty = none.",
        ));
        rows.push(Row::new(
            F::Soft,
            vec![
                label_value("Avoid 5h above", w),
                Span::raw(format!("{:.0}%", b.soft_5h)),
            ],
            "New sessions avoid accounts at or over this 5h usage. +/- change.",
        ));
        rows.push(Row::new(
            F::Hard,
            vec![
                label_value("Limited at", w),
                Span::raw(format!("{:.0}%", b.hard)),
            ],
            "At or over this usage an account counts as limited. +/- change.",
        ));
        rows
    }

    fn dock_rows(&self) -> Vec<Row> {
        let d = &self.config.dock;
        let mut rows = Vec::new();
        for (i, (m, desc)) in MODES.iter().enumerate() {
            let r = Row::new(
                F::Mode(*m),
                vec![
                    t::radio(d.mode == *m),
                    Span::raw(format!(" {}", t::fit(mode_name(*m), 10))),
                    Span::styled(*desc, t::dim()),
                ],
                "How much room the dock takes below Claude.",
            );
            rows.push(if i == 0 { r.in_section("Mode") } else { r });
        }
        rows.push(
            Row::new(
                F::Show(DockShow::Pool),
                vec![
                    t::radio(d.show == DockShow::Pool),
                    Span::raw(" pool accounts"),
                ],
                "Which accounts the dock lists.",
            )
            .in_section("Show"),
        );
        rows.push(Row::new(
            F::Show(DockShow::All),
            vec![
                t::radio(d.show == DockShow::All),
                Span::raw(" all accounts"),
            ],
            "Which accounts the dock lists.",
        ));
        rows.push(Row::new(
            F::Projections,
            vec![
                t::check(d.projections),
                Span::raw(" projections  "),
                Span::styled("48%→71%", t::dim()),
            ],
            "Show where usage is heading at the next reset, from recent use.",
        ));
        rows.push(
            Row::new(
                F::Icons(DockIcons::Unicode),
                vec![
                    t::radio(d.icons == DockIcons::Unicode),
                    Span::raw(" unicode    "),
                    Span::styled("● ◐ ○ ⚠ ✕ Σ", t::dim()),
                ],
                "Icons that every terminal font can show.",
            )
            .in_section("Icons"),
        );
        rows.push(Row::new(
            F::Icons(DockIcons::Nerd),
            vec![
                t::radio(d.icons == DockIcons::Nerd),
                Span::raw(" nerd font  "),
                Span::styled("\u{f192} ◐ \u{f10c} \u{f127} \u{f252} \u{f1c0}", t::dim()),
            ],
            "Nerd Font icons. Your terminal must use a Nerd Font.",
        ));
        rows
    }

    fn sharing_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (i, p) in self.config.profiles(&self.ctx.paths).iter().enumerate() {
            let (h, style) = self.health_text(i);
            let unmanaged = match self.health.get(i) {
                Some(Ok((h, _))) if !h.unmanaged.is_empty() => {
                    format!("  · {} left alone", h.unmanaged.len())
                }
                _ => String::new(),
            };
            let r = Row::new(
                F::Share(i),
                vec![
                    label_value(&p.label, 16),
                    Span::styled(t::fit(&tilde(&self.ctx, &p.dir), 18), t::dim()),
                    Span::styled(h, style),
                    Span::styled(unmanaged, t::dim()),
                ],
                "⏎ shows what Fix sharing would change. Nothing changes until you confirm.",
            );
            rows.push(if i == 0 { r.in_section("Accounts") } else { r });
        }
        rows.push(
            Row::new(
                F::Backup,
                vec![Span::raw(self.config.backup.target.clone())],
                "Backups before sharing changes go here: a folder or ssh://host/path. ⏎ edits.",
            )
            .in_section("Backup target"),
        );
        rows
    }

    // ----- actions ------------------------------------------------------

    fn profile(&self, i: usize) -> Option<prism_core::profile::Profile> {
        self.config.profiles(&self.ctx.paths).get(i).cloned()
    }

    fn activate(&mut self, f: F, step: i32) -> PanelOutcome<UiAction> {
        self.message = None;
        let enter = step == 0;
        match f {
            F::Add if enter => {
                let mut v = LoginView::add(self.ctx.clone(), &self.config, None, None);
                v.command = self.login_cmd.clone();
                self.overlay = Overlay::Login(Box::new(v));
            }
            F::Account(i) if enter => {
                self.detail = Some(i);
                self.detail_sel = 0;
            }
            F::Found(i) if enter => return self.add_found(i),
            F::Pool(i) if enter => {
                if let Some(p) = self.config.profiles.get_mut(i) {
                    p.pool = !p.pool;
                    let what = format!(
                        "{} {}",
                        label_of(p),
                        if p.pool {
                            "in the pool"
                        } else {
                            "out of the pool"
                        }
                    );
                    return self.save(&what);
                }
            }
            F::Label(i) if enter => {
                let cur = self
                    .config
                    .profiles
                    .get(i)
                    .map(label_of)
                    .unwrap_or_default();
                self.overlay = Overlay::Edit(f, TextField::new(&cur));
            }
            F::Weight(i) => {
                if let Some(p) = self.config.profiles.get_mut(i) {
                    let w = ((p.weight * 10.0).round() + f64::from(step)) / 10.0;
                    p.weight = w.clamp(0.1, 5.0);
                    if step != 0 {
                        return self.save("weight");
                    }
                    let cur = format!("{:.1}", p.weight);
                    self.overlay = Overlay::Edit(f, TextField::new(&cur));
                }
            }
            F::Reserve(i) => {
                if let Some(p) = self.config.profiles.get_mut(i) {
                    if step != 0 {
                        p.reserve_7d = (i32::from(p.reserve_7d) + step * 5).clamp(0, 90) as u8;
                        return self.save("weekly reserve");
                    }
                    let cur = p.reserve_7d.to_string();
                    self.overlay = Overlay::Edit(f, TextField::new(&cur));
                }
            }
            F::Color(i) => {
                if let Some(p) = self.config.profiles.get_mut(i) {
                    let cur = p
                        .color
                        .as_deref()
                        .and_then(|c| COLORS.iter().position(|x| *x == c));
                    let n = COLORS.len() as i32;
                    let next = match cur {
                        Some(c) => (c as i32 + if step < 0 { -1 } else { 1 }).rem_euclid(n),
                        None => 0,
                    };
                    p.color = Some(COLORS[next as usize].to_string());
                    return self.save("color");
                }
            }
            F::Fix(i) | F::Share(i) if enter => {
                if let Some(p) = self.profile(i) {
                    if p.is_default_install {
                        self.message =
                            Some(("This is the common home; nothing to share.".into(), false));
                    } else {
                        self.overlay = Overlay::Adopt(Box::new(AdoptView::new(
                            self.ctx.clone(),
                            &self.config,
                            p,
                        )));
                    }
                }
            }
            F::Relogin(i) if enter => {
                if let Some(p) = self.profile(i) {
                    let v = LoginView::relogin(
                        self.ctx.clone(),
                        &self.config,
                        &p,
                        self.login_cmd.clone(),
                    );
                    self.overlay = Overlay::Login(Box::new(v));
                }
            }
            F::Remove(i) if enter => self.overlay = Overlay::ConfirmRemove(i),
            F::Policy(p) if enter => {
                self.config.balance.policy = p;
                return self.save("policy");
            }
            F::OnLimit(o) if enter => {
                self.config.balance.on_limit = o;
                return self.save("limit behavior");
            }
            F::Countdown => {
                if step == 0 {
                    let cur = self.config.balance.countdown_secs.to_string();
                    self.overlay = Overlay::Edit(f, TextField::new(&cur));
                } else {
                    let v = (self.config.balance.countdown_secs as i32 + step).clamp(0, 60);
                    self.config.balance.countdown_secs = v as u32;
                    return self.save("countdown");
                }
            }
            F::Soft | F::Hard => {
                let v = if f == F::Soft {
                    self.config.balance.soft_5h
                } else {
                    self.config.balance.hard
                };
                if step == 0 {
                    self.overlay = Overlay::Edit(f, TextField::new(&format!("{v:.0}")));
                } else {
                    return self.set_number(f, v + f64::from(step));
                }
            }
            F::ResumePrompt if enter => {
                let cur = self.config.balance.resume_prompt.clone();
                self.overlay = Overlay::Edit(f, TextField::new(&cur));
            }
            F::Mode(m) if enter => {
                self.config.dock.mode = m;
                return self.save("dock mode");
            }
            F::Show(s) if enter => {
                self.config.dock.show = s;
                return self.save("dock accounts");
            }
            F::Projections if enter => {
                self.config.dock.projections = !self.config.dock.projections;
                return self.save("projections");
            }
            F::Icons(i) if enter => {
                self.config.dock.icons = i;
                return self.save("dock icons");
            }
            F::Backup if enter => {
                let cur = self.config.backup.target.clone();
                self.overlay = Overlay::Edit(f, TextField::new(&cur));
            }
            F::MenuKey if enter => {
                self.message = Some(("Changing the menu key is not supported yet.".into(), false));
            }
            _ => {}
        }
        PanelOutcome::Continue
    }

    fn set_number(&mut self, f: F, v: f64) -> PanelOutcome<UiAction> {
        let b = &mut self.config.balance;
        match f {
            F::Soft => b.soft_5h = v.round().clamp(50.0, 100.0),
            F::Hard => b.hard = v.round().clamp(50.0, 100.0),
            F::Countdown => b.countdown_secs = v.round().clamp(0.0, 60.0) as u32,
            F::Weight(i) => {
                if let Some(p) = self.config.profiles.get_mut(i) {
                    p.weight = ((v * 10.0).round() / 10.0).clamp(0.1, 5.0);
                }
            }
            F::Reserve(i) => {
                if let Some(p) = self.config.profiles.get_mut(i) {
                    p.reserve_7d = v.round().clamp(0.0, 90.0) as u8;
                }
            }
            _ => return PanelOutcome::Continue,
        }
        self.save("value")
    }

    fn commit_edit(&mut self, f: F, text: String) -> PanelOutcome<UiAction> {
        let text = text.trim().to_string();
        match f {
            F::Label(i) => {
                if let Some(p) = self.config.profiles.get_mut(i) {
                    p.label = text;
                    return self.save("label");
                }
                PanelOutcome::Continue
            }
            F::ResumePrompt => {
                self.config.balance.resume_prompt = text;
                self.save("resume prompt")
            }
            F::Backup => {
                if text.is_empty() {
                    self.message = Some(("The backup target cannot be empty.".into(), true));
                    return PanelOutcome::Continue;
                }
                self.config.backup.target = text;
                self.save("backup target")
            }
            _ => match text.trim_end_matches(['%', 's']).parse::<f64>() {
                Ok(v) => self.set_number(f, v),
                Err(_) => {
                    self.message = Some((format!("{text:?} is not a number."), true));
                    PanelOutcome::Continue
                }
            },
        }
    }

    fn add_found(&mut self, i: usize) -> PanelOutcome<UiAction> {
        let Some(d) = self.found.get(i).cloned() else {
            return PanelOutcome::Continue;
        };
        let id = unique_id(&self.config, &d.suggested_id);
        let label = d.suggested_label.clone();
        self.config.profiles.push(ProfileConfig {
            id,
            label: label.clone(),
            dir: tilde(&self.ctx, &d.dir),
            tool: d.tool,
            pool: d.kind == DirKind::LoggedIn && !d.is_default_install,
            ..Default::default()
        });
        self.refresh_found();
        self.refresh_health();
        self.save(&format!("added {label}"))
    }

    fn finish_overlay_login(&mut self, v: &LoginView) -> PanelOutcome<UiAction> {
        let Some(done) = v.result.clone() else {
            return PanelOutcome::Continue;
        };
        self.refresh_found();
        if done.is_new {
            self.config.profiles.push(done.profile_config());
            self.refresh_health();
            self.save(&format!("added {}", done.label))
        } else {
            self.message = Some((format!("{} is logged in again", done.label), false));
            PanelOutcome::Action(UiAction::ConfigChanged)
        }
    }

    fn switch_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.detail = None;
        self.message = None;
        if tab == Tab::Sharing {
            self.refresh_health();
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> Option<PanelOutcome<UiAction>> {
        let overlay = std::mem::replace(&mut self.overlay, Overlay::None);
        let (overlay, out) = match overlay {
            Overlay::None => return None,
            Overlay::Edit(f, mut field) => match key.code {
                KeyCode::Esc => (Overlay::None, PanelOutcome::Continue),
                KeyCode::Enter => {
                    let out = self.commit_edit(f, field.text.clone());
                    (Overlay::None, out)
                }
                _ => {
                    field.handle_key(key);
                    (Overlay::Edit(f, field), PanelOutcome::Continue)
                }
            },
            Overlay::ConfirmRemove(i) => match key.code {
                KeyCode::Char('y') => {
                    let out = if i < self.config.profiles.len() {
                        let p = self.config.profiles.remove(i);
                        self.detail = None;
                        self.sel[0] = 0;
                        self.refresh_found();
                        self.refresh_health();
                        self.save(&format!("removed {} (folder kept)", label_of(&p)))
                    } else {
                        PanelOutcome::Continue
                    };
                    (Overlay::None, out)
                }
                KeyCode::Char('n') | KeyCode::Esc => (Overlay::None, PanelOutcome::Continue),
                _ => (Overlay::ConfirmRemove(i), PanelOutcome::Continue),
            },
            Overlay::Adopt(mut v) => match v.handle_key(key) {
                FlowOutcome::Continue => (Overlay::Adopt(v), PanelOutcome::Continue),
                FlowOutcome::Done(_) => {
                    self.refresh_health();
                    (Overlay::None, PanelOutcome::Continue)
                }
            },
            Overlay::Login(mut v) => match v.handle_key(key) {
                FlowOutcome::Continue => (Overlay::Login(v), PanelOutcome::Continue),
                FlowOutcome::Done(_) => {
                    let out = self.finish_overlay_login(&v);
                    (Overlay::None, out)
                }
            },
        };
        self.overlay = overlay;
        Some(out)
    }

    // ----- rendering ----------------------------------------------------

    fn render_tabs(&self, area: Rect, buf: &mut Buffer) {
        let mut spans = vec![Span::raw(" ")];
        for (i, (tab, name)) in TABS.iter().enumerate() {
            let on = *tab == self.tab;
            let label = if area.width >= 70 {
                format!(" {} {name} ", i + 1)
            } else {
                format!(" {name} ")
            };
            spans.push(Span::styled(
                label,
                if on {
                    t::selected().add_modifier(Modifier::REVERSED)
                } else {
                    t::dim()
                },
            ));
        }
        t::draw_lines(buf, area, vec![Line::from(spans)]);
    }

    fn render_rows(
        &self,
        rows: &[Row],
        sel: usize,
        area: Rect,
        buf: &mut Buffer,
        head: Vec<Line<'static>>,
    ) {
        let mut lines = head;
        let mut sel_line = 0;
        for (i, r) in rows.iter().enumerate() {
            if let Some(s) = r.section {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(Span::styled(format!(" {s}"), t::bold())));
            }
            if i == sel {
                sel_line = lines.len();
            }
            let on = i == sel;
            let mut spans = vec![Span::raw(" "), t::marker(on)];
            let editing = matches!(&self.overlay, Overlay::Edit(f, _) if on && *f == r.f);
            for (k, sp) in r.spans.iter().enumerate() {
                if editing && k == r.spans.len() - 1 && r.spans.len() > 1 {
                    break;
                }
                spans.push(if on && k == 0 {
                    sp.clone().patch_style(t::selected())
                } else {
                    sp.clone()
                });
            }
            if editing && let Overlay::Edit(_, field) = &self.overlay {
                if r.spans.len() == 1 {
                    spans.truncate(2);
                }
                spans.extend(field.spans(true));
            }
            lines.push(Line::from(spans));
        }
        let top = t::scroll_top(sel_line, lines.len(), area.height as usize);
        t::draw_lines(buf, area, lines.into_iter().skip(top).collect());
    }

    fn render_body(&mut self, area: Rect, buf: &mut Buffer) -> String {
        let rows = self.rows(area.width);
        let ti = tab_index(self.tab);
        let sel = if self.detail.is_some() {
            self.detail_sel
        } else {
            self.sel[ti]
        };
        let sel = sel.min(rows.len().saturating_sub(1));
        let help = rows.get(sel).map(|r| r.help.clone()).unwrap_or_default();
        match self.tab {
            Tab::Accounts => {
                let head = match self
                    .detail
                    .and_then(|i| self.config.profiles.get(i).map(|p| (i, p)))
                {
                    Some((_, p)) => {
                        let v = self.view_for(&p.id);
                        let facts = v
                            .map(|v| {
                                [v.identity.email, v.identity.org, v.identity.subscription]
                                    .into_iter()
                                    .flatten()
                                    .collect::<Vec<_>>()
                                    .join(" · ")
                            })
                            .unwrap_or_default();
                        vec![
                            Line::from(vec![
                                Span::styled(" Accounts › ", t::dim()),
                                Span::styled(label_of(p), t::title()),
                                Span::styled(format!("  {facts}"), t::dim()),
                            ]),
                            Line::from(""),
                        ]
                    }
                    None => vec![],
                };
                self.render_rows(&rows, sel, area, buf, head);
            }
            Tab::Balancing => {
                let (body, prev) = split_bottom(area, 2);
                self.render_rows(&rows, sel, body, buf, vec![]);
                let views = self.live.lock().views.clone();
                let pick = preview_pick(&self.config, &views);
                let (label, reason) = pick_line(&self.config, &pick);
                let line = Line::from(vec![
                    Span::styled(" Next session → ", t::bold()),
                    Span::styled(label.unwrap_or_else(|| "none".into()), t::title()),
                    Span::styled(format!(" — {reason}"), t::dim()),
                ]);
                t::draw_lines(buf, prev, vec![Line::from(""), line]);
            }
            Tab::Dock => {
                let views = self.live.lock().views.clone();
                let shown = views
                    .iter()
                    .filter(|v| self.config.dock.show == DockShow::All || v.in_pool)
                    .count() as u16;
                let full_h = shown + 2;
                let mode = match self.config.dock.mode {
                    DockMode::Auto if area.height >= full_h + rows.len() as u16 + 8 => {
                        DockMode::Full
                    }
                    DockMode::Auto => DockMode::Compact,
                    m => m,
                };
                let ph = match mode {
                    DockMode::Full => full_h,
                    _ => 1,
                };
                let (body, prev) = split_bottom(area, ph + 2);
                self.render_rows(&rows, sel, body, buf, vec![]);
                if prev.height >= 2 {
                    t::draw_lines(
                        buf,
                        prev,
                        vec![
                            Line::from(""),
                            Line::from(Span::styled(" Preview", t::bold())),
                        ],
                    );
                    let dock = Rect {
                        y: prev.y + 2,
                        height: prev.height - 2,
                        ..prev
                    };
                    if mode == DockMode::Hidden {
                        t::draw_lines(
                            buf,
                            dock,
                            vec![Line::from(Span::styled(
                                "   (no dock — ⌃\\ still opens the menu)",
                                t::dim(),
                            ))],
                        );
                    } else if dock.height > 0 {
                        render_dock(&dock_model(&self.config, &views, mode), dock, buf);
                    }
                }
            }
            Tab::Sharing => {
                let mut homes = vec![self.common_text(Tool::Claude)];
                if self.config.profiles.iter().any(|p| p.tool == Tool::Codex) {
                    homes.push(format!("{} (Codex)", self.common_text(Tool::Codex)));
                }
                let head = vec![Line::from(Span::styled(
                    format!(
                        " Shared with {}: settings, skills, sessions and memory. Logins stay separate.",
                        homes.join(" and ")
                    ),
                    t::dim(),
                ))];
                self.render_rows(&rows, sel, area, buf, head);
            }
            Tab::Keys => {
                let k = |key: &str, what: &str| {
                    Line::from(vec![
                        Span::styled(format!("   {key:<12}"), Style::default().fg(t::ACCENT)),
                        Span::raw(what.to_string()),
                    ])
                };
                let mut head = vec![];
                let (body, rest) = split_bottom(area, 9);
                self.render_rows(&rows, sel, body, buf, std::mem::take(&mut head));
                t::draw_lines(
                    buf,
                    rest,
                    vec![
                        Line::from(""),
                        Line::from(Span::styled(" In menus and settings", t::bold())),
                        k("↑↓ ⏎ ␣", "move, choose, toggle"),
                        k("←→ 1–6 tab", "switch tabs"),
                        k("+ -", "change numbers and colors"),
                        k("esc", "back / close"),
                        k("⌃\\ ⌃\\", "send ⌃\\ to Claude or Codex"),
                    ],
                );
            }
            Tab::About => self.render_about(area, buf),
        }
        help
    }

    fn render_about(&self, area: Rect, buf: &mut Buffer) {
        let p = &self.ctx.paths;
        let kv = |k: &str, v: String, s: Style| {
            Line::from(vec![
                Span::styled(format!("   {k:<12}"), t::dim()),
                Span::styled(v, s),
            ])
        };
        let claude = match self.ctx.claude_bin(&self.config) {
            Ok(b) => (b.display().to_string(), Style::default()),
            Err(e) => (format!("{e:#}"), t::error()),
        };
        let has_codex = self.config.profiles.iter().any(|p| p.tool == Tool::Codex);
        let codex = match self.ctx.tool_bin(&self.config, Tool::Codex) {
            Ok(b) => (b.display().to_string(), Style::default()),
            Err(_) if !has_codex => ("not installed (optional)".to_string(), t::dim()),
            Err(e) => (format!("{e:#}"), t::error()),
        };
        let n = self.config.profiles.len();
        let pool = self.config.profiles.iter().filter(|p| p.pool).count();
        let views = self.live.lock().views.clone();
        let logged = views.iter().filter(|v| login_state(&v.token).1).count();
        let to_fix = self
            .health
            .iter()
            .filter(|h| matches!(h, Ok((h, _)) if h.to_fix > 0 || !h.sessions_shared))
            .count();
        let doctor = format!(
            "{n} account{}, {pool} in the pool, {logged} logged in, {to_fix} need sharing fixes",
            if n == 1 { "" } else { "s" }
        );
        let lines = vec![
            Line::from(vec![
                Span::styled(" agent-prism ", t::title()),
                Span::raw(env!("CARGO_PKG_VERSION")),
                Span::styled("  — Claude and Codex across several accounts", t::dim()),
            ]),
            Line::from(""),
            kv(
                "Config",
                p.config_file().display().to_string(),
                Style::default(),
            ),
            kv("State", p.state_dir.display().to_string(), Style::default()),
            kv("Log", p.log_file().display().to_string(), Style::default()),
            kv("Claude", claude.0, claude.1),
            kv("Codex", codex.0, codex.1),
            Line::from(""),
            kv(
                "Doctor",
                doctor,
                if to_fix > 0 { t::warn() } else { t::ok() },
            ),
        ];
        // Rows the text takes once wrapped. Word wrap can need one more
        // row than the plain width suggests, so count high, never low.
        let width = area.width.max(1) as usize;
        let text_h: usize = lines
            .iter()
            .map(|l| match l.width() {
                n if n <= width => 1,
                n => n.div_ceil(width) + 1,
            })
            .sum();
        t::draw_wrapped(buf, area, lines);

        // The prism along the bottom, when there is room under the text.
        let art_h = area.height.saturating_sub(text_h as u16 + 1).min(16);
        if art_h >= 6 && area.width >= 30 {
            let w = area.width.min(art_h * 5 + 10);
            let art = Rect {
                x: area.x + (area.width - w) / 2,
                y: area.y + area.height - art_h,
                width: w,
                height: art_h,
            };
            let secs = self.started.elapsed().as_secs_f64();
            t::draw_lines(buf, art, super::art::prism_hero(secs, w, art_h));
        }
    }
}

fn split_bottom(area: Rect, h: u16) -> (Rect, Rect) {
    let h = h.min(area.height / 2 + 1).min(area.height);
    (
        Rect {
            height: area.height - h,
            ..area
        },
        Rect {
            y: area.y + area.height - h,
            height: h,
            ..area
        },
    )
}

/// `id`, or `id-2`, `id-3`… if taken.
pub fn unique_id(config: &Config, id: &str) -> String {
    let taken = |s: &str| config.profiles.iter().any(|p| p.id == s);
    if !taken(id) {
        return id.to_string();
    }
    (2..)
        .map(|n| format!("{id}-{n}"))
        .find(|s| !taken(s))
        .unwrap_or_else(|| id.to_string())
}

impl Panel<UiAction> for SettingsPanel {
    fn preferred_height(&self, rows: u16) -> u16 {
        (rows * 6 / 10).max(12)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let inner = t::frame(buf, area, "agp settings");
        let (body, foot) = t::split_footer(inner);
        let esc = if self.full_screen_hint {
            "quit"
        } else {
            "close"
        };
        // Flows take the whole panel.
        match &mut self.overlay {
            Overlay::Adopt(v) => {
                v.render(body, buf);
                t::draw_lines(buf, foot, vec![t::hints(&v.hints())]);
                return;
            }
            Overlay::Login(v) => {
                v.render(body, buf);
                t::draw_lines(buf, foot, vec![t::hints(&v.hints())]);
                return;
            }
            _ => {}
        }
        if body.height < 3 {
            return;
        }
        let tabs = Rect { height: 1, ..body };
        self.render_tabs(tabs, buf);
        let content = Rect {
            y: body.y + 2,
            height: body.height.saturating_sub(3),
            ..body
        };
        let help_area = Rect {
            y: body.y + body.height - 1,
            height: 1,
            ..body
        };
        let help = self.render_body(content, buf);
        let help_line = match (&self.overlay, &self.message) {
            (Overlay::ConfirmRemove(i), _) => {
                let p = self.config.profiles.get(*i);
                Line::from(Span::styled(
                    format!(
                        " Remove {} from agent-prism? The folder {} and its login stay.  y remove · n keep",
                        p.map(label_of).unwrap_or_default(),
                        p.map(|p| p.dir.clone()).unwrap_or_default()
                    ),
                    t::warn(),
                ))
            }
            (_, Some((m, err))) => Line::from(Span::styled(
                format!(" {m}"),
                if *err { t::error() } else { t::ok() },
            )),
            _ => Line::from(Span::styled(format!(" {help}"), t::dim())),
        };
        t::draw_lines(buf, help_area, vec![help_line]);
        let hints: Vec<(&str, &str)> = match &self.overlay {
            Overlay::Edit(..) => vec![("⏎", "save"), ("esc", "cancel")],
            Overlay::ConfirmRemove(_) => vec![("y", "remove"), ("n", "keep")],
            _ if self.detail.is_some() => vec![
                ("↑↓", "move"),
                ("⏎", "edit"),
                ("+-", "change"),
                ("esc", "back"),
            ],
            _ => vec![
                ("←→", "tabs"),
                ("↑↓", "move"),
                ("⏎", "select"),
                ("␣", "toggle"),
                ("esc", esc),
            ],
        };
        t::draw_lines(buf, foot, vec![t::hints(&hints)]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> PanelOutcome<UiAction> {
        if let Some(out) = self.handle_overlay_key(key) {
            return out;
        }
        if is_menu_key(&key) {
            return PanelOutcome::CloseAndForward(vec![super::menu::MENU_BYTE]);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return PanelOutcome::Close;
        }
        let ti = tab_index(self.tab);
        let rows = self.rows(200);
        let n = rows.len();
        let sel_ref = if self.detail.is_some() {
            &mut self.detail_sel
        } else {
            &mut self.sel[ti]
        };
        *sel_ref = (*sel_ref).min(n.saturating_sub(1));
        let sel = *sel_ref;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.detail.is_some() {
                    self.detail = None;
                    return PanelOutcome::Continue;
                }
                PanelOutcome::Close
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if n > 0 {
                    *sel_ref = (sel + n - 1) % n;
                }
                PanelOutcome::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if n > 0 {
                    *sel_ref = (sel + 1) % n;
                }
                PanelOutcome::Continue
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.switch_tab(TABS[(ti + TABS.len() - 1) % TABS.len()].0);
                PanelOutcome::Continue
            }
            KeyCode::Right | KeyCode::Tab => {
                self.switch_tab(TABS[(ti + 1) % TABS.len()].0);
                PanelOutcome::Continue
            }
            KeyCode::Char(c @ '1'..='6') => {
                self.switch_tab(TABS[(c as u8 - b'1') as usize].0);
                PanelOutcome::Continue
            }
            KeyCode::Char(' ') => match rows.get(sel).map(|r| r.f) {
                Some(F::Account(i)) => self.activate(F::Pool(i), 0),
                Some(f) => self.activate(f, 0),
                None => PanelOutcome::Continue,
            },
            KeyCode::Enter => match rows.get(sel).map(|r| r.f) {
                Some(f) => self.activate(f, 0),
                None => PanelOutcome::Continue,
            },
            KeyCode::Char('+' | '=' | ']') => match rows.get(sel).map(|r| r.f) {
                Some(f) => self.activate(f, 1),
                None => PanelOutcome::Continue,
            },
            KeyCode::Char('-' | '_' | '[') => match rows.get(sel).map(|r| r.f) {
                Some(f) => self.activate(f, -1),
                None => PanelOutcome::Continue,
            },
            _ => PanelOutcome::Continue,
        }
    }

    fn handle_paste(&mut self, text: &str) -> PanelOutcome<UiAction> {
        match &mut self.overlay {
            Overlay::Edit(_, f) => f.insert(text),
            Overlay::Login(v) => v.handle_paste(text),
            _ => {}
        }
        PanelOutcome::Continue
    }

    fn tick(&mut self) {
        {
            let live = self.live.lock();
            if live.config != self.seen {
                self.seen = live.config.clone();
                self.config = live.config.clone();
            }
        }
        match &mut self.overlay {
            Overlay::Adopt(v) => v.tick(),
            Overlay::Login(v) => v.tick(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testutil::*;

    fn panel(tab: Tab) -> (Fixture, SettingsPanel) {
        let f = Fixture::new();
        let p = SettingsPanel::new(f.ctx.clone(), live(), tab);
        (f, p)
    }

    fn saved(f: &Fixture) -> Config {
        Config::load(f.paths()).unwrap().expect("config saved")
    }

    #[test]
    fn every_tab_renders() {
        let (_f, mut p) = panel(Tab::Accounts);
        let expect = [
            "+ Add account",
            "Next session → Bob",
            "Preview",
            "Backup target",
            "Menu key",
            "Doctor",
        ];
        for (i, want) in expect.iter().enumerate() {
            p.handle_key(ch((b'1' + i as u8) as char));
            for (w, h) in SIZES {
                let s = render(&mut p, w, h);
                assert!(s.contains("Accounts"), "{s}");
                if h >= 24 {
                    assert!(s.contains(want), "tab {i} {w}x{h}: {s}");
                }
            }
        }
    }

    #[test]
    fn accounts_list_shows_found_dirs() {
        let (_f, mut p) = panel(Tab::Accounts);
        let s = render(&mut p, 120, 40);
        assert!(s.contains("Ann") && s.contains("Bob"), "{s}");
        assert!(s.contains("Found:") && s.contains("~/.claude-c"), "{s}");
        assert!(
            !s.contains("(~/.claude-a)"),
            "configured dirs are not found rows"
        );
    }

    #[test]
    fn tab_switching() {
        let (_f, mut p) = panel(Tab::Accounts);
        p.handle_key(key(KeyCode::Right));
        assert_eq!(p.tab, Tab::Balancing);
        p.handle_key(key(KeyCode::Left));
        p.handle_key(key(KeyCode::Left));
        assert_eq!(p.tab, Tab::About);
        p.handle_key(key(KeyCode::Tab));
        assert_eq!(p.tab, Tab::Accounts);
        p.handle_key(ch('4'));
        assert_eq!(p.tab, Tab::Sharing);
    }

    #[test]
    fn toggling_pool_saves() {
        let (f, mut p) = panel(Tab::Accounts);
        p.handle_key(key(KeyCode::Down)); // Ann
        let out = p.handle_key(ch(' '));
        assert!(matches!(out, PanelOutcome::Action(UiAction::ConfigChanged)));
        assert!(!saved(&f).profiles[0].pool);
        // Same through the detail view.
        p.handle_key(key(KeyCode::Enter));
        assert_eq!(p.detail, Some(0));
        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Action(UiAction::ConfigChanged)
        ));
        assert!(saved(&f).profiles[0].pool);
        for (w, h) in SIZES {
            assert!(render(&mut p, w, h).contains("In pool"));
        }
        p.handle_key(key(KeyCode::Esc));
        assert_eq!(p.detail, None);
        assert!(matches!(
            p.handle_key(key(KeyCode::Esc)),
            PanelOutcome::Close
        ));
    }

    #[test]
    fn weight_and_reserve_bounds() {
        let (f, mut p) = panel(Tab::Accounts);
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Enter));
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down)); // Weight
        for _ in 0..60 {
            p.handle_key(ch('+'));
        }
        assert_eq!(saved(&f).profiles[0].weight, 5.0);
        for _ in 0..80 {
            p.handle_key(ch('-'));
        }
        assert_eq!(saved(&f).profiles[0].weight, 0.1);
        p.handle_key(key(KeyCode::Down)); // Reserve
        for _ in 0..30 {
            p.handle_key(ch('+'));
        }
        assert_eq!(saved(&f).profiles[0].reserve_7d, 90);
        p.handle_key(ch('-'));
        assert_eq!(saved(&f).profiles[0].reserve_7d, 85);
        // Typed values clamp too.
        p.handle_key(key(KeyCode::Enter));
        p.handle_key(ctrl('u'));
        for c in "300".chars() {
            p.handle_key(ch(c));
        }
        p.handle_key(key(KeyCode::Enter));
        assert_eq!(saved(&f).profiles[0].reserve_7d, 90);
        p.handle_key(key(KeyCode::Down)); // Color
        p.handle_key(ch('+'));
        assert_eq!(saved(&f).profiles[0].color.as_deref(), Some("cyan"));
        p.handle_key(ch('+'));
        assert_eq!(saved(&f).profiles[0].color.as_deref(), Some("magenta"));
    }

    #[test]
    fn edit_label_and_cancel() {
        let (f, mut p) = panel(Tab::Accounts);
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Enter));
        p.handle_key(key(KeyCode::Down)); // Label
        p.handle_key(key(KeyCode::Enter));
        p.handle_key(ctrl('u'));
        for c in "Work".chars() {
            p.handle_key(ch(c));
        }
        assert!(render(&mut p, 80, 24).contains("Work"));
        p.handle_key(key(KeyCode::Enter));
        assert_eq!(saved(&f).profiles[0].label, "Work");
        p.handle_key(key(KeyCode::Enter));
        p.handle_key(ch('X'));
        p.handle_key(key(KeyCode::Esc));
        assert_eq!(saved(&f).profiles[0].label, "Work");
        assert_eq!(p.detail, Some(0), "esc only cancels the edit");
    }

    #[test]
    fn remove_needs_confirm_and_keeps_dir() {
        let (f, mut p) = panel(Tab::Accounts);
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Enter));
        for _ in 0..7 {
            p.handle_key(key(KeyCode::Down));
        }
        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Continue
        ));
        assert!(render(&mut p, 120, 40).contains("Remove Ann from agent-prism?"));
        assert!(
            Config::load(f.paths()).unwrap().is_none(),
            "nothing saved yet"
        );
        p.handle_key(ch('y'));
        let c = saved(&f);
        assert_eq!(c.profiles.len(), 1);
        assert!(f.paths().home.join(".claude-a").is_dir());
        // The removed dir is now offered as found.
        assert!(render(&mut p, 120, 40).contains("~/.claude-a"));
    }

    #[test]
    fn add_found_dir() {
        let (f, mut p) = panel(Tab::Accounts);
        for _ in 0..3 {
            p.handle_key(key(KeyCode::Down));
        }
        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Action(UiAction::ConfigChanged)
        ));
        let c = saved(&f);
        assert!(
            c.profiles.iter().any(|p| p.dir == "~/.claude"),
            "{:?}",
            c.profiles
        );
    }

    #[test]
    fn add_account_flow_appends_profile() {
        let (f, mut p) = panel(Tab::Accounts);
        p.login_cmd = Some(LoginCommand {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"echo "Open this URL"; read x; printf '{"oauthAccount":{"displayName":"Neo","emailAddress":"neo@example.com"}}' > "$CLAUDE_CONFIG_DIR/.claude.json""#.into(),
            ],
        });
        p.handle_key(key(KeyCode::Enter)); // + Add account
        assert!(render(&mut p, 80, 24).contains("Add a Claude or Codex account"));
        p.handle_key(key(KeyCode::Enter)); // Tool: Claude
        for c in "neo".chars() {
            p.handle_key(ch(c));
        }
        p.handle_key(key(KeyCode::Enter));
        p.handle_key(key(KeyCode::Enter));
        assert!(render(&mut p, 80, 24).contains("A browser window opens"));
        p.handle_key(key(KeyCode::Enter));
        for _ in 0..300 {
            p.tick();
            if render(&mut p, 80, 24).contains("Logged in as") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(render(&mut p, 80, 24).contains("Logged in as Neo"));
        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Action(UiAction::ConfigChanged)
        ));
        let c = saved(&f);
        let n = c.profiles.last().unwrap();
        assert_eq!(
            (n.id.as_str(), n.dir.as_str(), n.pool),
            ("neo", "~/.claude-neo", true)
        );
    }

    #[test]
    fn balancing_changes_preview() {
        let (f, mut p) = panel(Tab::Balancing);
        p.handle_key(key(KeyCode::Down)); // priority
        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Action(UiAction::ConfigChanged)
        ));
        assert_eq!(saved(&f).balance.policy, Policy::Priority);
        // Views have valid tokens; priority picks the first: Ann.
        assert!(render(&mut p, 120, 40).contains("Next session → Ann"));
        for _ in 0..10 {
            p.handle_key(key(KeyCode::Down)); // Limited at
        }
        for _ in 0..10 {
            p.handle_key(ch('+'));
        }
        assert_eq!(saved(&f).balance.hard, 100.0);
    }

    #[test]
    fn sharing_opens_adopt_flow() {
        let (_f, mut p) = panel(Tab::Sharing);
        let s = render(&mut p, 120, 40);
        assert!(s.contains("sessions not shared"), "{s}");
        p.handle_key(key(KeyCode::Enter));
        for (w, h) in SIZES {
            assert!(render(&mut p, w, h).contains("Share sessions"));
        }
        p.handle_key(key(KeyCode::Esc));
        assert!(render(&mut p, 120, 40).contains("Backup target"));
    }

    #[test]
    fn menu_key_forwards() {
        let (_f, mut p) = panel(Tab::Dock);
        let k = KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL);
        assert!(matches!(p.handle_key(k), PanelOutcome::CloseAndForward(b) if b == vec![0x1c]));
    }

    #[test]
    fn unique_ids() {
        let c = config();
        assert_eq!(unique_id(&c, "x"), "x");
        assert_eq!(unique_id(&c, "a"), "a-2");
    }
}
