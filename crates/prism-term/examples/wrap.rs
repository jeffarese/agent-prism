//! Demo host: `cargo run -p prism-term --example wrap -- <cmd> [args…]`.
//!
//! Wraps the command with a fake dock of two accounts whose numbers change
//! every few seconds. The menu key opens a small panel: `r` restarts the
//! child, `d` cycles the dock mode, `c` starts a fake switch countdown,
//! Esc closes, and the menu key again sends a literal Ctrl+\ to the child.

use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use prism_core::model::*;
use prism_term::panel::{Panel, PanelOutcome};
use prism_term::pty::ChildSpec;
use prism_term::runner::{Control, Host, HostEvent, Runner, RunnerOptions};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

enum Act {
    Restart,
    CycleDock,
    Countdown,
}

struct Menu;

impl Panel<Act> for Menu {
    fn preferred_height(&self, rows: u16) -> u16 {
        (rows * 6 / 10).max(12)
    }
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let lines = [
            "─ agent-prism demo ──────────────────────────────",
            "  r  Restart the child      d  Dock mode",
            "  c  Fake switch countdown  ⌃\\ Send ⌃\\ to child",
            "  esc Close",
        ];
        for (i, l) in lines.iter().enumerate() {
            if (i as u16) < area.height {
                let st = if i == 0 {
                    Style::default().add_modifier(Modifier::DIM)
                } else {
                    Style::default()
                };
                buf.set_line(area.x, area.y + i as u16, &Line::styled(*l, st), area.width);
            }
        }
    }
    fn handle_key(&mut self, key: KeyEvent) -> PanelOutcome<Act> {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => PanelOutcome::Close,
            (KeyCode::Char('\\'), KeyModifiers::CONTROL) => {
                PanelOutcome::CloseAndForward(vec![0x1c])
            }
            (KeyCode::Char('r'), _) => PanelOutcome::Action(Act::Restart),
            (KeyCode::Char('d'), _) => PanelOutcome::Action(Act::CycleDock),
            (KeyCode::Char('c'), _) => PanelOutcome::Action(Act::Countdown),
            _ => PanelOutcome::Continue,
        }
    }
}

struct Demo {
    spec: ChildSpec,
    mode: DockMode,
    start: i64,
    countdown: Option<i64>,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn account(label: &str, color: &str, here: bool, five: f64, seven: f64, t: i64) -> AccountView {
    AccountView {
        profile_id: label.to_lowercase(),
        label: label.into(),
        color: Some(color.into()),
        identity: AccountIdentity {
            subscription: Some("team".into()),
            ..Default::default()
        },
        token: TokenState::Valid {
            expires_at: t + 3600,
        },
        usage: Some(UsageSnapshot {
            fetched_at: t,
            five_hour: Some(Window {
                utilization: five,
                resets_at: Some(t + 8040),
                inferred: false,
            }),
            seven_day: Some(Window {
                utilization: seven,
                resets_at: Some(t + 273600),
                inferred: false,
            }),
            scoped: vec![ScopedWindow {
                label: "Fable".into(),
                kind: "weekly_scoped".into(),
                utilization: 12.0,
                resets_at: None,
                inferred: false,
            }],
            extra_usage_enabled: false,
            limit_reached: false,
        }),
        usage_status: UsageStatus::Fresh,
        projection: Projection {
            five_hour: Some((five * 1.5).min(100.0)),
            seven_day: None,
        },
        in_pool: true,
        active_here: here,
        active_elsewhere: 0,
        limited: None,
        next_pick: !here,
        score: None,
        reason: None,
    }
}

impl Host<(), Act> for Demo {
    fn dock_model(&mut self) -> DockModel {
        let t = now();
        let k = ((t - self.start) / 3) as f64;
        DockModel {
            accounts: vec![
                account("Acme Al", "cyan", true, (48.0 + k) % 100.0, 31.0, t),
                account(
                    "Acme Bea",
                    "magenta",
                    false,
                    (1.0 + 2.0 * k) % 100.0,
                    72.0,
                    t,
                ),
            ],
            mode: self.mode,
            policy: Policy::Smart,
            session_short: Some("3f2a91c0".into()),
            on_account: Some(("Acme Al".into(), self.start)),
            usage_age_secs: Some((t - self.start) % 60),
            message: None,
            countdown: self.countdown.map(|d| Countdown {
                from_label: "Acme Al".into(),
                to_label: "Acme Bea".into(),
                reason: "hit the 5h limit".into(),
                deadline: d,
            }),
            menu_key_label: String::new(),
            show_projections: true,
            icons: DockIcons::Unicode,
            now: t,
        }
    }

    fn on_event(&mut self, event: HostEvent<(), Act>, ctl: &mut Control<Act>) {
        match event {
            HostEvent::MenuKey => ctl.open_panel(Box::new(Menu)),
            HostEvent::PanelAction(Act::Restart) => {
                ctl.close_panel();
                ctl.restart_child(self.spec.clone());
            }
            HostEvent::PanelAction(Act::CycleDock) => {
                self.mode = match self.mode {
                    DockMode::Auto => DockMode::Full,
                    DockMode::Full => DockMode::Compact,
                    DockMode::Compact => DockMode::Hidden,
                    DockMode::Hidden => DockMode::Auto,
                };
                ctl.close_panel();
                ctl.set_dock_mode(self.mode);
            }
            HostEvent::PanelAction(Act::Countdown) => {
                self.countdown = Some(now() + 5);
                ctl.close_panel();
                ctl.redraw();
            }
            HostEvent::Tick => {
                if let Some(d) = self.countdown
                    && now() > d
                {
                    self.countdown = None;
                }
                ctl.redraw();
            }
            _ => {}
        }
    }
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let Some(program) = args.next() else {
        anyhow::bail!("usage: wrap <cmd> [args…]");
    };
    let spec = ChildSpec {
        program: program.into(),
        args: args.collect(),
        env_set: vec![("AGP".into(), "1".into())],
        env_remove: vec![],
        cwd: std::env::current_dir()?,
    };
    let mode = match std::env::var("WRAP_DOCK").as_deref() {
        Ok("compact") => DockMode::Compact,
        Ok("hidden") => DockMode::Hidden,
        Ok("auto") => DockMode::Auto,
        _ => DockMode::Full,
    };
    let mut host = Demo {
        spec: spec.clone(),
        mode,
        start: now(),
        countdown: None,
    };
    let (mut runner, _tx) = Runner::new(RunnerOptions {
        initial_dock_mode: mode,
        ..Default::default()
    });
    let out = runner.run(&mut host, spec)?;
    std::process::exit(out.exit_code);
}
