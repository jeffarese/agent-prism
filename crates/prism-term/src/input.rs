//! Input handling. See docs/DESIGN.md §7.5.
//!
//! [`InputScanner`] splits raw stdin bytes into bytes for the child, menu
//! key presses, clicks on the dock and (while a panel has focus) pastes.
//! [`KeyParser`] turns bytes into crossterm key and mouse events for panels.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

/// How long an incomplete escape sequence is held before it is flushed.
pub const PENDING_TIMEOUT: Duration = Duration::from_millis(20);

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Length of the next token in `buf`, or `None` when the token is incomplete
/// (the buffer ends inside it). `buf` must not be empty.
fn token_len(buf: &[u8]) -> Option<usize> {
    let b = buf[0];
    if b != 0x1b {
        let need = match b {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        if need == 1 {
            return Some(1);
        }
        // Stop at a non-continuation byte (invalid UTF-8 passes as is).
        for i in 1..need {
            match buf.get(i) {
                None => return None,
                Some(c) if c & 0xc0 != 0x80 => return Some(i),
                _ => {}
            }
        }
        return Some(need);
    }
    let &b1 = buf.get(1)?;
    match b1 {
        b'[' => {
            let mut i = 2;
            loop {
                let &c = buf.get(i)?;
                match c {
                    0x20..=0x3f => i += 1,
                    0x40..=0x7e => {
                        // Legacy X10 mouse: CSI M b x y.
                        if c == b'M' && i == 2 {
                            return if buf.len() >= 6 { Some(6) } else { None };
                        }
                        return Some(i + 1);
                    }
                    _ => return Some(i), // malformed: end before the byte
                }
            }
        }
        b'O' => {
            if buf.len() >= 3 {
                Some(3)
            } else {
                None
            }
        }
        b']' | b'P' | b'_' | b'^' | b'X' => {
            let mut i = 2;
            while i < buf.len() {
                match buf[i] {
                    0x07 => return Some(i + 1),
                    0x1b => {
                        return match buf.get(i + 1) {
                            None => None,
                            Some(b'\\') => Some(i + 2),
                            Some(_) => Some(i),
                        };
                    }
                    _ => i += 1,
                }
            }
            None
        }
        0x1b => Some(1),
        _ => token_len(&buf[1..]).map(|n| n + 1),
    }
}

/// Split a CSI token (`ESC [ ... final`) into (private prefix, params, final).
fn split_csi(tok: &[u8]) -> Option<(Option<u8>, &[u8], u8)> {
    if tok.len() < 3 || tok[0] != 0x1b || tok[1] != b'[' {
        return None;
    }
    let fin = *tok.last()?;
    let mut body = &tok[2..tok.len() - 1];
    let private = match body.first() {
        Some(&p @ (b'?' | b'<' | b'>' | b'=')) => {
            body = &body[1..];
            Some(p)
        }
        _ => None,
    };
    Some((private, body, fin))
}

fn num(s: &[u8]) -> Option<u32> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// Parameters split on `;`, each with its `:` sub-parameters.
fn params(body: &[u8]) -> Vec<Vec<Option<u32>>> {
    body.split(|&b| b == b';')
        .map(|p| p.split(|&b| b == b':').map(num).collect())
        .collect()
}

/// Kitty / xterm modifier value → (is ctrl-only, ignoring lock bits).
fn is_ctrl_only(mods: u32) -> bool {
    mods >= 1 && (mods - 1) & !(64 | 128) == 4
}

/// What a menu-key-like token is.
#[derive(Debug, PartialEq, Eq)]
enum MenuMatch {
    No,
    Press,
    Release,
}

fn menu_key(tok: &[u8]) -> MenuMatch {
    if tok == [0x1c] {
        return MenuMatch::Press;
    }
    let Some((None, body, fin)) = split_csi(tok) else {
        return MenuMatch::No;
    };
    let p = params(body);
    match fin {
        b'u' => {
            let code = p.first().and_then(|v| v.first().copied().flatten());
            let mods = p.get(1).and_then(|v| v.first().copied().flatten());
            let event = p.get(1).and_then(|v| v.get(1).copied().flatten());
            if code == Some(92) && mods.is_some_and(is_ctrl_only) {
                if event == Some(3) {
                    return MenuMatch::Release;
                }
                return MenuMatch::Press;
            }
            MenuMatch::No
        }
        b'~' => {
            let f = |i: usize| p.get(i).and_then(|v| v.first().copied().flatten());
            if p.len() == 3
                && f(0) == Some(27)
                && f(1).is_some_and(is_ctrl_only)
                && f(2) == Some(92)
            {
                MenuMatch::Press
            } else {
                MenuMatch::No
            }
        }
        _ => MenuMatch::No,
    }
}

/// An SGR mouse report: (button code, 1-based x, 1-based y, pressed).
fn sgr_mouse(tok: &[u8]) -> Option<(u16, u16, u16, bool)> {
    let (Some(b'<'), body, fin @ (b'M' | b'm')) = split_csi(tok)? else {
        return None;
    };
    let mut it = body.split(|&b| b == b';');
    let b = num(it.next()?)?;
    let x = num(it.next()?)?;
    let y = num(it.next()?)?;
    let c = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
    Some((c(b), c(x), c(y), fin == b'M'))
}

/// One item produced by [`InputScanner`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputItem {
    /// Bytes for the child (or, while a panel has focus, for the panel's
    /// key parser).
    Forward(Vec<u8>),
    /// The menu key was pressed.
    MenuKey,
    /// An SGR mouse report on a dock row. `row` is 0-based inside the dock,
    /// `col` 0-based; `button` is the raw SGR button code.
    DockMouse {
        button: u16,
        col: u16,
        row: u16,
        pressed: bool,
    },
    /// A bracketed paste while a panel has focus.
    Paste(String),
}

