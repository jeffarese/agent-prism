//! The small design system every agent-prism screen uses: ANSI-16 named colors
//! (so light and dark terminals both work), one accent color, rounded
//! borders and a shared key-hint footer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Widget, Wrap};

/// The one accent color.
pub const ACCENT: Color = Color::Cyan;
/// Marker in front of the selected row.
pub const MARKER: &str = "▸ ";
/// Spinner frames for background work.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Bold accent, for titles and headings.
pub fn title() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// The selected row.
pub fn selected() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// Help text and secondary facts.
pub fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// Bold, no color.
pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Errors.
pub fn error() -> Style {
    Style::default().fg(Color::Red)
}

/// Warnings.
pub fn warn() -> Style {
    Style::default().fg(Color::Yellow)
}

/// Success.
pub fn ok() -> Style {
    Style::default().fg(Color::Green)
}

/// Color of a usage percent, the same thresholds as the dock bars.
pub fn level(pct: f64) -> Style {
    let c = if pct < 60.0 {
        Color::Green
    } else if pct < 85.0 {
        Color::Yellow
    } else {
        Color::Red
    };
    Style::default().fg(c)
}

/// A usage percent like ` 48%`, colored by level.
pub fn pct_span(pct: Option<f64>) -> Span<'static> {
    match pct {
        Some(p) => Span::styled(format!("{:>3.0}%", p), level(p)),
        None => Span::styled("  —", dim()),
    }
}

/// The current spinner frame for a tick counter.
pub fn spinner(tick: usize) -> &'static str {
    SPINNER[tick % SPINNER.len()]
}

/// A rounded, dim-bordered block with an accent title.
pub fn block(title_text: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Span::styled(format!(" {title_text} "), title()))
}

/// Prefix for a list row: the marker when selected, else spaces.
pub fn marker(sel: bool) -> Span<'static> {
    if sel {
        Span::styled(MARKER, selected())
    } else {
        Span::raw("  ")
    }
}

/// Style of a row's main text.
pub fn row(sel: bool) -> Style {
    if sel { selected() } else { Style::default() }
}

/// A checkbox: `[x]` or `[ ]`.
pub fn check(on: bool) -> Span<'static> {
    if on {
        Span::styled("[x]", Style::default().fg(ACCENT))
    } else {
        Span::styled("[ ]", dim())
    }
}

/// A radio button: `(•)` or `( )`.
pub fn radio(on: bool) -> Span<'static> {
    if on {
        Span::styled("(•)", Style::default().fg(ACCENT))
    } else {
        Span::styled("( )", dim())
    }
}

/// Key-hint footer: `⏎ select · ␣ toggle · esc back`, keys bold, rest dim.
pub fn hints(items: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, what)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", dim()));
        }
        spans.push(Span::styled((*key).to_string(), bold()));
        spans.push(Span::styled(format!(" {what}"), dim()));
    }
    Line::from(spans)
}

/// Pad or cut `s` to exactly `w` columns. Cut text ends in `… ` so that
/// it never touches the next column.
pub fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        return format!("{s}{}", " ".repeat(w - n));
    }
    let keep = w.saturating_sub(2);
    let mut out: String = s.chars().take(keep).collect();
    if w > 0 {
        out.push('…');
    }
    if w > 1 {
        out.push(' ');
    }
    out
}

/// Clear `area` and draw `lines` into it (no wrapping).
pub fn draw_lines(buf: &mut Buffer, area: Rect, lines: Vec<Line<'static>>) {
    Paragraph::new(lines).render(area, buf);
}

/// Draw wrapped text.
pub fn draw_wrapped(buf: &mut Buffer, area: Rect, lines: Vec<Line<'static>>) {
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(area, buf);
}

/// Clear an area and draw the standard block; returns the inner area.
pub fn frame(buf: &mut Buffer, area: Rect, title_text: &str) -> Rect {
    Clear.render(area, buf);
    let b = block(title_text);
    let inner = b.inner(area);
    b.render(area, buf);
    inner
}

/// Split off the last row of `area` for the footer: `(body, footer)`.
pub fn split_footer(area: Rect) -> (Rect, Rect) {
    if area.height < 2 {
        return (area, Rect { height: 0, ..area });
    }
    let body = Rect {
        height: area.height - 1,
        ..area
    };
    let foot = Rect {
        y: area.y + area.height - 1,
        height: 1,
        ..area
    };
    (body, foot)
}

/// Keep the selected row visible: the first row to draw for a list of
/// `len` rows in `height` rows with `sel` selected.
pub fn scroll_top(sel: usize, len: usize, height: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    sel.saturating_sub(height - 1).min(len - height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_pads_and_cuts() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdef", 4), "ab… ");
        assert_eq!(fit("abcdef", 1), "…");
        assert_eq!(fit("", 0), "");
    }

    #[test]
    fn scroll_keeps_selection_visible() {
        assert_eq!(scroll_top(0, 10, 4), 0);
        assert_eq!(scroll_top(5, 10, 4), 2);
        assert_eq!(scroll_top(9, 10, 4), 6);
        assert_eq!(scroll_top(3, 3, 4), 0);
    }
}
