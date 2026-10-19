//! The dock: a ratatui renderer for [`DockModel`] (docs/DESIGN.md §7.7) and
//! [`DockPainter`], which turns rendered buffers into escape sequences for
//! the bottom rows of the real terminal (§7.3).

use std::fmt::Write as _;

use prism_core::model::{
    AccountView, DockIcons, DockMode, DockModel, MessageLevel, Policy, TokenState, UsageStatus,
    Window,
};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

const BAR_CELLS: usize = 15;
const PALETTE: [Color; 6] = [
    Color::Cyan,
    Color::Magenta,
    Color::Blue,
    Color::Green,
    Color::Yellow,
    Color::Red,
];

/// Map a config color name to a terminal palette color. Unknown or missing
/// names get a stable pick by `index`.
pub fn account_color(name: Option<&str>, index: usize) -> Color {
    match name.map(str::to_ascii_lowercase).as_deref() {
        Some("cyan") => Color::Cyan,
        Some("magenta") => Color::Magenta,
        Some("blue") => Color::Blue,
        Some("green") => Color::Green,
        Some("yellow") => Color::Yellow,
        Some("red") => Color::Red,
        Some("white") => Color::White,
        _ => PALETTE[index % PALETTE.len()],
    }
}

/// Format a duration in seconds as `2h14m`, `21h`, `3d4h`, `12m`.
pub fn fmt_duration(secs: i64) -> String {
    let s = secs.max(0);
    let (d, h, m) = (s / 86400, (s % 86400) / 3600, (s % 3600) / 60);
    if d > 0 {
        if h > 0 {
            format!("{d}d{h}h")
        } else {
            format!("{d}d")
        }
    } else if h >= 10 {
        format!("{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

fn level_color(pct: f64) -> Color {
    if pct < 60.0 {
        Color::Green
    } else if pct < 85.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn bar(pct: f64, color: Color) -> Vec<Span<'static>> {
    const PARTS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = ((pct.clamp(0.0, 100.0) / 100.0) * (BAR_CELLS * 8) as f64).round() as usize;
    let eighths = if pct > 0.0 { eighths.max(1) } else { 0 };
    let full = eighths / 8;
    let part = eighths % 8;
    let mut filled = "█".repeat(full);
    filled.push_str(PARTS[part]);
    let used = full + usize::from(part > 0);
    vec![
        Span::styled(filled, Style::default().fg(color)),
        Span::styled("░".repeat(BAR_CELLS - used), dim()),
    ]
}

fn policy_name(p: Policy) -> &'static str {
    match p {
        Policy::Smart => "smart",
        Policy::Priority => "priority",
        Policy::RoundRobin => "round-robin",
        Policy::Headroom => "headroom",
    }
}

/// The dock icons for one icon set.
struct Glyphs {
    here: &'static str,
    other: &'static str,
    idle: &'static str,
    expired: &'static str,
    limited: &'static str,
    next: &'static str,
    pool: &'static str,
    /// Drawn before the menu key label. Empty for none.
    menu: &'static str,
}

const UNICODE: Glyphs = Glyphs {
    here: "●",
    other: "◐",
    idle: "○",
    expired: "⚠",
    limited: "✕",
    next: "▸",
    pool: "Σ",
    menu: "",
};

/// Font Awesome icons from a Nerd Font.
const NERD: Glyphs = Glyphs {
    here: "\u{f192}", // dot-circle
    other: "◐",
    idle: "\u{f10c}",    // circle-o
    expired: "\u{f127}", // chain-broken
    limited: "\u{f252}", // hourglass-half
    next: "▸",
    pool: "\u{f1c0}",  // database
    menu: "\u{f013} ", // cog
};

fn glyphs(model: &DockModel) -> &'static Glyphs {
    match model.icons {
        DockIcons::Unicode => &UNICODE,
        DockIcons::Nerd => &NERD,
    }
}

fn glyph(model: &DockModel, a: &AccountView) -> (&'static str, Style) {
    let g = glyphs(model);
    if a.limited.is_some() {
        (g.limited, Style::default().fg(Color::Red))
    } else if matches!(a.token, TokenState::Expired { .. }) {
        (g.expired, dim())
    } else if a.active_here {
        (g.here, Style::default().add_modifier(Modifier::BOLD))
    } else if a.active_elsewhere > 0 {
        (g.other, Style::default())
    } else {
        (g.idle, Style::default())
    }
}

/// Label style. The account this session runs on gets its color as a
/// background so it stands out from the rest.
fn label_style(a: &AccountView, idx: usize) -> Style {
    let c = account_color(a.color.as_deref(), idx);
    if a.active_here {
        Style::default()
            .fg(Color::Black)
            .bg(c)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(c)
    }
}

/// Which optional columns are shown.
#[derive(Clone, Copy)]
struct Cols {
    proj: bool,
    scoped: bool,
    reset: bool,
    tier: bool,
    bars: bool,
}

fn pad(s: &str, w: usize) -> String {
    let cur = s.width();
    let mut o = s.to_string();
    o.extend(std::iter::repeat_n(' ', w.saturating_sub(cur)));
    o
}

fn window_spans(
    model: &DockModel,
    name: &str,
    w: Option<&Window>,
    proj: Option<f64>,
    cols: Cols,
    row_dim: bool,
) -> Vec<Span<'static>> {
    let mut v = vec![Span::styled(format!("  {name} "), dim())];
    let Some(w) = w else {
        if cols.bars {
            v.push(Span::styled("░".repeat(BAR_CELLS), dim()));
            v.push(Span::raw(" "));
        }
        v.push(Span::styled("  —", dim()));
        if cols.proj && model.show_projections {
            v.push(Span::raw("     "));
        }
        if cols.reset {
            v.push(Span::raw(" ".repeat(8)));
        }
        return v;
    };
    let pct = w.utilization;
    let color = level_color(pct);
    if cols.bars {
        v.extend(bar(pct, color));
        v.push(Span::raw(" "));
    }
    let text = if w.inferred {
        format!("~{:.0}%", pct)
    } else {
        format!("{:.0}%", pct)
    };
    let mut st = Style::default().fg(color);
    if w.inferred || row_dim {
        st = st.add_modifier(Modifier::DIM);
    }
    v.push(Span::styled(format!("{text:>4}"), st));
    if cols.proj && model.show_projections {
        match proj {
            Some(p) => v.push(Span::styled(
                format!("→{:<4}", format!("{p:.0}%")),
                Style::default()
                    .fg(level_color(p))
                    .add_modifier(Modifier::DIM),
            )),
            None => v.push(Span::raw("     ")),
        }
    }
    if cols.reset {
        let r = match w.resets_at {
            Some(t) => format!(" ↻{}", fmt_duration(t - model.now)),
            None => String::new(),
        };
        v.push(Span::styled(pad(&r, 8), dim()));
    }
    v
}

