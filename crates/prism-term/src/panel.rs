//! A panel is an interactive view drawn inside the expanded dock (the menu,
//! the settings). While a panel is open it gets all keyboard input.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

/// What the panel wants after it handled an event.
pub enum PanelOutcome<A> {
    /// Keep the panel open; redraw if anything changed.
    Continue,
    /// Close the panel.
    Close,
    /// Close the panel and send these bytes to the child (e.g. a literal
    /// Ctrl+\ when the menu key is pressed twice).
    CloseAndForward(Vec<u8>),
    /// An action for the host to perform. The panel stays open unless the
    /// host closes it.
    Action(A),
    /// Replace this panel with another one (e.g. menu → settings).
    Replace(Box<dyn Panel<A>>),
}

pub trait Panel<A> {
    /// Dock height this panel wants for a terminal of `rows` rows. The runner
    /// clamps it so that the child keeps at least 5 rows.
    fn preferred_height(&self, rows: u16) -> u16;

    /// Draw into `area` of `buf`. Called after any event and on every tick
    /// while the panel is open.
    fn render(&mut self, area: Rect, buf: &mut Buffer);

    fn handle_key(&mut self, key: KeyEvent) -> PanelOutcome<A>;

    fn handle_paste(&mut self, _text: &str) -> PanelOutcome<A> {
        PanelOutcome::Continue
    }

    /// Mouse events with coordinates relative to the panel area.
    fn handle_mouse(&mut self, _event: MouseEvent) -> PanelOutcome<A> {
        PanelOutcome::Continue
    }

    /// Called about 4 times per second while open (for live data).
    fn tick(&mut self) {}
}
