//! Inference for past resets and utilization projection (DESIGN §6.5).

use prism_core::model::{Projection, UsageSnapshot, Window};

use crate::cache::Sample;

/// Sample look-back for the 5-hour projection.
pub const LOOKBACK_5H_SECS: i64 = 3600;
/// Sample look-back for the 7-day projection.
pub const LOOKBACK_7D_SECS: i64 = 6 * 3600;
/// Minimum sample span for a projection.
pub const MIN_SPAN_SECS: i64 = 600;
/// Minimum sample count for a projection.
pub const MIN_SAMPLES: usize = 3;

fn infer_window(w: &Window, now: i64) -> Window {
    match w.resets_at {
        Some(r) if r <= now => Window {
            utilization: 0.0,
            resets_at: None,
            inferred: true,
        },
        _ => w.clone(),
    }
}

/// Windows whose `resets_at` is in the past become 0% with an unknown reset
/// and `inferred = true`.
pub fn apply_inference(s: &UsageSnapshot, now: i64) -> UsageSnapshot {
    let mut out = s.clone();
    out.five_hour = s.five_hour.as_ref().map(|w| infer_window(w, now));
    out.seven_day = s.seven_day.as_ref().map(|w| infer_window(w, now));
    for w in &mut out.scoped {
        if w.resets_at.is_some_and(|r| r <= now) {
            w.utilization = 0.0;
            w.resets_at = None;
            w.inferred = true;
        }
    }
    // A "limit reached" from before a window reset is old news.
    if out.limit_reached
        && [&out.five_hour, &out.seven_day]
            .into_iter()
            .flatten()
            .any(|w| w.inferred)
    {
        out.limit_reached = false;
    }
    out
}

/// Weighted least-squares slope (percent per second); recent samples weigh
/// more (weight grows linearly from 1 to 3 across the span).
fn weighted_slope(points: &[(i64, f64)]) -> Option<f64> {
    let (t0, t1) = (points.first()?.0, points.last()?.0);
    let span = (t1 - t0) as f64;
    let w = |t: i64| 1.0 + 2.0 * (t - t0) as f64 / span;
    let sw: f64 = points.iter().map(|p| w(p.0)).sum();
    let mt = points.iter().map(|p| w(p.0) * p.0 as f64).sum::<f64>() / sw;
    let mu = points.iter().map(|p| w(p.0) * p.1).sum::<f64>() / sw;
    let (mut num, mut den) = (0.0, 0.0);
    for &(t, u) in points {
        let dt = t as f64 - mt;
        num += w(t) * dt * (u - mu);
        den += w(t) * dt * dt;
    }
    (den > 0.0).then(|| num / den)
}

/// The utilization slope (percent per second, never negative) of the
/// samples in `[now - lookback, now]` after the last reset; `None` with too
/// few samples or too short a span.
fn recent_slope(
    samples: &[Sample],
    get: impl Fn(&Sample) -> Option<f64>,
    lookback: i64,
    now: i64,
) -> Option<(f64, i64)> {
    let mut points: Vec<(i64, f64)> = samples
        .iter()
        .filter(|s| s.ts >= now - lookback && s.ts <= now)
        .filter_map(|s| get(s).map(|u| (s.ts, u)))
        .collect();
    points.sort_by_key(|p| p.0);
    // Ignore samples before a reset (a clear drop in utilization).
    if let Some(i) = points.windows(2).rposition(|p| p[1].1 + 1.0 < p[0].1) {
        points.drain(..=i);
    }
    if points.len() < MIN_SAMPLES || points.last()?.0 - points.first()?.0 < MIN_SPAN_SECS {
        return None;
    }
    Some((weighted_slope(&points)?.max(0.0), points.last()?.0))
}

fn project_window(
    samples: &[Sample],
    get: impl Fn(&Sample) -> Option<f64>,
    w: Option<&Window>,
    lookback: i64,
    now: i64,
) -> Option<f64> {
    let w = w?;
    let reset = w.resets_at?;
    let (rate, _) = recent_slope(samples, get, lookback, now)?;
    let remaining = (reset - now).max(0) as f64;
    Some((w.utilization + rate * remaining).clamp(0.0, 100.0))
}