fn account_line(
    model: &DockModel,
    idx: usize,
    a: &AccountView,
    label_w: usize,
    tier_w: usize,
    scoped_label: Option<&str>,
    cols: Cols,
) -> Line<'static> {
    let (g, gs) = glyph(model, a);
    let expired = match a.token {
        TokenState::Expired { since } => Some(since),
        _ => None,
    };
    let row_dim = expired.is_some();
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(g, gs),
        Span::raw(" "),
        Span::styled(format!(" {} ", a.label), label_style(a, idx)),
        Span::raw(pad("", label_w.saturating_sub(a.label.width()))),
    ];
    if cols.tier {
        let tier = a.identity.subscription.clone().unwrap_or_default();
        spans.push(Span::styled(format!("  {}", pad(&tier, tier_w)), dim()));
    }
    let usage = a.usage.as_ref();
    spans.extend(window_spans(
        model,
        "5h",
        usage.and_then(|u| u.five_hour.as_ref()),
        a.projection.five_hour,
        cols,
        row_dim,
    ));
    spans.extend(window_spans(
        model,
        "7d",
        usage.and_then(|u| u.seven_day.as_ref()),
        a.projection.seven_day,
        cols,
        row_dim,
    ));
    if cols.scoped
        && let Some(label) = scoped_label
    {
        let sw = usage.and_then(|u| u.scoped.iter().find(|s| s.label == label));
        match sw {
            Some(s) => {
                let t = if s.inferred {
                    format!("~{:.0}%", s.utilization)
                } else {
                    format!("{:.0}%", s.utilization)
                };
                let mut st = Style::default().fg(level_color(s.utilization));
                if s.inferred || row_dim {
                    st = st.add_modifier(Modifier::DIM);
                }
                spans.push(Span::styled(format!("  {label} "), dim()));
                spans.push(Span::styled(format!("{t:>3}"), st));
            }
            None => spans.push(Span::styled(format!("  {label}  —"), dim())),
        }
    }
    if let Some(l) = &a.limited {
        let t = match l.until {
            Some(u) => format!("  limit · resets {}", fmt_duration(u - model.now)),
            None => "  limit".to_string(),
        };
        spans.push(Span::styled(t, Style::default().fg(Color::Red)));
    } else if let Some(since) = expired {
        spans.push(Span::styled(
            format!("  stale {}", fmt_duration(model.now - since)),
            dim(),
        ));
    } else if let UsageStatus::Stale { age_secs } = a.usage_status {
        spans.push(Span::styled(
            format!("  stale {}", fmt_duration(age_secs)),
            dim(),
        ));
    } else if let UsageStatus::Unavailable(_) = a.usage_status
        && a.usage.is_none()
    {
        spans.push(Span::styled("  no usage", dim()));
    }
    if a.next_pick {
        spans.push(Span::styled(
            format!("  next {}", glyphs(model).next),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    let mut line = Line::from(spans);
    if row_dim {
        line = line.style(dim());
    }
    line
}

/// Whether the dock adds a combined row for the pool (two or more pool
/// accounts shown).
pub fn has_pool_row(model: &DockModel) -> bool {
    model.accounts.iter().filter(|a| a.in_pool).count() >= 2
}

/// Rows the full dock wants between the separator and the status line.
pub fn dock_rows(model: &DockModel) -> usize {
    model.accounts.len() + usize::from(has_pool_row(model))
}

/// One window combined over the pool accounts: utilization and projection
/// weighted by plan capacity, and the earliest reset. Accounts without data
/// for the window are left out; an account without a projection counts at
/// its current utilization.
fn pool_window(
    model: &DockModel,
    pick: impl Fn(&AccountView) -> (Option<&Window>, Option<f64>),
) -> (Option<Window>, Option<f64>) {
    let (mut cap, mut used, mut proj) = (0.0, 0.0, 0.0);
    let (mut any_proj, mut inferred, mut reset) = (false, false, None::<i64>);
    for a in model.accounts.iter().filter(|a| a.in_pool) {
        let (Some(w), p) = pick(a) else { continue };
        let c = a.identity.capacity();
        cap += c;
        used += c * w.utilization;
        proj += c * p.unwrap_or(w.utilization);
        any_proj |= p.is_some();
        inferred |= w.inferred;
        reset = match (reset, w.resets_at) {
            (Some(r), Some(t)) => Some(r.min(t)),
            (r, t) => r.or(t),
        };
    }
    if cap <= 0.0 {
        return (None, None);
    }
    let win = Window {
        utilization: used / cap,
        resets_at: reset,
        inferred,
    };
    (Some(win), any_proj.then(|| proj / cap))
}

fn pool_five(model: &DockModel) -> (Option<Window>, Option<f64>) {
    pool_window(model, |a| {
        (
            a.usage.as_ref().and_then(|u| u.five_hour.as_ref()),
            a.projection.five_hour,
        )
    })
}

fn pool_seven(model: &DockModel) -> (Option<Window>, Option<f64>) {
    pool_window(model, |a| {
        (
            a.usage.as_ref().and_then(|u| u.seven_day.as_ref()),
            a.projection.seven_day,
        )
    })
}

fn pool_line(model: &DockModel, label_w: usize, tier_w: usize, cols: Cols) -> Line<'static> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(glyphs(model).pool, bold),
        Span::raw("  "),
        Span::styled(pad("pool", label_w + 1), bold),
    ];
    if cols.tier {
        spans.push(Span::raw(format!("  {}", pad("", tier_w))));
    }
    let (w5, p5) = pool_five(model);
    let (w7, p7) = pool_seven(model);
    spans.extend(window_spans(model, "5h", w5.as_ref(), p5, cols, false));
    spans.extend(window_spans(model, "7d", w7.as_ref(), p7, cols, false));
    let n = model.accounts.iter().filter(|a| a.in_pool).count();
    spans.push(Span::styled(format!("  {n} accounts"), dim()));
    Line::from(spans)
}

