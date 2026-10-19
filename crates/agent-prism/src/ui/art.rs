//! Animated ASCII art: a prism that splits a beam of light, for the
//! welcome screen and the About tab. It is a pure function of time, so
//! any panel can draw it from its own clock.
//!
//! Shapes are filled, not outlined: every cell gets a brightness, and a
//! character ramp turns brightness into glyph density.

use std::f64::consts::PI;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Glyph density ramp for brightness 0..1 (space first).
const GLOW: &[u8] = b" .:-=+*#%@";

/// The spectrum, top band to bottom band.
const SPECTRUM: [Color; 6] = [
    Color::Red,
    Color::Yellow,
    Color::Green,
    Color::Cyan,
    Color::Blue,
    Color::Magenta,
];

type Cell = (char, Style);
type V2 = (f64, f64);

/// One frame of a beam of white light that enters a glass prism and
/// leaves it as a fan of six colored bands, `w` × `h` cells.
///
/// Distances use square units: one column wide, half a row tall. In the
/// first two seconds the beam draws in from the left and then the fan
/// opens. After that, waves of light run along the beam and out through
/// the fan, and a glint sweeps through the glass.
pub fn prism_hero(secs: f64, w: u16, h: u16) -> Vec<Line<'static>> {
    let (w, h) = (w as usize, h as usize);
    if w < 30 || h < 6 {
        return Vec::new();
    }
    let (wf, hf) = (w as f64, h as f64 * 2.0);

    // Equilateral triangle, apex up.
    let th = hf * 0.86;
    let half = (th / 3f64.sqrt()).min(wf * 0.17);
    let th = half * 3f64.sqrt();
    let apex = (wf * 0.40, (hf - th) / 2.0);
    let bl = (apex.0 - half, apex.1 + th);
    let br = (apex.0 + half, apex.1 + th);
    let on_left = |t: f64| lerp2(apex, bl, t);
    let on_right = |t: f64| lerp2(apex, br, t);

    // The light path: source, entry on the left face, exit on the right.
    let entry = on_left(0.58);
    let exit = on_right(0.50);
    let src = (0.0, (entry.1 + entry.0 * 0.24).min(hf - 1.0));
    let beam_len = dist(src, entry);
    let inner_len = dist(entry, exit);

    // The fan leaves the right face, angled a little down. Its virtual
    // origin sits behind the face so the fan already has some width there.
    let breathe = (secs * 0.8).sin() * 0.015;
    let (mid, spread): (f64, f64) = (0.17, 0.17 + breathe);
    let origin = (exit.0 - 6.0 * mid.cos(), exit.1 - 6.0 * mid.sin());

    // The intro: beam, then fan, then the glint.
    let beam_reach = (secs / 0.8).clamp(0.0, 1.0) * beam_len;
    let fan_reach = ((secs - 0.8) / 1.0).clamp(0.0, 1.0) * wf * 1.2;
    let fade_in = (secs / 0.6).clamp(0.0, 1.0);

    let mut g: Vec<Cell> = Vec::with_capacity(w * h);
    for row in 0..h {
        for col in 0..w {
            let p = (col as f64 + 0.5, row as f64 * 2.0 + 1.0);
            let sd = tri_sd(p, apex, bl, br);
            let inside = sd < 0.0;
            // Light travels from the source; waves ride on that distance.
            let wave = |s: f64| (s * 0.28 - secs * 6.0).sin();

            // 1. Outline of the glass: brightest, white.
            let edge = smooth(1.5, 0.3, sd.abs()) * fade_in;
            if edge > 0.35 {
                g.push((glyph(0.55 + 0.45 * edge), bold()));
                continue;
            }

            // 2. The incoming beam.
            let (d, s) = seg_dist(p, src, entry);
            let core = smooth(1.6, 0.0, d);
            if core > 0.05 && s * beam_len <= beam_reach && !inside {
                let i = core * (0.78 + 0.22 * wave(s * beam_len));
                g.push((glyph(i), if i > 0.7 { bold() } else { Style::default() }));
                continue;
            }

            if inside {
                // 3. Inside the glass: light widens from entry to exit.
                let (d, s) = seg_dist(p, entry, exit);
                let lit = secs > 0.8;
                let width = 0.8 + 2.2 * s;
                let ray = smooth(width + 1.2, width * 0.3, d);
                if lit && ray > 0.05 {
                    let i = ray * (0.42 + 0.12 * wave(beam_len + s * inner_len));
                    g.push((glyph(i), Style::default()));
                    continue;
                }
                // 4. Glass: faint, lighter at the top, with a sweeping glint.
                let depth = (p.1 - apex.1) / th;
                let band = ((p.0 + p.1) * 0.022 - secs * 0.22).rem_euclid(1.0);
                let glint = (-((band - 0.5) / 0.035).powi(2)).exp() * 0.5;
                let i = (0.16 - 0.08 * depth + glint) * fade_in;
                g.push((glyph(i), if glint > 0.25 { bold() } else { dim() }));
                continue;
            }

            // 5. The spectrum fan, right of the glass.
            let v = (p.0 - origin.0, p.1 - origin.1);
            let ang = v.1.atan2(v.0);
            let f = (ang - (mid - spread)) / (2.0 * spread);
            let r = dist(p, exit);
            if p.0 > exit.0 && (0.0..1.0).contains(&f) && r <= fan_reach {
                let n = SPECTRUM.len() as f64;
                let band = ((f * n) as usize).min(SPECTRUM.len() - 1);
                // Soft seams between bands and soft outer edges.
                let seam = (PI * (f * n).fract()).sin().powf(0.6);
                let outer = smooth(0.0, 0.06, f.min(1.0 - f));
                let s = beam_len + inner_len + r;
                let i = (0.5 + 0.2 * seam + 0.22 * wave(s)) * outer;
                let mut st = Style::default().fg(SPECTRUM[band]);
                if i > 0.72 {
                    st = st.add_modifier(Modifier::BOLD);
                }
                g.push((glyph(i), st));
                continue;
            }
            g.push((' ', Style::default()));
        }
    }
    (0..h)
        .map(|r| runs(g[r * w..(r + 1) * w].iter().copied()))
        .collect()
}

