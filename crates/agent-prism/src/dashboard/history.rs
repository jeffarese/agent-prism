//! Utilization history from the usage samples, bucketed for the charts.

use prism_usage::cache::Sample;
use serde::Serialize;

/// One bucket: the time of its newest sample and the highest 5h and 7d
/// utilization sampled in it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Point(pub i64, pub Option<f64>, pub Option<f64>);

/// The bucket size for a range: about 300 points.
pub fn bucket_secs(range_secs: i64) -> i64 {
    match range_secs {
        r if r <= 86_400 => 300,
        r if r <= 3 * 86_400 => 900,
        _ => 1800,
    }
}

/// The samples in `[since, now]` folded into buckets of `bucket` seconds,
/// keeping the peak of each window (a limit hit never hides between two
/// samples). Buckets without samples are left out; the chart breaks its
/// line there.
pub fn bucket(samples: &[Sample], since: i64, now: i64, bucket: i64) -> Vec<Point> {
    let max = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, y) => x.or(y),
    };
    let mut out: Vec<Point> = Vec::new();
    for s in samples.iter().filter(|s| s.ts >= since && s.ts <= now) {
        let b = s.ts - s.ts.rem_euclid(bucket);
        match out.last_mut() {
            Some(p) if p.0 - p.0.rem_euclid(bucket) == b => {
                p.0 = s.ts;
                p.1 = max(p.1, s.u5h);
                p.2 = max(p.2, s.u7d);
            }
            _ => out.push(Point(s.ts, s.u5h, s.u7d)),
        }
    }
    out
}