fn local_hhmm(t: i64) -> String {
    let tt = t as libc::time_t;
    // SAFETY: localtime_r writes into the provided struct only.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&tt, &mut tm).is_null() };
    if ok {
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    } else {
        "--:--".to_string()
    }
}

/// The countdown text, e.g. `Acme Al hit the 5h limit · moving to Acme Bea
/// in 4s · ⌃\ stay`. Keys go to Claude while no panel is open, so the menu
/// key is the only way to stay.
fn countdown_text(model: &DockModel) -> Option<String> {
    let c = model.countdown.as_ref()?;
    let secs = (c.deadline - model.now).max(0);
    Some(format!(
        "{} {} · moving to {} in {}s · {} stay",
        c.from_label, c.reason, c.to_label, secs, model.menu_key_label
    ))
}

fn status_line(model: &DockModel) -> Line<'static> {
    if let Some(t) = countdown_text(model) {
        return Line::from(vec![
            Span::raw(" "),
            Span::styled(
                t,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
    }
    if let Some(m) = &model.message {
        let st = match m.level {
            MessageLevel::Info => Style::default(),
            MessageLevel::Warn => Style::default().fg(Color::Yellow),
            MessageLevel::Error => Style::default().fg(Color::Red),
        };
        return Line::from(vec![Span::raw(" "), Span::styled(m.text.clone(), st)]);
    }
    let mut s = format!(" {}{} menu", glyphs(model).menu, model.menu_key_label);
    if let Some(id) = &model.session_short {
        let _ = write!(s, " · session {id}");
    }
    if let Some((label, since)) = &model.on_account {
        let _ = write!(s, " · on {label} since {}", local_hhmm(*since));
    }
    if let Some(age) = model.usage_age_secs {
        let _ = write!(s, " · usage {} ago", fmt_duration(age));
    }
    Line::from(Span::styled(s, dim()))
}

fn line_width(l: &Line<'_>) -> usize {
    l.spans.iter().map(|s| s.content.width()).sum()
}

fn render_full(model: &DockModel, area: Rect, buf: &mut Buffer) {
    let w = area.width as usize;
    // Separator.
    let title = format!(" agent-prism · {} ─", policy_name(model.policy));
    let tw = title.width();
    let sep = format!("{}{}", "─".repeat(w.saturating_sub(tw)), title);
    buf.set_line(area.x, area.y, &Line::styled(sep, dim()), area.width);

    let label_w = model
        .accounts
        .iter()
        .map(|a| a.label.width())
        .max()
        .unwrap_or(0)
        .min(20);
    let pool = has_pool_row(model);
    let label_w = if pool { label_w.max(4) } else { label_w };
    let tier_w = model
        .accounts
        .iter()
        .filter_map(|a| a.identity.subscription.as_ref().map(|s| s.width()))
        .max()
        .unwrap_or(0);
    let scoped_label = model
        .accounts
        .iter()
        .filter_map(|a| a.usage.as_ref()?.scoped.first())
        .map(|s| s.label.clone())
        .next();
    let all = Cols {
        proj: true,
        scoped: true,
        reset: true,
        tier: tier_w > 0,
        bars: true,
    };
    let candidates = [
        all,
        Cols { proj: false, ..all },
        Cols {
            proj: false,
            scoped: false,
            ..all
        },
        Cols {
            proj: false,
            scoped: false,
            reset: false,
            ..all
        },
        Cols {
            proj: false,
            scoped: false,
            reset: false,
            tier: false,
            bars: true,
        },
        Cols {
            proj: false,
            scoped: false,
            reset: false,
            tier: false,
            bars: false,
        },
    ];
    let build = |cols: Cols| -> Vec<Line<'static>> {
        let mut v: Vec<Line<'static>> = model
            .accounts
            .iter()
            .enumerate()
            .map(|(i, a)| account_line(model, i, a, label_w, tier_w, scoped_label.as_deref(), cols))
            .collect();
        if pool {
            v.push(pool_line(model, label_w, tier_w, cols));
        }
        v
    };
    let mut lines = Vec::new();
    for (k, c) in candidates.iter().enumerate() {
        lines = build(*c);
        if k == candidates.len() - 1 || lines.iter().all(|l| line_width(l) <= w) {
            break;
        }
    }
    let rows_for_accounts = area.height.saturating_sub(2) as usize;
    for (i, l) in lines.iter().take(rows_for_accounts).enumerate() {
        buf.set_line(area.x, area.y + 1 + i as u16, l, area.width);
    }
    if area.height >= 2 {
        buf.set_line(
            area.x,
            area.y + area.height - 1,
            &status_line(model),
            area.width,
        );
    }
}

fn compact_pct(w: Option<&Window>) -> Span<'static> {
    let Some(w) = w else {
        return Span::styled("—", dim());
    };
    let mut st = Style::default().fg(level_color(w.utilization));
    if w.inferred {
        st = st.add_modifier(Modifier::DIM);
    }
    let t = if w.inferred {
        format!("~{:.0}%", w.utilization)
    } else {
        format!("{:.0}%", w.utilization)
    };
    Span::styled(t, st)
}