/// The glyph for brightness `i` (0..1).
fn glyph(i: f64) -> char {
    let n = GLOW.len() - 1;
    GLOW[((i.clamp(0.0, 1.0) * n as f64).round() as usize).min(n)] as char
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// 0 at `e0`, 1 at `e1`, smooth between (either order).
fn smooth(e0: f64, e1: f64, x: f64) -> f64 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn lerp2(a: V2, b: V2, t: f64) -> V2 {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

fn dist(a: V2, b: V2) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Distance from `p` to the segment `a`–`b`, and where along it (0..1).
fn seg_dist(p: V2, a: V2, b: V2) -> (f64, f64) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    (dist(p, (a.0 + t * dx, a.1 + t * dy)), t)
}

/// Signed distance to a triangle: negative inside.
fn tri_sd(p: V2, a: V2, b: V2, c: V2) -> f64 {
    let d = seg_dist(p, a, b)
        .0
        .min(seg_dist(p, b, c).0)
        .min(seg_dist(p, c, a).0);
    let side = |u: V2, v: V2| (v.0 - u.0) * (p.1 - u.1) - (v.1 - u.1) * (p.0 - u.0);
    let (s1, s2, s3) = (side(a, b), side(b, c), side(c, a));
    let inside = (s1 >= 0.0 && s2 >= 0.0 && s3 >= 0.0) || (s1 <= 0.0 && s2 <= 0.0 && s3 <= 0.0);
    if inside { -d } else { d }
}

/// Join cells with the same style into spans.
fn runs(cells: impl Iterator<Item = Cell>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut style = Style::default();
    for (c, st) in cells {
        if st != style && !cur.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut cur), style));
        }
        style = st;
        cur.push(c);
    }
    if !cur.is_empty() {
        spans.push(Span::styled(cur, style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn colors(lines: &[Line]) -> Vec<Color> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().filter_map(|s| s.style.fg))
            .collect()
    }

    #[test]
    fn hero_fills_its_box() {
        for secs in [0.0, 0.7, 1.9, 3.3] {
            let f = prism_hero(secs, 72, 16);
            assert_eq!(f.len(), 16);
            assert!(f.iter().all(|l| l.width() == 72));
        }
        assert!(prism_hero(0.0, 20, 4).is_empty());
    }

    #[test]
    fn hero_opens_the_fan_after_the_beam() {
        let early = colors(&prism_hero(0.3, 72, 16));
        assert!(!early.iter().any(|c| SPECTRUM.contains(c)));
        let late = colors(&prism_hero(3.0, 72, 16));
        for c in SPECTRUM {
            assert!(late.contains(&c), "{c:?} missing");
        }
    }

    #[test]
    #[ignore]
    fn show_frames() {
        for secs in [3.0, 4.2] {
            println!("{}\n", text(&prism_hero(secs, 90, 16)));
        }
    }
}
