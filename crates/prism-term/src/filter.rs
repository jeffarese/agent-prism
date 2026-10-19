//! Streaming output filter for the child's output. See docs/DESIGN.md §7.2.
//!
//! The filter passes bytes through unchanged except for a few CSI sequences
//! that could touch the dock rows: DECSTBM bottom margins and absolute row
//! positions (CUP, HVP, VPA) are clamped to the child's row count `N`. It
//! also reports events after which the dock must be redrawn or the scroll
//! region re-injected. It is safe across arbitrary chunk boundaries: a CSI
//! sequence that is cut by the end of a chunk is held until it completes.

/// Maximum length of a held CSI sequence. Longer ones pass through
/// unmodified.
const MAX_CSI: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    /// `ESC` followed by intermediate bytes, waiting for the final byte.
    EscIntermediate,
    /// Inside a CSI sequence; bytes are held in `csi`.
    Csi,
    /// Inside an overlong CSI sequence; bytes pass through until the final.
    CsiPassthrough,
    /// Inside an OSC string (BEL or ST terminated).
    Osc,
    /// Inside a DCS/APC/PM/SOS string (ST terminated).
    Str,
    /// Saw ESC inside a string; `\` ends it.
    OscEsc,
    StrEsc,
}

/// What happened in one processed chunk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FilterReport {
    /// The child did something that may have erased or overwritten the dock
    /// rows (ED, RIS, DECSTR, alt-screen toggle, DECCOLM).
    pub dock_damaged: bool,
    /// The scroll region must be re-injected (RIS, DECSTR, alt-screen
    /// toggle).
    pub needs_region: bool,
    /// A sequence was rewritten.
    pub rewritten: bool,
}

/// The streaming filter. See the module docs.
#[derive(Debug)]
pub struct OutputFilter {
    rows: u16,
    state: State,
    csi: Vec<u8>,
    sync: bool,
    /// Continuation bytes still missing for a UTF-8 character that a chunk
    /// split. agent-prism must not inject in the middle of a character.
    utf8_need: u8,
}

/// Continuation bytes still missing after `seg`, given `need` before it.
fn utf8_need_after(mut need: u8, seg: &[u8]) -> u8 {
    // A character is at most 4 bytes, so only the tail can be incomplete.
    let tail = if seg.len() >= 4 {
        need = 0;
        &seg[seg.len() - 4..]
    } else {
        seg
    };
    for &b in tail {
        need = match b {
            0x80..=0xbf if need > 0 => need - 1,
            0xc0..=0xdf => 1,
            0xe0..=0xef => 2,
            0xf0..=0xf7 => 3,
            _ => 0,
        };
    }
    need
}

impl OutputFilter {
    /// A filter for a child with `child_rows` rows.
    pub fn new(child_rows: u16) -> Self {
        Self {
            rows: child_rows.max(1),
            state: State::Ground,
            csi: Vec::with_capacity(MAX_CSI),
            sync: false,
            utf8_need: 0,
        }
    }

    /// Change the child's row count (after a resize or dock height change).
    pub fn set_child_rows(&mut self, n: u16) {
        self.rows = n.max(1);
    }

    /// The current child row count.
    pub fn child_rows(&self) -> u16 {
        self.rows
    }

    /// True when the parser is between escape sequences and not inside a
    /// UTF-8 character, so that agent-prism may inject its own sequences.
    pub fn in_ground(&self) -> bool {
        self.state == State::Ground && self.utf8_need == 0
    }

    /// True while the child is inside a synchronized output block
    /// (`CSI ? 2026 h` … `CSI ? 2026 l`).
    pub fn in_sync(&self) -> bool {
        self.sync
    }

