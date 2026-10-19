//! Runs a panel full screen (alt screen, raw mode, bracketed paste, mouse)
//! for `agp settings`, `agp setup` and `agp add`.

use std::io::{Stdout, Write, stdout};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use prism_term::panel::{Panel, PanelOutcome};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use super::data::load_live;
use crate::actions::UiAction;
use crate::app::AppCtx;
use crate::live::Live;

const TICK: Duration = Duration::from_millis(100);

fn restore() {
    let _ = disable_raw_mode();
    let mut out = stdout();
    let _ = execute!(
        out,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = out.flush();
}

/// Restores the terminal when dropped (also during unwinding).
struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));
    });
}

fn enter() -> Result<(Guard, Terminal<CrosstermBackend<Stdout>>)> {
    install_panic_hook();
    enable_raw_mode()?;
    let guard = Guard;
    let mut out = stdout();
    execute!(
        out,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        crossterm::cursor::Hide
    )?;
    let term = Terminal::new(CrosstermBackend::new(out))?;
    Ok((guard, term))
}

/// Reload the config and rebuild `live` from the usage cache.
pub fn refresh_live(ctx: &AppCtx, live: &Live) {
    match ctx.load_config() {
        Ok(cfg) => {
            let log = live.lock().log.clone();
            let data = load_live(ctx, &cfg, log);
            *live.lock() = data;
        }
        Err(e) => live.lock().log.push(format!("config: {e:#}")),
    }
}

/// Run `panel` full screen until it closes.
pub fn run(ctx: &Arc<AppCtx>, live: &Live, panel: &mut dyn Panel<UiAction>) -> Result<()> {
    let (_guard, mut term) = enter()?;
    term.clear()?;
    let mut replaced: Option<Box<dyn Panel<UiAction>>> = None;
    let mut last_tick = Instant::now();
    loop {
        let active: &mut dyn Panel<UiAction> = match replaced.as_deref_mut() {
            Some(p) => p,
            None => &mut *panel,
        };
        term.draw(|f| {
            let area = f.area();
            active.render(area, f.buffer_mut());
        })?;
        let wait = TICK.saturating_sub(last_tick.elapsed());
        let mut outcome = PanelOutcome::Continue;
        if event::poll(wait)? {
            outcome = match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => active.handle_key(k),
                Event::Paste(s) => active.handle_paste(&s),
                Event::Mouse(m) => active.handle_mouse(m),
                _ => PanelOutcome::Continue,
            };
        }
        if last_tick.elapsed() >= TICK {
            active.tick();
            last_tick = Instant::now();
        }
        match outcome {
            PanelOutcome::Continue => {}
            PanelOutcome::Close | PanelOutcome::CloseAndForward(_) => break,
            PanelOutcome::Replace(p) => replaced = Some(p),
            PanelOutcome::Action(UiAction::ConfigChanged | UiAction::RefreshUsage(_)) => {
                refresh_live(ctx, live);
            }
            PanelOutcome::Action(_) => {}
        }
    }
    Ok(())
}