fn render_compact(model: &DockModel, area: Rect, buf: &mut Buffer) {
    let mut spans: Vec<Span<'static>> = vec![];
    for (i, a) in model.accounts.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" │", dim()));
        }
        let (g, gs) = glyph(model, a);
        spans.push(Span::raw(" "));
        spans.push(Span::styled(g, gs));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!(" {} ", a.label), label_style(a, i)));
        let u = a.usage.as_ref();
        for (name, w) in [
            ("5h", u.and_then(|u| u.five_hour.as_ref())),
            ("7d", u.and_then(|u| u.seven_day.as_ref())),
        ] {
            spans.push(Span::styled(format!(" {name} "), dim()));
            spans.push(compact_pct(w));
        }
        if a.next_pick {
            spans.push(Span::raw(format!(" {}", glyphs(model).next)));
        }
    }
    if has_pool_row(model) {
        spans.push(Span::styled(" │ ", dim()));
        spans.push(Span::styled(
            glyphs(model).pool,
            Style::default().add_modifier(Modifier::BOLD),
        ));
        for (name, (w, _)) in [("5h", pool_five(model)), ("7d", pool_seven(model))] {
            spans.push(Span::styled(format!(" {name} "), dim()));
            spans.push(compact_pct(w.as_ref()));
        }
    }
    spans.push(Span::styled(" │ ", dim()));
    if let Some(c) = &model.countdown {
        let secs = (c.deadline - model.now).max(0);
        spans.push(Span::styled(
            format!(
                "→ {} in {secs}s · {} stay ",
                c.to_label, model.menu_key_label
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    } else if let Some(m) = &model.message {
        spans.push(Span::raw(format!("{} ", m.text)));
    }
    spans.push(Span::styled(
        format!("{}{} ", glyphs(model).menu, model.menu_key_label),
        dim(),
    ));
    buf.set_line(area.x, area.y, &Line::from(spans), area.width);
}

/// Draw the dock for `model` into `area`. Full mode when the area has more
/// than one row, compact mode for a single row.
pub fn render_dock(model: &DockModel, area: Rect, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    if area.height == 1 || model.mode == DockMode::Compact {
        render_compact(model, Rect { height: 1, ..area }, buf);
    } else {
        render_full(model, area, buf);
    }
}

/// Emits escape sequences for the dock rows, diffing against the last
/// drawn buffer.
#[derive(Debug, Default)]
pub struct DockPainter {
    last: Option<Buffer>,
    last_top: u16,
}

fn color_sgr(out: &mut String, c: Color, fg: bool) {
    let base = if fg { 30 } else { 40 };
    let n = match c {
        Color::Reset => return,
        Color::Black => base,
        Color::Red => base + 1,
        Color::Green => base + 2,
        Color::Yellow => base + 3,
        Color::Blue => base + 4,
        Color::Magenta => base + 5,
        Color::Cyan => base + 6,
        Color::Gray => base + 7,
        Color::DarkGray => base + 60,
        Color::LightRed => base + 61,
        Color::LightGreen => base + 62,
        Color::LightYellow => base + 63,
        Color::LightBlue => base + 64,
        Color::LightMagenta => base + 65,
        Color::LightCyan => base + 66,
        Color::White => base + 67,
        Color::Indexed(i) => {
            let _ = write!(out, ";{};5;{i}", base + 8);
            return;
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";{};2;{r};{g};{b}", base + 8);
            return;
        }
    };
    let _ = write!(out, ";{n}");
}

fn sgr_for(cell: &Cell) -> String {
    let mut s = String::from("\x1b[0");
    let m = cell.modifier;
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::SLOW_BLINK, "5"),
        (Modifier::REVERSED, "7"),
        (Modifier::HIDDEN, "8"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if m.contains(flag) {
            s.push(';');
            s.push_str(code);
        }
    }
    color_sgr(&mut s, cell.fg, true);
    color_sgr(&mut s, cell.bg, false);
    s.push('m');
    s
}