/// A burn rate needs a sample at most this old.
pub const RATE_FRESH_SECS: i64 = 15 * 60;

/// How fast the 5h window fills now: the slope of the last hour of
/// samples, in percent per hour. `None` with too few samples or when the
/// newest is older than [`RATE_FRESH_SECS`]. It counts all use of the
/// account, also from outside agent-prism.
pub fn rate_5h(samples: &[Sample], now: i64) -> Option<f64> {
    let (rate, newest) = recent_slope(samples, |x| x.u5h, LOOKBACK_5H_SECS, now)?;
    (now - newest <= RATE_FRESH_SECS).then_some(rate * 3600.0)
}

/// Samples further apart than this are not compared for [`week_per_5h`]:
/// in a long gap a 5h window can reset and fill again unseen.
const PAIR_MAX_GAP_SECS: i64 = 30 * 60;
/// The 5h use [`week_per_5h`] needs to see, in points: one full window.
const MIN_5H_POINTS: f64 = 100.0;

/// How many points of the weekly window one point of the 5h window costs
/// on this account (0.16: a full 5h window is 16% of the week), from the
/// rises of both between neighbouring samples (oldest first). `None` when
/// the samples show less than one full 5h window of use.
pub fn week_per_5h(samples: &[Sample]) -> Option<f64> {
    let (mut d5, mut d7) = (0.0, 0.0);
    for p in samples.windows(2) {
        if p[1].ts - p[0].ts > PAIR_MAX_GAP_SECS {
            continue;
        }
        let (Some(a5), Some(b5), Some(a7), Some(b7)) = (p[0].u5h, p[1].u5h, p[0].u7d, p[1].u7d)
        else {
            continue;
        };
        // A fall is that window's reset.
        if b5 < a5 || b7 < a7 {
            continue;
        }
        d5 += b5 - a5;
        d7 += b7 - a7;
    }
    (d5 >= MIN_5H_POINTS && d7 > 0.0).then(|| (d7 / d5).clamp(0.02, 1.0))
}