/// Splits stdin bytes into [`InputItem`]s. See the module docs.
#[derive(Debug, Default)]
pub struct InputScanner {
    pending: Vec<u8>,
    pending_since: Option<Instant>,
    /// Inside a bracketed paste (child mode: forwarded raw).
    in_paste: bool,
    paste: Vec<u8>,
    panel_focus: bool,
}

impl InputScanner {
    /// A new scanner.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a panel has keyboard focus (pastes become [`InputItem::Paste`]).
    pub fn set_panel_focus(&mut self, focus: bool) {
        self.panel_focus = focus;
    }

    /// When the held incomplete sequence must be flushed, if any.
    pub fn pending_deadline(&self) -> Option<Instant> {
        self.pending_since.map(|t| t + PENDING_TIMEOUT)
    }

    /// Return the held incomplete sequence as a forward item.
    pub fn flush_pending(&mut self) -> Option<InputItem> {
        self.pending_since = None;
        if self.pending.is_empty() {
            return None;
        }
        Some(InputItem::Forward(std::mem::take(&mut self.pending)))
    }

    /// Scan `bytes`; `child_rows` is the child's row count (mouse rows above
    /// it belong to the child).
    pub fn feed(&mut self, bytes: &[u8], child_rows: u16, now: Instant) -> Vec<InputItem> {
        let mut items = Vec::new();
        let mut fwd: Vec<u8> = Vec::new();
        let owned;
        let buf: &[u8] = if self.pending.is_empty() {
            bytes
        } else {
            self.pending.extend_from_slice(bytes);
            owned = std::mem::take(&mut self.pending);
            &owned
        };
        self.pending_since = None;
        let mut i = 0;
        while i < buf.len() {
            if self.in_paste {
                // Look for the end marker; everything before it is paste.
                let rest = &buf[i..];
                match find(rest, PASTE_END) {
                    Some(p) => {
                        let end = p + PASTE_END.len();
                        if self.panel_focus {
                            self.paste.extend_from_slice(&rest[..p]);
                            let text = String::from_utf8_lossy(&self.paste).into_owned();
                            self.paste.clear();
                            items.push(InputItem::Paste(text));
                        } else {
                            fwd.extend_from_slice(&rest[..end]);
                        }
                        self.in_paste = false;
                        i += end;
                        continue;
                    }
                    None => {
                        // Keep a possible partial end marker for later.
                        let keep = partial_suffix(rest, PASTE_END);
                        let take = rest.len() - keep;
                        if self.panel_focus {
                            self.paste.extend_from_slice(&rest[..take]);
                        } else {
                            fwd.extend_from_slice(&rest[..take]);
                        }
                        if keep > 0 {
                            self.pending.extend_from_slice(&rest[take..]);
                            self.pending_since = Some(now);
                        }
                        break;
                    }
                }
            }
            let Some(n) = token_len(&buf[i..]) else {
                self.pending.extend_from_slice(&buf[i..]);
                self.pending_since = Some(now);
                break;
            };
            let tok = &buf[i..i + n];
            i += n;
            if tok == PASTE_START {
                self.in_paste = true;
                if !self.panel_focus {
                    fwd.extend_from_slice(tok);
                }
                continue;
            }
            match menu_key(tok) {
                MenuMatch::Press => {
                    flush_fwd(&mut fwd, &mut items);
                    items.push(InputItem::MenuKey);
                    continue;
                }
                MenuMatch::Release => continue,
                MenuMatch::No => {}
            }
            if let Some((button, x, y, pressed)) = sgr_mouse(tok)
                && y > child_rows
            {
                flush_fwd(&mut fwd, &mut items);
                items.push(InputItem::DockMouse {
                    button,
                    col: x.saturating_sub(1),
                    row: y - 1 - child_rows,
                    pressed,
                });
                continue;
            }
            fwd.extend_from_slice(tok);
        }
        flush_fwd(&mut fwd, &mut items);
        items
    }
}