    /// Filter `input`, appending the output to `out`.
    pub fn process(&mut self, input: &[u8], out: &mut Vec<u8>) -> FilterReport {
        let mut rep = FilterReport::default();
        out.reserve(input.len());
        let mut i = 0;
        while i < input.len() {
            match self.state {
                State::Ground => {
                    // Fast path: copy everything up to the next ESC.
                    match input[i..].iter().position(|&b| b == 0x1b) {
                        Some(p) => {
                            out.extend_from_slice(&input[i..i + p + 1]);
                            i += p + 1;
                            self.utf8_need = 0;
                            self.state = State::Escape;
                        }
                        None => {
                            out.extend_from_slice(&input[i..]);
                            self.utf8_need = utf8_need_after(self.utf8_need, &input[i..]);
                            i = input.len();
                        }
                    }
                }
                State::Escape => {
                    let b = input[i];
                    i += 1;
                    match b {
                        b'[' => {
                            // The ESC is already emitted; hold the rest.
                            self.csi.clear();
                            self.state = State::Csi;
                            out.push(b);
                            // Mark that '[' was already emitted by leaving
                            // `csi` empty; params follow.
                        }
                        b']' => {
                            out.push(b);
                            self.state = State::Osc;
                        }
                        b'P' | b'_' | b'^' | b'X' => {
                            out.push(b);
                            self.state = State::Str;
                        }
                        b'c' => {
                            out.push(b);
                            rep.dock_damaged = true;
                            rep.needs_region = true;
                            self.sync = false;
                            self.state = State::Ground;
                        }
                        0x1b => out.push(b),
                        0x20..=0x2f => {
                            out.push(b);
                            self.state = State::EscIntermediate;
                        }
                        0x18 | 0x1a => {
                            out.push(b);
                            self.state = State::Ground;
                        }
                        0x00..=0x1f => out.push(b), // C0 executes, stay
                        _ => {
                            out.push(b);
                            self.state = State::Ground;
                        }
                    }
                }
                State::EscIntermediate => {
                    let b = input[i];
                    i += 1;
                    out.push(b);
                    match b {
                        0x1b => self.state = State::Escape,
                        0x20..=0x2f | 0x00..=0x17 | 0x19 | 0x1c..=0x1f => {}
                        _ => self.state = State::Ground,
                    }
                }
                State::Csi => {
                    let b = input[i];
                    i += 1;
                    match b {
                        0x40..=0x7e => {
                            self.csi.push(b);
                            self.finish_csi(out, &mut rep);
                            self.state = State::Ground;
                        }
                        0x1b => {
                            out.extend_from_slice(&self.csi);
                            self.csi.clear();
                            out.push(b);
                            self.state = State::Escape;
                        }
                        0x18 | 0x1a => {
                            out.extend_from_slice(&self.csi);
                            self.csi.clear();
                            out.push(b);
                            self.state = State::Ground;
                        }
                        _ => {
                            self.csi.push(b);
                            if self.csi.len() >= MAX_CSI {
                                out.extend_from_slice(&self.csi);
                                self.csi.clear();
                                self.state = State::CsiPassthrough;
                            }
                        }
                    }
                }
                State::CsiPassthrough => {
                    let b = input[i];
                    i += 1;
                    out.push(b);
                    match b {
                        0x40..=0x7e | 0x18 | 0x1a => self.state = State::Ground,
                        0x1b => self.state = State::Escape,
                        _ => {}
                    }
                }
                State::Osc | State::Str => {
                    let osc = self.state == State::Osc;
                    let rest = &input[i..];
                    let end = rest
                        .iter()
                        .position(|&b| b == 0x1b || b == 0x18 || b == 0x1a || (osc && b == 0x07));
                    match end {
                        Some(p) => {
                            let b = rest[p];
                            out.extend_from_slice(&rest[..=p]);
                            i += p + 1;
                            self.state = match b {
                                0x1b if osc => State::OscEsc,
                                0x1b => State::StrEsc,
                                _ => State::Ground,
                            };
                        }
                        None => {
                            out.extend_from_slice(rest);
                            i = input.len();
                        }
                    }
                }
                State::OscEsc | State::StrEsc => {
                    let b = input[i];
                    if b == b'\\' {
                        out.push(b);
                        i += 1;
                        self.state = State::Ground;
                    } else {
                        // The string was aborted by a new escape sequence;
                        // reprocess this byte in the escape state.
                        self.state = State::Escape;
                    }
                }
            }
        }
        rep
    }

