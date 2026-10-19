//! The balancer: limits, scores and the next-session pick (DESIGN §6.6).

use std::collections::HashMap;

use prism_core::model::{LimitKind, Limited, Policy, TokenState, UsageSnapshot};

use crate::cache::CachedUsage;
use crate::infer::apply_inference;
use crate::views::{AccountInput, ViewCtx};

/// Balancer thresholds from the config.
#[derive(Clone, Debug, PartialEq)]
pub struct BalanceSettings {
    pub policy: Policy,
    /// 5h utilization above which the smart score is penalized.
    pub soft_5h: f64,
    /// Utilization at which an account counts as limited.
    pub hard: f64,
}

/// The balancer's decision for the next new session.
#[derive(Clone, Debug, PartialEq)]
pub struct PickResult {
    /// Profile id of the chosen account.
    pub chosen: Option<String>,
    /// One-line reason for the UI.
    pub reason: String,
    /// When nothing is pickable: label and time of the earliest reset.
    pub earliest_reset: Option<(String, i64)>,
}

/// Per-account facts the balancer derives from inputs and usage.
#[derive(Clone, Debug)]
pub(crate) struct Assessed {
    pub snapshot: Option<UsageSnapshot>,
    pub u5h: f64,
    pub u7d: f64,
    pub reset_7d: Option<i64>,
    pub limited: Option<Limited>,
    /// In the pool with a usable token.
    pub eligible: bool,
}

/// Formats a duration compactly: "3d4h", "21h", "1h12m", "7m".
pub fn fmt_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (d, h) if d > 0 => format!("{d}d{h}h"),
        (_, h) if h >= 10 => format!("{h}h"),
        (_, h) if h > 0 => format!("{h}h{m}m"),
        _ if m > 0 => format!("{m}m"),
        _ => "<1m".into(),
    }
}

/// Plan multiplier from the rate-limit tier or subscription.
pub fn capacity(input: &AccountInput) -> f64 {
    input.identity.capacity()
}

/// Whether a scoped window label applies to a model id ("claude-fable-5-1" ~ "Fable").
pub fn model_matches(model: &str, label: &str) -> bool {
    !label.is_empty() && model.to_lowercase().contains(&label.to_lowercase())
}

pub(crate) fn assess(
    input: &AccountInput,
    cached: Option<&CachedUsage>,
    ctx: &ViewCtx,
) -> Assessed {
    let snapshot = cached
        .and_then(|c| c.snapshot.as_ref())
        .map(|s| apply_inference(s, ctx.now));
    let hard = ctx.settings.hard;
    let (mut u5h, mut u7d, mut reset_5h, mut reset_7d) = (0.0, 0.0, None, None);
    let mut limited = None;
    if let Some(s) = &snapshot {
        if let Some(w) = &s.five_hour {
            (u5h, reset_5h) = (w.utilization, w.resets_at);
        }
        if let Some(w) = &s.seven_day {
            (u7d, reset_7d) = (w.utilization, w.resets_at);
        }
        if u5h >= hard {
            limited = Some(Limited {
                kind: LimitKind::FiveHour,
                until: reset_5h,
            });
        } else if u7d >= (100.0 - f64::from(input.reserve_7d)).min(hard) {
            limited = Some(Limited {
                kind: LimitKind::SevenDay,
                until: reset_7d,
            });
        } else if let Some(model) = &ctx.model {
            limited = s
                .scoped
                .iter()
                .find(|w| model_matches(model, &w.label) && w.utilization >= hard)
                .map(|w| Limited {
                    kind: LimitKind::Scoped,
                    until: w.resets_at,
                });
        }
        // The service says no (Codex): blame the fuller window.
        if limited.is_none() && s.limit_reached {
            let short = s.five_hour.is_some() && (s.seven_day.is_none() || u5h >= u7d);
            limited = Some(if short {
                Limited {
                    kind: LimitKind::FiveHour,
                    until: reset_5h,
                }
            } else {
                Limited {
                    kind: LimitKind::SevenDay,
                    until: reset_7d,
                }
            });
        }
    }
    let eligible = input.in_pool && !matches!(input.token, TokenState::Missing);
    Assessed {
        snapshot,
        u5h,
        u7d,
        reset_7d,
        limited,
        eligible,
    }
}

fn slack7(input: &AccountInput, a: &Assessed) -> f64 {
    (100.0 - f64::from(input.reserve_7d) - a.u7d).max(0.0) / 100.0
}