fn flush_fwd(fwd: &mut Vec<u8>, items: &mut Vec<InputItem>) {
    if fwd.is_empty() {
        return;
    }
    if let Some(InputItem::Forward(prev)) = items.last_mut() {
        prev.append(fwd);
    } else {
        items.push(InputItem::Forward(std::mem::take(fwd)));
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Length of the longest suffix of `hay` that is a proper prefix of `needle`.
fn partial_suffix(hay: &[u8], needle: &[u8]) -> usize {
    (1..needle.len())
        .rev()
        .find(|&k| hay.len() >= k && hay[hay.len() - k..] == needle[..k])
        .unwrap_or(0)
}

/// An event parsed by [`KeyParser`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
}

/// Turns bytes into key, mouse and paste events for panels.
#[derive(Debug, Default)]
pub struct KeyParser {
    pending: Vec<u8>,
    in_paste: bool,
    paste: Vec<u8>,
}

fn mods_from(m: u32) -> KeyModifiers {
    let bits = m.saturating_sub(1);
    let mut k = KeyModifiers::empty();
    if bits & 1 != 0 {
        k |= KeyModifiers::SHIFT;
    }
    if bits & 2 != 0 {
        k |= KeyModifiers::ALT;
    }
    if bits & 4 != 0 {
        k |= KeyModifiers::CONTROL;
    }
    if bits & 8 != 0 {
        k |= KeyModifiers::SUPER;
    }
    k
}

fn key(code: KeyCode, mods: KeyModifiers) -> Option<ParsedEvent> {
    Some(ParsedEvent::Key(KeyEvent::new(code, mods)))
}

fn codepoint_key(cp: u32, mods: KeyModifiers) -> Option<ParsedEvent> {
    let code = match cp {
        13 => KeyCode::Enter,
        9 if mods.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        9 => KeyCode::Tab,
        27 => KeyCode::Esc,
        127 | 8 => KeyCode::Backspace,
        57344..=63743 => return None, // kitty functional keys (PUA): ignore
        _ => {
            let c = char::from_u32(cp)?;
            if mods.contains(KeyModifiers::SHIFT) && c.is_ascii_lowercase() {
                KeyCode::Char(c.to_ascii_uppercase())
            } else {
                KeyCode::Char(c)
            }
        }
    };
    key(code, mods)
}

fn single_byte_key(b: u8) -> Option<ParsedEvent> {
    let none = KeyModifiers::NONE;
    let ctrl = KeyModifiers::CONTROL;
    match b {
        b'\r' | b'\n' => key(KeyCode::Enter, none),
        b'\t' => key(KeyCode::Tab, none),
        0x7f | 0x08 => key(KeyCode::Backspace, none),
        0x1b => key(KeyCode::Esc, none),
        0x00 => key(KeyCode::Char(' '), ctrl),
        0x01..=0x1a => key(KeyCode::Char((b - 1 + b'a') as char), ctrl),
        0x1c => key(KeyCode::Char('\\'), ctrl),
        0x1d => key(KeyCode::Char(']'), ctrl),
        0x1e => key(KeyCode::Char('^'), ctrl),
        0x1f => key(KeyCode::Char('_'), ctrl),
        _ => None,
    }
}

fn plain_token(tok: &[u8]) -> Option<ParsedEvent> {
    if tok.len() == 1 && tok[0] < 0x80 && !(0x20..0x7f).contains(&tok[0]) {
        return single_byte_key(tok[0]);
    }
    let s = std::str::from_utf8(tok).ok()?;
    let c = s.chars().next()?;
    key(KeyCode::Char(c), KeyModifiers::NONE)
}

fn csi_key(tok: &[u8]) -> Option<ParsedEvent> {
    let (private, body, fin) = split_csi(tok)?;
    if private == Some(b'<') {
        let (b, x, y, pressed) = sgr_mouse(tok)?;
        let mut modifiers = KeyModifiers::empty();
        if b & 4 != 0 {
            modifiers |= KeyModifiers::SHIFT;
        }
        if b & 8 != 0 {
            modifiers |= KeyModifiers::ALT;
        }
        if b & 16 != 0 {
            modifiers |= KeyModifiers::CONTROL;
        }
        let button = match b & 3 {
            0 => MouseButton::Left,
            1 => MouseButton::Middle,
            _ => MouseButton::Right,
        };
        let kind = if b & 64 != 0 {
            match b & 3 {
                0 => MouseEventKind::ScrollUp,
                1 => MouseEventKind::ScrollDown,
                2 => MouseEventKind::ScrollLeft,
                _ => MouseEventKind::ScrollRight,
            }
        } else if b & 32 != 0 {
            if b & 3 == 3 {
                MouseEventKind::Moved
            } else {
                MouseEventKind::Drag(button)
            }
        } else if pressed {
            MouseEventKind::Down(button)
        } else {
            MouseEventKind::Up(button)
        };
        return Some(ParsedEvent::Mouse(MouseEvent {
            kind,
            column: x.saturating_sub(1),
            row: y.saturating_sub(1),
            modifiers,
        }));
    }
    if private.is_some() {
        return None; // terminal replies
    }
    let p = params(body);
    let first = |i: usize| p.get(i).and_then(|v| v.first().copied().flatten());
    let mods = mods_from(first(1).unwrap_or(1));
    match fin {
        b'A' => key(KeyCode::Up, mods),
        b'B' => key(KeyCode::Down, mods),
        b'C' => key(KeyCode::Right, mods),
        b'D' => key(KeyCode::Left, mods),
        b'H' => key(KeyCode::Home, mods),
        b'F' => key(KeyCode::End, mods),
        b'Z' => key(KeyCode::BackTab, KeyModifiers::SHIFT),
        b'P' => key(KeyCode::F(1), mods),
        b'Q' => key(KeyCode::F(2), mods),
        b'S' => key(KeyCode::F(4), mods),
        b'u' => {
            let event = p.get(1).and_then(|v| v.get(1).copied().flatten());
            if event == Some(3) {
                return None;
            }
            codepoint_key(first(0)?, mods)
        }
        b'~' => {
            let code = match first(0)? {
                27 => return codepoint_key(first(2)?, mods),
                1 | 7 => KeyCode::Home,
                2 => KeyCode::Insert,
                3 => KeyCode::Delete,
                4 | 8 => KeyCode::End,
                5 => KeyCode::PageUp,
                6 => KeyCode::PageDown,
                n @ 11..=15 => KeyCode::F((n - 10) as u8),
                n @ 17..=21 => KeyCode::F((n - 11) as u8),
                n @ 23..=24 => KeyCode::F((n - 12) as u8),
                _ => return None,
            };
            key(code, mods)
        }
        _ => None,
    }
}

fn token_event(tok: &[u8]) -> Option<ParsedEvent> {
    if tok[0] != 0x1b || tok.len() == 1 {
        return plain_token(tok);
    }
    match tok[1] {
        b'[' => csi_key(tok),
        b'O' if tok.len() == 3 => {
            let code = match tok[2] {
                b'A' => KeyCode::Up,
                b'B' => KeyCode::Down,
                b'C' => KeyCode::Right,
                b'D' => KeyCode::Left,
                b'H' => KeyCode::Home,
                b'F' => KeyCode::End,
                b'M' => KeyCode::Enter,
                b'P' => KeyCode::F(1),
                b'Q' => KeyCode::F(2),
                b'R' => KeyCode::F(3),
                b'S' => KeyCode::F(4),
                _ => return None,
            };
            key(code, KeyModifiers::NONE)
        }
        b']' | b'P' | b'_' | b'^' | b'X' => None,
        _ => match token_event(&tok[1..])? {
            ParsedEvent::Key(mut k) => {
                k.modifiers |= KeyModifiers::ALT;
                Some(ParsedEvent::Key(k))
            }
            _ => None,
        },
    }
}

impl KeyParser {
    /// A new parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse `bytes`. An incomplete trailing sequence is held; call
    /// [`flush`](Self::flush) when no more bytes follow soon.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<ParsedEvent> {
        self.pending.extend_from_slice(bytes);
        let buf = std::mem::take(&mut self.pending);
        let mut out = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            if self.in_paste {
                let rest = &buf[i..];
                if let Some(p) = find(rest, PASTE_END) {
                    self.paste.extend_from_slice(&rest[..p]);
                    out.push(ParsedEvent::Paste(
                        String::from_utf8_lossy(&self.paste).into_owned(),
                    ));
                    self.paste.clear();
                    self.in_paste = false;
                    i += p + PASTE_END.len();
                    continue;
                }
                let keep = partial_suffix(rest, PASTE_END);
                self.paste.extend_from_slice(&rest[..rest.len() - keep]);
                self.pending.extend_from_slice(&rest[rest.len() - keep..]);
                return out;
            }
            let Some(n) = token_len(&buf[i..]) else {
                self.pending.extend_from_slice(&buf[i..]);
                return out;
            };
            let tok = &buf[i..i + n];
            i += n;
            if tok == PASTE_START {
                self.in_paste = true;
                continue;
            }
            if let Some(ev) = token_event(tok) {
                out.push(ev);
            }
        }
        out
    }

    /// Resolve the held bytes: a lone ESC becomes an Esc key, ESC+`[`
    /// becomes Alt+`[`, anything else is dropped.
    pub fn flush(&mut self) -> Vec<ParsedEvent> {
        let p = std::mem::take(&mut self.pending);
        if self.in_paste {
            self.paste.extend_from_slice(&p);
            return Vec::new();
        }
        match p.as_slice() {
            [0x1b] => vec![ParsedEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            ))],
            [0x1b, c] if c.is_ascii_graphic() => vec![ParsedEvent::Key(KeyEvent::new(
                KeyCode::Char(*c as char),
                KeyModifiers::ALT,
            ))],
            _ => Vec::new(),
        }
    }

    /// True when bytes are held.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(bytes: &[u8], rows: u16) -> Vec<InputItem> {
        InputScanner::new().feed(bytes, rows, Instant::now())
    }
    fn fwd(b: &[u8]) -> InputItem {
        InputItem::Forward(b.to_vec())
    }

    #[test]
    fn menu_key_forms() {
        for k in [
            &b"\x1c"[..],
            b"\x1b[92;5u",
            b"\x1b[92;5:1u",
            b"\x1b[92;5:2u",
            b"\x1b[92;69u",
            b"\x1b[27;5;92~",
        ] {
            assert_eq!(scan(k, 20), vec![InputItem::MenuKey], "{k:?}");
        }
        assert_eq!(scan(b"\x1b[92;5:3u", 20), vec![]);
        assert_eq!(scan(b"\x1b[92;6u", 20), vec![fwd(b"\x1b[92;6u")]);
        assert_eq!(
            scan(b"ab\x1cc", 20),
            vec![fwd(b"ab"), InputItem::MenuKey, fwd(b"c")]
        );
    }

    #[test]
    fn mouse() {
        assert_eq!(scan(b"\x1b[<0;5;20M", 20), vec![fwd(b"\x1b[<0;5;20M")]);
        assert_eq!(
            scan(b"\x1b[<0;5;22m", 20),
            vec![InputItem::DockMouse {
                button: 0,
                col: 4,
                row: 1,
                pressed: false
            }]
        );
    }

    #[test]
    fn replies_forwarded() {
        for r in [
            &b"\x1b[?1;2c"[..],
            b"\x1b[12;40R",
            b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\",
            b"\x1b]10;rgb:ffff/ffff/ffff\x07",
            b"\x1b[?2026;2$y",
            b"\x1b[?31u",
            b"\x1b[A\x1bOA hello \xe2\x9c\x93",
        ] {
            assert_eq!(scan(r, 20), vec![fwd(r)], "{r:?}");
        }
    }

    #[test]
    fn partial() {
        let t0 = Instant::now();
        let mut s = InputScanner::new();
        assert_eq!(s.feed(b"x\x1b", 20, t0), vec![fwd(b"x")]);
        assert_eq!(s.pending_deadline(), Some(t0 + PENDING_TIMEOUT));
        assert_eq!(s.flush_pending(), Some(fwd(b"\x1b")));
        assert_eq!(s.pending_deadline(), None);
        assert_eq!(s.feed(b"\x1b[92", 20, t0), vec![]);
        assert_eq!(s.feed(b";5u", 20, t0), vec![InputItem::MenuKey]);
        assert_eq!(s.feed(b"\x1b]11;rgb:0/0", 20, t0), vec![]);
        assert_eq!(
            s.feed(b"/0\x07", 20, t0),
            vec![fwd(b"\x1b]11;rgb:0/0/0\x07")]
        );
        assert_eq!(s.feed(b"\xe2\x9c", 20, t0), vec![]);
        assert_eq!(s.feed(b"\x93", 20, t0), vec![fwd("✓".as_bytes())]);
    }

    #[test]
    fn paste() {
        let mut s = InputScanner::new();
        let t = Instant::now();
        let p = b"\x1b[200~a\x1cb\x1b[201~";
        assert_eq!(s.feed(p, 20, t), vec![fwd(p)]);
        s.set_panel_focus(true);
        assert_eq!(s.feed(b"\x1b[200~hi\x1b[20", 20, t), vec![]);
        assert_eq!(s.feed(b"1~", 20, t), vec![InputItem::Paste("hi".into())]);
    }

    fn keys(b: &[u8]) -> Vec<ParsedEvent> {
        let mut k = KeyParser::new();
        let mut v = k.feed(b);
        v.extend(k.flush());
        v
    }
    fn k(code: KeyCode, m: KeyModifiers) -> ParsedEvent {
        ParsedEvent::Key(KeyEvent::new(code, m))
    }

    #[test]
    fn key_parser() {
        let n = KeyModifiers::NONE;
        let c = KeyModifiers::CONTROL;
        let a = KeyModifiers::ALT;
        let s = KeyModifiers::SHIFT;
        assert_eq!(
            keys("aé".as_bytes()),
            vec![k(KeyCode::Char('a'), n), k(KeyCode::Char('é'), n)]
        );
        assert_eq!(keys(b"\r"), vec![k(KeyCode::Enter, n)]);
        assert_eq!(keys(b"\x1b"), vec![k(KeyCode::Esc, n)]);
        assert_eq!(keys(b"\x1b[27u"), vec![k(KeyCode::Esc, n)]);
        assert_eq!(
            keys(b"\t\x1b[Z"),
            vec![k(KeyCode::Tab, n), k(KeyCode::BackTab, s)]
        );
        assert_eq!(keys(b"\x7f"), vec![k(KeyCode::Backspace, n)]);
        assert_eq!(
            keys(b"\x1b[A\x1bOB"),
            vec![k(KeyCode::Up, n), k(KeyCode::Down, n)]
        );
        assert_eq!(keys(b"\x1b[1;5C"), vec![k(KeyCode::Right, c)]);
        assert_eq!(
            keys(b"\x1b[H\x1b[F\x1b[5~\x1b[6~\x1b[3~"),
            vec![
                k(KeyCode::Home, n),
                k(KeyCode::End, n),
                k(KeyCode::PageUp, n),
                k(KeyCode::PageDown, n),
                k(KeyCode::Delete, n)
            ]
        );
        assert_eq!(keys(b"\x03"), vec![k(KeyCode::Char('c'), c)]);
        assert_eq!(keys(b"\x1bx"), vec![k(KeyCode::Char('x'), a)]);
        assert_eq!(keys(b"\x1b[97;5u"), vec![k(KeyCode::Char('a'), c)]);
        assert_eq!(keys(b"\x1b[97;5:3u"), vec![]);
        assert_eq!(keys(b"\x1c"), vec![k(KeyCode::Char('\\'), c)]);
        assert_eq!(
            keys(b"\x1b[<0;3;4M"),
            vec![ParsedEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 3,
                modifiers: n
            })]
        );
        assert_eq!(
            keys(b"\x1b[200~x\ny\x1b[201~q"),
            vec![ParsedEvent::Paste("x\ny".into()), k(KeyCode::Char('q'), n)]
        );
        assert_eq!(keys(b"\x1b[?1;2c\x1b[12;40R"), vec![]);
    }
}