impl DockPainter {
    /// A painter with nothing drawn yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the last drawn buffer: the next paint emits every cell.
    pub fn invalidate(&mut self) {
        self.last = None;
    }

    /// Emit escape sequences that draw `buf` with its first row at screen
    /// row `top` (0-based). Only cells that changed since the last paint are
    /// emitted, unless invalidated or the size or position changed. Ends
    /// with an SGR reset. Returns `false` if nothing was emitted.
    pub fn paint(&mut self, buf: &Buffer, top: u16, out: &mut Vec<u8>) -> bool {
        let area = buf.area;
        let full = match &self.last {
            Some(l) => l.area != area || self.last_top != top,
            None => true,
        };
        let cells: Vec<(u16, u16, &Cell)> = if full {
            let mut v = Vec::with_capacity(buf.content.len());
            for y in 0..area.height {
                let mut skip = 0usize;
                for x in 0..area.width {
                    let cell = &buf.content[(y * area.width + x) as usize];
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    if cell.diff_option == ratatui::buffer::CellDiffOption::Skip {
                        continue;
                    }
                    skip = cell.symbol().width().saturating_sub(1);
                    v.push((x, y, cell));
                }
            }
            v
        } else {
            match &self.last {
                Some(l) => l.diff(buf),
                None => Vec::new(),
            }
        };
        if cells.is_empty() {
            self.last = Some(buf.clone());
            self.last_top = top;
            return false;
        }
        let mut s = String::new();
        let mut cursor: Option<(u16, u16)> = None;
        let mut cur_sgr = String::new();
        for (x, y, cell) in cells {
            if cursor != Some((x, y)) {
                let _ = write!(s, "\x1b[{};{}H", top + y + 1, area.x + x + 1);
            }
            let sgr = sgr_for(cell);
            if sgr != cur_sgr {
                s.push_str(&sgr);
                cur_sgr = sgr;
            }
            let sym = cell.symbol();
            let w = sym.width().max(1) as u16;
            if sym.is_empty() {
                s.push(' ');
            } else {
                s.push_str(sym);
            }
            cursor = Some((x + w, y));
        }
        s.push_str("\x1b[0m");
        out.extend_from_slice(s.as_bytes());
        self.last = Some(buf.clone());
        self.last_top = top;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_core::model::*;

    fn acct(label: &str, here: bool, five: f64, seven: f64) -> AccountView {
        AccountView {
            profile_id: label.to_lowercase(),
            label: label.into(),
            color: Some("cyan".into()),
            identity: AccountIdentity {
                subscription: Some("team".into()),
                ..Default::default()
            },
            token: TokenState::Valid { expires_at: 0 },
            usage: Some(UsageSnapshot {
                fetched_at: 0,
                five_hour: Some(Window {
                    utilization: five,
                    resets_at: Some(1000 + 2 * 3600 + 14 * 60),
                    inferred: false,
                }),
                seven_day: Some(Window {
                    utilization: seven,
                    resets_at: Some(1000 + 3 * 86400 + 4 * 3600),
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
                five_hour: Some(71.0),
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

    fn model(mode: DockMode) -> DockModel {
        let mut b = acct("Acme Bea", false, 1.0, 72.0);
        b.usage.as_mut().unwrap().scoped.clear();
        DockModel {
            accounts: vec![acct("Acme Al", true, 48.0, 31.0), b],
            mode,
            policy: Policy::Smart,
            session_short: Some("3f2a91c0".into()),
            on_account: Some(("Acme Al".into(), 0)),
            usage_age_secs: Some(40),
            message: None,
            countdown: None,
            menu_key_label: "⌃\\".into(),
            show_projections: true,
            icons: DockIcons::Unicode,
            now: 1000,
        }
    }

    fn rows(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    fn draw(m: &DockModel, w: u16, h: u16) -> Vec<String> {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        render_dock(m, area, &mut buf);
        let r = rows(&buf);
        for l in &r {
            assert!(l.width() <= w as usize, "overflow: {l}");
        }
        r
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(2 * 3600 + 14 * 60), "2h14m");
        assert_eq!(fmt_duration(3 * 86400 + 4 * 3600), "3d4h");
        assert_eq!(fmt_duration(21 * 3600 + 5), "21h");
        assert_eq!(fmt_duration(300), "5m");
    }

    #[test]
    fn full_120() {
        let r = draw(&model(DockMode::Full), 120, 5);
        assert!(r[0].contains("agent-prism · smart"), "{}", r[0]);
        assert!(r[1].contains("●  Acme Al"), "{}", r[1]);
        assert!(r[1].contains("team"));
        assert!(r[1].contains("48%→71%"), "{}", r[1]);
        assert!(r[1].contains("↻2h14m"));
        assert!(r[1].contains("↻3d4h"));
        assert!(r[1].contains("Fable 12%"), "{}", r[1]);
        assert!(r[1].contains('█'));
        assert!(r[2].contains("○  Acme Bea"));
        assert!(r[2].contains("Fable  —"), "{}", r[2]);
        assert!(r[2].contains("next ▸"), "{}", r[2]);
        assert!(r[3].contains("Σ  pool"), "{}", r[3]);
        assert!(r[4].contains("⌃\\ menu · session 3f2a91c0"), "{}", r[4]);
    }

    #[test]
    fn full_narrow_drops_columns() {
        let r = draw(&model(DockMode::Full), 80, 4);
        assert!(r[1].contains("48%") && r[1].contains("31%"), "{}", r[1]);
        assert!(!r[1].contains("→71%"));
        let r = draw(&model(DockMode::Full), 50, 4);
        assert!(r[1].contains("48%") && r[1].contains("31%"), "{}", r[1]);
        assert!(!r[1].contains('↻'));
    }

    #[test]
    fn compact_widths() {
        for w in [120, 80, 50] {
            let r = draw(&model(DockMode::Compact), w, 1);
            assert!(r[0].contains("●  Acme Al  5h 48% 7d 31%"), "{}", r[0]);
        }
        let r = draw(&model(DockMode::Compact), 120, 1);
        assert!(
            r[0].contains("○  Acme Bea  5h 1% 7d 72% ▸ │ Σ 5h 24% 7d 52% │ ⌃\\"),
            "{}",
            r[0]
        );
    }

    #[test]
    fn states_and_countdown() {
        let mut m = model(DockMode::Full);
        m.accounts[0].limited = Some(Limited {
            kind: LimitKind::FiveHour,
            until: Some(1000 + 3600 + 12 * 60),
        });
        m.accounts[1].token = TokenState::Expired {
            since: 1000 - 19 * 3600,
        };
        m.accounts[1]
            .usage
            .as_mut()
            .unwrap()
            .five_hour
            .as_mut()
            .unwrap()
            .inferred = true;
        m.countdown = Some(Countdown {
            from_label: "Acme Al".into(),
            to_label: "Acme Bea".into(),
            reason: "hit the 5h limit".into(),
            deadline: 1004,
        });
        let r = draw(&m, 120, 5);
        assert!(
            r[1].contains("✕") && r[1].contains("limit · resets 1h12m"),
            "{}",
            r[1]
        );
        assert!(
            r[2].contains("⚠") && r[2].contains("stale 19h") && r[2].contains("~1%"),
            "{}",
            r[2]
        );
        assert!(r[4].contains("Acme Al hit the 5h limit · moving to Acme Bea in 4s · ⌃\\ stay"));
        let r = draw(&m, 50, 1);
        assert!(r[0].contains("✕  Acme Al"));
    }

    #[test]
    fn pool_row() {
        let mut m = model(DockMode::Full);
        assert_eq!(dock_rows(&m), 3);
        // A 20x plan counts double: (2*48 + 1*1) / 3 = 32.3; projection
        // uses 71 for Al and 1 (current) for Bea: (2*71 + 1) / 3 = 47.7.
        m.accounts[0].identity.rate_limit_tier = Some("default_claude_max_20x".into());
        m.accounts[1].projection.five_hour = None;
        let r = draw(&m, 120, 5);
        assert!(r[3].contains("Σ  pool"), "{}", r[3]);
        assert!(r[3].contains("32%→48%"), "{}", r[3]);
        assert!(
            r[3].contains("↻2h14m") && r[3].contains("↻3d4h"),
            "{}",
            r[3]
        );
        assert!(r[3].contains("2 accounts"), "{}", r[3]);

        // One pool account: no pool row.
        m.accounts[1].in_pool = false;
        assert_eq!(dock_rows(&m), 2);
        let r = draw(&m, 120, 4);
        assert!(!r.iter().any(|l| l.contains("Σ")), "{r:?}");
        let r = draw(&model(DockMode::Compact), 120, 1);
        assert!(r[0].contains("Σ 5h 24%"), "{}", r[0]);
    }

    #[test]
    fn colors() {
        assert_eq!(account_color(Some("magenta"), 0), Color::Magenta);
        assert_eq!(account_color(Some("nope"), 1), account_color(None, 1));
    }

    #[test]
    fn painter() {
        let area = Rect::new(0, 0, 10, 2);
        let mut a = Buffer::empty(area);
        a.set_string(0, 0, "hello", Style::default());
        let mut p = DockPainter::new();
        let mut out = Vec::new();
        assert!(p.paint(&a, 20, &mut out));
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("\x1b[21;1H"));
        assert!(s.contains("hello") && s.contains("\x1b[22;1H"));
        assert!(s.ends_with("\x1b[0m"));
        assert!(!s.contains("\x1b7") && !s.contains("\x1b8"));

        let mut b = a.clone();
        b.set_string(1, 1, "X", Style::default().fg(Color::Red));
        let mut out = Vec::new();
        assert!(p.paint(&b, 20, &mut out));
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "\x1b[22;2H\x1b[0;31mX\x1b[0m");

        let mut out = Vec::new();
        assert!(!p.paint(&b, 20, &mut out));
        p.invalidate();
        assert!(p.paint(&b, 20, &mut out));
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s.matches(' ').count(), 10 + 9 - 5);

        // Wide chars.
        let mut c = Buffer::empty(area);
        c.set_string(0, 0, "日本", Style::default());
        let mut out = Vec::new();
        p.invalidate();
        p.paint(&c, 0, &mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("日本"));
        assert_eq!(s.matches(' ').count(), 6 + 10);
    }
}