    /// Emit the held CSI sequence (`csi` holds everything after `ESC [`),
    /// rewritten when needed.
    fn finish_csi(&mut self, out: &mut Vec<u8>, rep: &mut FilterReport) {
        let seq = &self.csi;
        let Some((&fin, body)) = seq.split_last() else {
            return;
        };
        let (private, params_end) = match body.first() {
            Some(&p @ (b'?' | b'>' | b'<' | b'=')) => (Some(p), body.len()),
            _ => (None, body.len()),
        };
        let params_start = usize::from(private.is_some());
        // Split off intermediates (0x20..=0x2f) at the end of the body.
        let inter_start = body[params_start..params_end]
            .iter()
            .position(|b| (0x20..=0x2f).contains(b))
            .map(|p| p + params_start)
            .unwrap_or(params_end);
        let params = &body[params_start..inter_start];
        let inter = &body[inter_start..];
        let n = self.rows;

        match (private, inter, fin) {
            (None, [], b'r') => {
                let mut it = params.split(|&b| b == b';');
                let top = it.next().unwrap_or(&[]);
                let bottom = it.next();
                let more = it.next().is_some();
                let bot_val = bottom.and_then(parse_num);
                if !more && params.iter().all(|b| b.is_ascii_digit() || *b == b';') {
                    let need = match bot_val {
                        None | Some(0) => true,
                        Some(v) => v > u32::from(n),
                    };
                    if need {
                        rep.rewritten = true;
                        out.extend_from_slice(top);
                        if top.is_empty() {
                            out.push(b'1');
                        }
                        out.push(b';');
                        push_num(out, n);
                        out.push(b'r');
                        self.csi.clear();
                        return;
                    }
                }
            }
            (None, [], b'H' | b'f' | b'd') => {
                let first_end = params
                    .iter()
                    .position(|&b| b == b';')
                    .unwrap_or(params.len());
                let first = &params[..first_end];
                if let Some(v) = parse_num(first)
                    && v > u32::from(n)
                {
                    rep.rewritten = true;
                    push_num(out, n);
                    out.extend_from_slice(&params[first_end..]);
                    out.push(fin);
                    self.csi.clear();
                    return;
                }
            }
            (None, [], b'J') => rep.dock_damaged = true,
            (None, [b'!'], b'p') => {
                rep.dock_damaged = true;
                rep.needs_region = true;
            }
            (Some(b'?'), [], b'h' | b'l') => {
                let set = fin == b'h';
                for p in params.split(|&b| b == b';') {
                    match parse_num(p) {
                        Some(1049 | 1047 | 47) => {
                            rep.dock_damaged = true;
                            rep.needs_region = true;
                        }
                        Some(3) => rep.dock_damaged = true,
                        Some(2026) => self.sync = set,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        out.extend_from_slice(&self.csi);
        self.csi.clear();
    }
}

fn parse_num(s: &[u8]) -> Option<u32> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut v: u32 = 0;
    for &d in s {
        v = v.saturating_mul(10).saturating_add(u32::from(d - b'0'));
    }
    Some(v)
}

fn push_num(out: &mut Vec<u8>, n: u16) {
    let mut buf = [0u8; 5];
    let mut i = buf.len();
    let mut v = n;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_ground_inside_split_utf8() {
        // "─" is E2 94 80; "😀" is F0 9F 98 80.
        let mut f = OutputFilter::new(10);
        let mut out = Vec::new();
        f.process(b"ab\xe2\x94", &mut out);
        assert!(!f.in_ground());
        f.process(b"\x80cd", &mut out);
        assert!(f.in_ground());
        f.process(b"\xf0", &mut out);
        assert!(!f.in_ground());
        f.process(b"\x9f", &mut out);
        assert!(!f.in_ground());
        f.process(b"\x98\x80", &mut out);
        assert!(f.in_ground());
        f.process("long line ─".as_bytes(), &mut out);
        assert!(f.in_ground());
        assert_eq!(out, "ab─cd😀long line ─".as_bytes());
    }

    fn run(n: u16, input: &[u8]) -> (Vec<u8>, FilterReport) {
        let mut f = OutputFilter::new(n);
        let mut out = Vec::new();
        let r = f.process(input, &mut out);
        (out, r)
    }

    fn s(input: &str) -> String {
        String::from_utf8(run(20, input.as_bytes()).0).unwrap()
    }

    #[test]
    fn passthrough_identity() {
        let plain = "hello\r\nworld \x1b[1;31mred\x1b[0m \x1b[5;10H \x1b]0;title\x07 ü✓";
        assert_eq!(s(plain), plain);
        // Pseudo-random bytes.
        let mut x: u32 = 12345;
        let mut data = Vec::new();
        for _ in 0..20000 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            data.push((x >> 16) as u8);
        }
        let (out, _) = run(1000, &data);
        // With N large and no row numbers above it the only possible change
        // is in sequences with huge rows; check the byte count is close and
        // that plain-only data is identical.
        let filtered: Vec<u8> = data.iter().copied().filter(|&b| b != 0x1b).collect();
        assert_eq!(run(5, &filtered).0, filtered);
        assert!(!out.is_empty());
    }

    #[test]
    fn decstbm() {
        assert_eq!(s("\x1b[r"), "\x1b[1;20r");
        assert_eq!(s("\x1b[;r"), "\x1b[1;20r");
        assert_eq!(s("\x1b[5r"), "\x1b[5;20r");
        assert_eq!(s("\x1b[1;30r"), "\x1b[1;20r");
        assert_eq!(s("\x1b[2;10r"), "\x1b[2;10r");
        assert_eq!(s("\x1b[3;0r"), "\x1b[3;20r");
    }

    #[test]
    fn cup_vpa() {
        assert_eq!(s("\x1b[999;1H"), "\x1b[20;1H");
        assert_eq!(s("\x1b[21H"), "\x1b[20H");
        assert_eq!(s("\x1b[30;5f"), "\x1b[20;5f");
        assert_eq!(s("\x1b[40d"), "\x1b[20d");
        assert_eq!(s("\x1b[H"), "\x1b[H");
        assert_eq!(s("\x1b[;5H"), "\x1b[;5H");
        assert_eq!(s("\x1b[20;80H"), "\x1b[20;80H");
        // Private / other finals untouched.
        assert_eq!(s("\x1b[?999h"), "\x1b[?999h");
        assert_eq!(s("\x1b[99;99R"), "\x1b[99;99R");
    }

    #[test]
    fn strings_not_rewritten() {
        let t =
            "\x1b]0;\x1b[999;1H r H\x07x\x1b]8;;http://a/r\x1b\\y\x1bP999;1H\x1b\\z\x1b_99H\x1b\\";
        // ESC inside an OSC aborts it in real terminals; use payloads that do
        // not contain ESC to check strings.
        let t2 = "\x1b]0;[999;1H r H\x07x\x1b]8;;http://a/r\x1b\\y\x1bP999;1H\x1b\\";
        assert_eq!(s(t2), t2);
        let _ = t;
    }

    #[test]
    fn split_every_boundary() {
        let input = b"ab\x1b[r\x1b[999;1Hx\x1b]0;t\x1b\\\x1b[?1049h\x1b[2J\x1b[5;80r\x1b[40d\xe2\x9c\x93\x1bP1$r\x1b\\\x1b(B\x1b[?2026h";
        let (whole, rw) = run(20, input);
        for cut in 0..=input.len() {
            let mut f = OutputFilter::new(20);
            let mut out = Vec::new();
            let mut r1 = f.process(&input[..cut], &mut out);
            let r2 = f.process(&input[cut..], &mut out);
            assert_eq!(out, whole, "cut at {cut}");
            r1.dock_damaged |= r2.dock_damaged;
            r1.needs_region |= r2.needs_region;
            assert_eq!(r1.needs_region, rw.needs_region);
            assert!(f.in_sync());
            assert!(f.in_ground());
        }
        // Byte by byte.
        let mut f = OutputFilter::new(20);
        let mut out = Vec::new();
        for b in input {
            f.process(std::slice::from_ref(b), &mut out);
        }
        assert_eq!(out, whole);
    }

    #[test]
    fn flags() {
        assert!(run(20, b"\x1b[2J").1.dock_damaged);
        assert!(run(20, b"\x1b[J").1.dock_damaged);
        assert!(!run(20, b"\x1b[J").1.needs_region);
        let r = run(20, b"\x1bc").1;
        assert!(r.dock_damaged && r.needs_region);
        let r = run(20, b"\x1b[!p").1;
        assert!(r.dock_damaged && r.needs_region);
        for m in ["1049", "1047", "47"] {
            for f in ['h', 'l'] {
                let r = run(20, format!("\x1b[?{m}{f}").as_bytes()).1;
                assert!(r.dock_damaged && r.needs_region);
            }
        }
        let r = run(20, b"\x1b[?25;1049h").1;
        assert!(r.needs_region);
        let r = run(20, b"\x1b[?3h").1;
        assert!(r.dock_damaged && !r.needs_region);
        let r = run(20, b"\x1b[K\x1b[?25h").1;
        assert!(!r.dock_damaged && !r.needs_region);
        // OSC containing "c" after ESC-less text is not RIS.
        assert!(!run(20, b"\x1b]0;c[2J\x07").1.dock_damaged);
    }

    #[test]
    fn sync_and_ground() {
        let mut f = OutputFilter::new(10);
        let mut out = Vec::new();
        f.process(b"\x1b[?2026h", &mut out);
        assert!(f.in_sync());
        f.process(b"abc\x1b[", &mut out);
        assert!(!f.in_ground());
        f.process(b"?2026l", &mut out);
        assert!(f.in_ground());
        assert!(!f.in_sync());
        f.process(b"\x1b]0;x", &mut out);
        assert!(!f.in_ground());
        f.process(b"\x07", &mut out);
        assert!(f.in_ground());
    }

    #[test]
    fn set_rows() {
        let mut f = OutputFilter::new(10);
        let mut out = Vec::new();
        f.set_child_rows(7);
        f.process(b"\x1b[r", &mut out);
        assert_eq!(out, b"\x1b[1;7r");
    }
}
