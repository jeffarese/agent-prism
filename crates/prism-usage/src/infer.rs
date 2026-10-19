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

fn project_window(
    samples: &[Sample],
    get: impl Fn(&Sample) -> Option<f64>,
    w: Option<&Window>,
    lookback: i64,
    now: i64,
) -> Option<f64> {
    let w = w?;
    let reset = w.resets_at?;
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
    let rate = weighted_slope(&points)?.max(0.0);
    let remaining = (reset - now).max(0) as f64;
    Some((w.utilization + rate * remaining).clamp(0.0, 100.0))
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
}