/// Projects utilization at the next reset of each window from recent
/// samples. A window gets `None` when there is too little data or no known
/// reset.
pub fn project(samples: &[Sample], s: &UsageSnapshot, now: i64) -> Projection {
    let s = apply_inference(s, now);
    Projection {
        five_hour: project_window(
            samples,
            |x| x.u5h,
            s.five_hour.as_ref(),
            LOOKBACK_5H_SECS,
            now,
        ),
        seven_day: project_window(
            samples,
            |x| x.u7d,
            s.seven_day.as_ref(),
            LOOKBACK_7D_SECS,
            now,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_core::model::ScopedWindow;

    fn win(u: f64, r: Option<i64>) -> Window {
        Window {
            utilization: u,
            resets_at: r,
            inferred: false,
        }
    }

    fn snap(u5: f64, r5: Option<i64>) -> UsageSnapshot {
        UsageSnapshot {
            fetched_at: 0,
            five_hour: Some(win(u5, r5)),
            seven_day: Some(win(50.0, Some(10_000))),
            scoped: vec![ScopedWindow {
                label: "Fable".into(),
                kind: "weekly_scoped".into(),
                utilization: 30.0,
                resets_at: Some(500),
                inferred: false,
            }],
            extra_usage_enabled: false,
            limit_reached: false,
        }
    }

    #[test]
    fn inference_past_and_future() {
        let s = apply_inference(&snap(80.0, Some(900)), 1000);
        assert_eq!(
            s.five_hour,
            Some(Window {
                utilization: 0.0,
                resets_at: None,
                inferred: true
            })
        );
        assert_eq!(s.seven_day, Some(win(50.0, Some(10_000))));
        assert!(s.scoped[0].inferred && s.scoped[0].utilization == 0.0);
        let s = apply_inference(&snap(80.0, Some(2000)), 1000);
        assert_eq!(s.five_hour, Some(win(80.0, Some(2000))));
    }

    fn samples(now: i64, rate_per_min: f64, n: i64, step: i64) -> Vec<Sample> {
        (0..n)
            .map(|i| {
                let ts = now - (n - 1 - i) * step;
                Sample {
                    ts,
                    u5h: Some(10.0 + rate_per_min * (ts - now + 3600) as f64 / 60.0),
                    u7d: None,
                }
            })
            .collect()
    }

    #[test]
    fn rising_rate_projects_higher() {
        let now = 100_000;
        let s = snap(20.0, Some(now + 3600));
        let slow = project(&samples(now, 0.05, 7, 300), &s, now)
            .five_hour
            .unwrap();
        let fast = project(&samples(now, 0.2, 7, 300), &s, now)
            .five_hour
            .unwrap();
        assert!(slow > 20.0 && fast > slow, "{slow} {fast}");
        let flat = project(&samples(now, 0.0, 7, 300), &s, now)
            .five_hour
            .unwrap();
        assert!((flat - 20.0).abs() < 1e-9);
        let huge = project(&samples(now, 5.0, 7, 300), &s, now)
            .five_hour
            .unwrap();
        assert_eq!(huge, 100.0);
        assert_eq!(project(&samples(now, 0.1, 7, 300), &s, now).seven_day, None);
    }

    #[test]
    fn too_little_data_is_none() {
        let now = 100_000;
        let s = snap(20.0, Some(now + 3600));
        // 5 minutes of span.
        assert_eq!(project(&samples(now, 0.1, 6, 60), &s, now).five_hour, None);
        // Two samples only.
        assert_eq!(project(&samples(now, 0.1, 2, 900), &s, now).five_hour, None);
        // Unknown reset.
        let s = snap(20.0, None);
        assert_eq!(project(&samples(now, 0.1, 7, 300), &s, now).five_hour, None);
    }

    #[test]
    fn burn_rate_of_the_last_hour() {
        let now = 100_000;
        let pts = |xs: &[(i64, f64)]| -> Vec<Sample> {
            xs.iter()
                .map(|&(t, u)| Sample {
                    ts: now - t,
                    u5h: Some(u),
                    u7d: None,
                })
                .collect()
        };
        // 10% in 30 min: 20%/h.
        let r = rate_5h(
            &pts(&[(1800, 10.0), (1200, 13.3), (600, 16.7), (0, 20.0)]),
            now,
        )
        .unwrap();
        assert!((r - 20.0).abs() < 0.5, "{r}");
        // Too old: the newest sample is 20 min old.
        assert_eq!(
            rate_5h(&pts(&[(3000, 10.0), (2400, 13.0), (1200, 16.0)]), now),
            None
        );
        // A reset in between: only the samples after it count (too few).
        assert_eq!(
            rate_5h(
                &pts(&[(1800, 80.0), (1200, 90.0), (600, 2.0), (0, 4.0)]),
                now
            ),
            None
        );
        // Never negative.
        assert_eq!(
            rate_5h(&pts(&[(1800, 20.0), (1200, 19.5), (0, 19.2)]), now),
            Some(0.0)
        );
    }

    #[test]
    fn weekly_cost_of_the_5h_window() {
        // Every 10 minutes the 5h window rises 5 points and the week 0.8,
        // with a 5h reset every 16 samples.
        let mut s: Vec<Sample> = (0..60)
            .map(|i| Sample {
                ts: i * 600,
                u5h: Some((i % 16) as f64 * 5.0),
                u7d: Some(i as f64 * 0.8),
            })
            .collect();
        let k = week_per_5h(&s).unwrap();
        assert!((k - 0.16).abs() < 1e-9, "{k}");
        // Less than a full 5h window of use says nothing.
        assert_eq!(week_per_5h(&s[..12]), None);
        // A long gap is not compared: the 5h window may have reset in it.
        for x in &mut s[30..] {
            x.ts += 7200;
            x.u7d = x.u7d.map(|u| u + 40.0);
        }
        let k = week_per_5h(&s).unwrap();
        assert!((k - 0.16).abs() < 1e-9, "{k}");
        // No weekly data.
        for x in &mut s {
            x.u7d = None;
        }
        assert_eq!(week_per_5h(&s), None);
    }
}