/// The slack term `min(1, slack7 / days)`.
fn slack_term(input: &AccountInput, a: &Assessed, now: i64) -> f64 {
    let days = a
        .reset_7d
        .map(|r| ((r - now) as f64 / 86_400.0).max(0.25))
        .unwrap_or(7.0);
    (slack7(input, a) / days).min(1.0)
}

/// The smart score (higher is better).
pub(crate) fn smart_score(input: &AccountInput, a: &Assessed, ctx: &ViewCtx) -> f64 {
    let base = 0.45 * (1.0 - a.u5h / 100.0)
        + 0.35 * slack_term(input, a, ctx.now)
        + 0.20 * (1.0 - a.u7d / 100.0);
    let mut score =
        capacity(input) * input.weight * base - 0.10 * f64::from(input.sessions_elsewhere);
    if a.u5h >= ctx.settings.soft_5h {
        score -= 0.50;
    }
    if a.snapshot.is_none() {
        score -= 0.15;
    }
    score
}

/// The headroom metric: `min(100 - u5h, 100 - reserve - u7d)`.
pub(crate) fn headroom(input: &AccountInput, a: &Assessed) -> f64 {
    (100.0 - a.u5h).min(100.0 - f64::from(input.reserve_7d) - a.u7d)
}

/// The policy's score for display; `None` for order-based policies.
pub(crate) fn policy_score(input: &AccountInput, a: &Assessed, ctx: &ViewCtx) -> Option<f64> {
    match ctx.settings.policy {
        Policy::Smart => Some(smart_score(input, a, ctx)),
        Policy::Headroom => Some(headroom(input, a)),
        Policy::Priority | Policy::RoundRobin => None,
    }
}

/// Least recently used first (never used is oldest).
fn lru_key(i: &AccountInput) -> i64 {
    i.last_used.unwrap_or(i64::MIN)
}

fn argmax(cands: &[usize], inputs: &[AccountInput], f: impl Fn(usize) -> f64) -> usize {
    let mut best = cands[0];
    for &c in &cands[1..] {
        let (fc, fb) = (f(c), f(best));
        if fc > fb + 1e-9
            || ((fc - fb).abs() <= 1e-9 && lru_key(&inputs[c]) < lru_key(&inputs[best]))
        {
            best = c;
        }
    }
    best
}

/// Picks the account for the next new session.
pub fn pick(
    inputs: &[AccountInput],
    usage: &HashMap<String, CachedUsage>,
    ctx: &ViewCtx,
) -> PickResult {
    let assessed: Vec<Assessed> = inputs
        .iter()
        .map(|i| assess(i, usage.get(&i.profile_id), ctx))
        .collect();
    pick_assessed(inputs, &assessed, ctx)
}

pub(crate) fn pick_assessed(
    inputs: &[AccountInput],
    assessed: &[Assessed],
    ctx: &ViewCtx,
) -> PickResult {
    let considered: Vec<usize> = (0..inputs.len())
        .filter(|&i| assessed[i].eligible && ctx.exclude.as_deref() != Some(&inputs[i].profile_id))
        .collect();
    let cands: Vec<usize> = considered
        .iter()
        .copied()
        .filter(|&i| assessed[i].limited.is_none())
        .collect();
    if cands.is_empty() {
        let earliest = considered
            .iter()
            .filter_map(|&i| {
                let until = assessed[i].limited.as_ref()?.until?;
                Some((inputs[i].label.clone(), until))
            })
            .min_by_key(|(_, t)| *t);
        let reason = match (&earliest, considered.is_empty()) {
            (_, true) => "no eligible account".to_string(),
            (Some((label, t)), _) => {
                format!(
                    "all limited — {label} resets in {}",
                    fmt_duration(t - ctx.now)
                )
            }
            (None, _) => "all limited".to_string(),
        };
        return PickResult {
            chosen: None,
            reason,
            earliest_reset: earliest,
        };
    }
    let (best, reason) = if cands.len() == 1 {
        let why = if considered.len() > 1 {
            "the only account not limited"
        } else if ctx.exclude.is_some() {
            "the only other account"
        } else {
            "the only account in the pool"
        };
        (cands[0], why.to_string())
    } else {
        match ctx.settings.policy {
            Policy::Priority => (cands[0], "first in priority order".to_string()),
            Policy::RoundRobin => {
                let b = *cands.iter().min_by_key(|&&i| lru_key(&inputs[i])).unwrap();
                let r = if inputs[b].last_used.is_none() {
                    "not used yet"
                } else {
                    "least recently used"
                };
                (b, r.to_string())
            }
            Policy::Headroom => {
                let b = argmax(&cands, inputs, |i| headroom(&inputs[i], &assessed[i]));
                let h = headroom(&inputs[b], &assessed[b]);
                (b, format!("most headroom ({h:.0}%)"))
            }
            Policy::Smart => {
                let b = argmax(&cands, inputs, |i| {
                    smart_score(&inputs[i], &assessed[i], ctx)
                });
                (b, smart_reason(b, &cands, inputs, assessed, ctx))
            }
        }
    };
    PickResult {
        chosen: Some(inputs[best].profile_id.clone()),
        reason,
        earliest_reset: None,
    }
}

