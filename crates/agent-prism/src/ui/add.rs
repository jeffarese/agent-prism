//! `agp add [id] [--codex|--claude]`: the login flow full screen, then a short success screen.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use prism_core::config::Config;
use prism_core::model::Tool;
use prism_term::panel::{Panel, PanelOutcome};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::adopt::FlowOutcome;
use super::login::{LoginDone, LoginView};
use super::theme as t;
use crate::actions::UiAction;
use crate::app::AppCtx;

/// The full-screen add-account app.
pub struct AddApp {
    ctx: Arc<AppCtx>,
    config: Config,
    login: LoginView,
    done: Option<LoginDone>,
    /// A save error to report after the screen closes.
    pub error: Option<String>,
}

impl AddApp {
    /// Start with the id prefilled if given. Without `tool` the form asks
    /// for it.
    pub fn new(ctx: Arc<AppCtx>, config: Config, id: Option<String>, tool: Option<Tool>) -> Self {
        let login = LoginView::add(ctx.clone(), &config, id, tool);
        Self {
            ctx,
            config,
            login,
            done: None,
            error: None,
        }
    }

    fn save(&mut self, done: LoginDone) {
        self.config.profiles.push(done.profile_config());
        if let Err(e) = self.config.save(&self.ctx.paths) {
            self.error = Some(format!("Could not save the config: {e:#}"));
        }
        self.done = Some(done);
    }
}

impl Panel<UiAction> for AddApp {
    fn preferred_height(&self, rows: u16) -> u16 {
        rows
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let inner = t::frame(buf, area, "agp add");
        let (body, foot) = t::split_footer(inner);
        let body = Rect {
            x: body.x + 1,
            y: body.y + 1,
            width: body.width.saturating_sub(2),
            height: body.height.saturating_sub(1),
        };
        let Some(done) = &self.done else {
            self.login.render(body, buf);
            t::draw_lines(buf, foot, vec![t::hints(&self.login.hints())]);
            return;
        };
        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(" ✓ ", t::ok()),
                Span::styled(done.label.clone(), t::title()),
                Span::raw(" is ready"),
            ]),
            Line::from(""),
        ];
        match &self.error {
            Some(e) => lines.push(Line::from(Span::styled(format!(" {e}"), t::error()))),
            None => {
                lines.push(Line::from(format!(
                    " Added to the pool as {} ({}).",
                    done.id, done.dir
                )));
                lines.push(Line::from(
                    " New agent-prism sessions can use it right away.",
                ));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    " Tune weight, reserve or color in agp settings › Accounts.",
                    t::dim(),
                )));
            }
        }
        t::draw_wrapped(buf, body, lines);
        t::draw_lines(buf, foot, vec![t::hints(&[("⏎", "close")])]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> PanelOutcome<UiAction> {
        if self.done.is_some() {
            return match key.code {
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => PanelOutcome::Close,
                _ => PanelOutcome::Continue,
            };
        }
        match self.login.handle_key(key) {
            FlowOutcome::Continue => PanelOutcome::Continue,
            FlowOutcome::Done(_) => match self.login.result.clone() {
                Some(done) => {
                    self.save(done);
                    PanelOutcome::Continue
                }
                None => PanelOutcome::Close,
            },
        }
    }

    fn handle_paste(&mut self, text: &str) -> PanelOutcome<UiAction> {
        if self.done.is_none() {
            self.login.handle_paste(text);
        }
        PanelOutcome::Continue
    }

    fn tick(&mut self) {
        self.login.tick();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::login::LoginCommand;
    use crate::ui::testutil::*;

    #[test]
    fn add_then_success_screen() {
        let f = Fixture::new();
        let mut app = AddApp::new(
            f.ctx.clone(),
            config(),
            Some("neo".into()),
            Some(Tool::Claude),
        );
        app.login.command = Some(LoginCommand {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"printf '{"oauthAccount":{"emailAddress":"neo@example.com"}}' > "$CLAUDE_CONFIG_DIR/.claude.json""#.into(),
            ],
        });
        for (w, h) in SIZES {
            assert!(render(&mut app, w, h).contains("~/.claude-neo"));
        }
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));
        for _ in 0..300 {
            app.tick();
            if render(&mut app, 80, 24).contains("Logged in as") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        app.handle_key(key(KeyCode::Enter));
        for (w, h) in SIZES {
            assert!(render(&mut app, w, h).contains("is ready"));
        }
        let c = Config::load(f.paths()).unwrap().unwrap();
        assert_eq!(c.profiles.last().unwrap().id, "neo");
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter)),
            PanelOutcome::Close
        ));
    }

    #[test]
    fn esc_on_form_closes() {
        let f = Fixture::new();
        let mut app = AddApp::new(f.ctx.clone(), config(), None, Some(Tool::Claude));
        assert!(matches!(
            app.handle_key(key(KeyCode::Esc)),
            PanelOutcome::Close
        ));
    }
}