fn smart_reason(
    b: usize,
    cands: &[usize],
    inputs: &[AccountInput],
    assessed: &[Assessed],
    ctx: &ViewCtx,
) -> String {
    let a = &assessed[b];
    if a.snapshot.is_none() {
        return "usage unknown, others busier".to_string();
    }
    let others = || cands.iter().filter(move |&&i| i != b);
    let slack = slack_term(&inputs[b], a, ctx.now);
    if let Some(r) = a.reset_7d
        && slack > 0.0
        && others().all(|&i| slack_term(&inputs[i], &assessed[i], ctx.now) < slack)
    {
        return format!(
            "weekly resets in {} with {:.0}% unused",
            fmt_duration(r - ctx.now),
            slack7(&inputs[b], a) * 100.0
        );
    }
    let h5 = 100.0 - a.u5h;
    if others().all(|&i| 100.0 - assessed[i].u5h <= h5) {
        return format!("most 5h headroom ({h5:.0}%)");
    }
    "best overall score".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::tests::{ctx, input, usage};

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(72 * 60), "1h12m");
        assert_eq!(fmt_duration(21 * 3600 + 59), "21h");
        assert_eq!(fmt_duration(86_400 + 7200), "1d2h");
        assert_eq!(fmt_duration(30), "<1m");
    }

    #[test]
    fn caps() {
        let mut i = input("a");
        i.identity.rate_limit_tier = Some("default_claude_max_20x".into());
        assert_eq!(capacity(&i), 2.0);
        i.identity.rate_limit_tier = Some("default_claude_max_5x".into());
        assert_eq!(capacity(&i), 1.0);
        i.identity.rate_limit_tier = None;
        i.identity.subscription = Some("pro".into());
        assert_eq!(capacity(&i), 0.4);
        i.identity.subscription = Some("team".into());
        assert_eq!(capacity(&i), 1.0);
    }

    #[test]
    fn codex_caps_and_limit_reached() {
        let mut i = input("a");
        i.identity.tool = prism_core::model::Tool::Codex;
        i.identity.subscription = Some("pro".into());
        assert_eq!(capacity(&i), 2.0);
        i.identity.subscription = Some("plus".into());
        assert_eq!(capacity(&i), 0.4);
        let c = ctx(Policy::Smart);
        let now = c.now;
        let (id, mut u) = usage("a", 0.0, 30.0, Some(now + 3600), now);
        let s = u.snapshot.as_mut().unwrap();
        s.five_hour = None;
        s.limit_reached = true;
        let a = assess(&i, Some(&u), &c);
        let l = a.limited.expect("limited");
        assert_eq!((l.kind, l.until), (LimitKind::SevenDay, Some(now + 3600)));
        // After the reset the old flag does not count.
        u.snapshot
            .as_mut()
            .unwrap()
            .seven_day
            .as_mut()
            .unwrap()
            .resets_at = Some(now - 1);
        assert!(assess(&i, Some(&u), &c).limited.is_none(), "{id}");
    }

    #[test]
    fn smart_prefers_reset_soon_slack() {
        let c = ctx(Policy::Smart);
        let now = c.now;
        // A: 7d resets in 21h with 30% unused; B: same 5h, 7d at 40% resetting in 6 days.
        let inputs = vec![input("b"), input("a")];
        let u = HashMap::from([
            usage("a", 10.0, 70.0, Some(now + 21 * 3600), now),
            usage("b", 10.0, 40.0, Some(now + 6 * 86_400), now),
        ]);
        let p = pick(&inputs, &u, &c);
        assert_eq!(p.chosen.as_deref(), Some("a"));
        assert_eq!(p.reason, "weekly resets in 21h with 30% unused");
    }

    #[test]
    fn smart_soft_penalty_and_hard_limit() {
        let mut c = ctx(Policy::Smart);
        let now = c.now;
        let inputs = vec![input("a"), input("b")];
        // a has more weekly slack but is above soft_5h.
        let u = HashMap::from([
            usage("a", 85.0, 0.0, Some(now + 86_400), now),
            usage("b", 30.0, 60.0, Some(now + 6 * 86_400), now),
        ]);
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("b"));
        // At hard, a is limited; b is the only one.
        let u = HashMap::from([
            usage("a", 30.0, 0.0, Some(now + 86_400), now),
            usage("b", 96.0, 0.0, Some(now + 86_400), now),
        ]);
        let p = pick(&inputs, &u, &c);
        assert_eq!(
            (p.chosen.as_deref(), p.reason.as_str()),
            (Some("a"), "the only account not limited")
        );
        // exclude
        c.exclude = Some("a".into());
        assert_eq!(pick(&inputs, &u, &c).chosen, None);
    }

    #[test]
    fn reserve_7d_limits() {
        let c = ctx(Policy::Smart);
        let now = c.now;
        let mut a = input("a");
        a.reserve_7d = 20;
        let u = HashMap::from([
            usage("a", 0.0, 80.0, Some(now + 86_400), now),
            usage("b", 50.0, 50.0, Some(now + 86_400), now),
        ]);
        let p = pick(&[a, input("b")], &u, &c);
        assert_eq!(p.chosen.as_deref(), Some("b"));
    }

    #[test]
    fn all_limited_reports_earliest() {
        let c = ctx(Policy::Smart);
        let now = c.now;
        let (mut a, mut b) = (input("a"), input("b"));
        a.label = "PA Ann".into();
        b.label = "Acme Bea".into();
        let mut ua = usage("a", 100.0, 0.0, None, now);
        ua.1.snapshot
            .as_mut()
            .unwrap()
            .five_hour
            .as_mut()
            .unwrap()
            .resets_at = Some(now + 3 * 3600);
        let mut ub = usage("b", 100.0, 0.0, None, now);
        ub.1.snapshot
            .as_mut()
            .unwrap()
            .five_hour
            .as_mut()
            .unwrap()
            .resets_at = Some(now + 72 * 60);
        let p = pick(&[a, b], &HashMap::from([ua, ub]), &c);
        assert_eq!(p.chosen, None);
        assert_eq!(p.reason, "all limited — Acme Bea resets in 1h12m");
        assert_eq!(p.earliest_reset, Some(("Acme Bea".into(), now + 72 * 60)));
    }

    #[test]
    fn order_policies() {
        let now = ctx(Policy::Smart).now;
        let (mut a, mut b, mut cc) = (input("a"), input("b"), input("c"));
        a.last_used = Some(now - 10);
        b.last_used = Some(now - 1000);
        cc.last_used = Some(now - 100);
        let inputs = vec![a, b, cc];
        let u = HashMap::from([
            usage("a", 50.0, 10.0, None, now),
            usage("b", 10.0, 80.0, None, now),
            usage("c", 20.0, 20.0, None, now),
        ]);
        assert_eq!(
            pick(&inputs, &u, &ctx(Policy::Priority)).chosen.as_deref(),
            Some("a")
        );
        assert_eq!(
            pick(&inputs, &u, &ctx(Policy::RoundRobin))
                .chosen
                .as_deref(),
            Some("b")
        );
        let p = pick(&inputs, &u, &ctx(Policy::Headroom));
        assert_eq!(
            (p.chosen.as_deref(), p.reason.as_str()),
            (Some("c"), "most headroom (80%)")
        );
    }

    #[test]
    fn scoped_window_only_for_matching_model() {
        let mut c = ctx(Policy::Priority);
        let now = c.now;
        let mut ua = usage("a", 10.0, 10.0, None, now);
        ua.1.snapshot
            .as_mut()
            .unwrap()
            .scoped
            .push(prism_core::model::ScopedWindow {
                label: "Fable".into(),
                kind: "weekly_scoped".into(),
                utilization: 100.0,
                resets_at: Some(now + 3600),
                inferred: false,
            });
        let u = HashMap::from([ua, usage("b", 50.0, 50.0, None, now)]);
        let inputs = vec![input("a"), input("b")];
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("a"));
        c.model = Some("claude-opus-5".into());
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("a"));
        c.model = Some("claude-fable-5-1".into());
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("b"));
    }

    #[test]
    fn token_states() {
        let c = ctx(Policy::Priority);
        let (mut a, mut b) = (input("a"), input("b"));
        a.token = TokenState::Missing;
        b.token = TokenState::Expired { since: 0 };
        let p = pick(&[a, b], &HashMap::new(), &c);
        assert_eq!(p.chosen.as_deref(), Some("b"));
    }
}
