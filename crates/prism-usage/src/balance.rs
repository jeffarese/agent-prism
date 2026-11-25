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
    /// The profile id of that account.
    pub earliest_id: Option<String>,
}

/// Points of the weekly window one point of the 5h window costs, for an
/// account whose samples don't say yet (`AccountInput::week_per_5h`): a
/// full 5h window is a fifth of the week. Max 5x and team accounts measure
/// 0.15 to 0.17.
pub const WEEK_PER_5H: f64 = 0.2;

/// Usage older than this says too little about an account that
/// agent-prism has run on since: it counts as unknown.
const UNKNOWN_AFTER_SECS: i64 = 3600;

/// The smart score's weight of the week: how fast the account must be used
/// for none of its week to expire. A week about to expire is worth about
/// half a 5h window of room. More weight puts the runs of a day on one
/// account: simulated (5 accounts, 8 weeks), 0.65 served 2% more work when
/// the demand exceeded the pool, but at a normal demand cost a 5h limit
/// switch a week that 0.40 does not.
const WEEK_WEIGHT: f64 = 0.40;
/// The smart score's weight of the 5h room left with this run.
const ROOM_WEIGHT: f64 = 0.60;

/// A weekly window that binds the run: the 7-day window, or a scoped
/// window of the run's model.
#[derive(Clone, Debug)]
pub(crate) struct Week {
    /// The scoped window's label ("Fable"); `None` for the 7-day window.
    pub label: Option<String>,
    /// Percent of the window the balancer may still use: what is unused,
    /// less the reserve (7-day window only).
    pub unused: f64,
    pub resets_at: Option<i64>,
    /// The use expected within the hour (or by the window's reset) with
    /// this run on the account. Not clamped.
    pub with_run: f64,
    /// The use at which the account counts as limited.
    pub limit: f64,
}

impl Week {
    /// How fast the window must be used for none of it to expire:
    /// `min(1, unused / days to its reset)`, 1 being all of it in a day.
    pub fn urgency(&self, now: i64) -> f64 {
        let days = self
            .resets_at
            .map(|r| ((r - now) as f64 / 86_400.0).max(0.25))
            .unwrap_or(7.0);
        (self.unused / 100.0 / days).min(1.0)
    }
}

/// The window a run would bring near its limit within the hour.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Tight<'a> {
    FiveHour,
    Week(&'a Week),
}

/// Per-account facts the balancer derives from inputs and usage.
#[derive(Clone, Debug)]
pub(crate) struct Assessed {
    pub snapshot: Option<UsageSnapshot>,
    /// The snapshot says what the account has used: there is one, and it
    /// is not an old one of an account that runs have used since.
    pub known: bool,
    pub u7d: f64,
    pub reset_5h: Option<i64>,
    pub limited: Option<Limited>,
    /// In the pool with a usable token.
    pub eligible: bool,
    /// The 5h use expected within the hour (or by the window's reset) with
    /// the runs on the account now: measured use plus the recent burn rate
    /// and what the runs too new to show in it are expected to use. Not
    /// clamped.
    pub load_5h: f64,
    /// What one more run adds to `load_5h`.
    pub add_5h: f64,
    /// The weekly windows that bind the run: the 7-day one, then the
    /// scoped ones of its model. Never empty.
    pub weeks: Vec<Week>,
}

impl Assessed {
    /// The expected 5h use with one more run on the account.
    pub fn with_run(&self) -> f64 {
        self.load_5h + self.add_5h
    }

    /// The weekly window that leaves the run the least to use up before
    /// it expires, and its urgency: the account's.
    pub fn week(&self, now: i64) -> (&Week, f64) {
        self.weeks
            .iter()
            .map(|w| (w, w.urgency(now)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .expect("the 7-day window is always there")
    }

    /// How fast the account must be used for none of its week to expire:
    /// its binding week's urgency, and none while its usage is unknown
    /// (nothing says that its week would go unused).
    pub fn urgency(&self, now: i64) -> f64 {
        if self.known { self.week(now).1 } else { 0.0 }
    }

    /// The window one more run would bring near its limit within the hour:
    /// the 5h window at `soft_5h`, a weekly one at the use that limits the
    /// account.
    pub fn tight(&self, soft_5h: f64) -> Option<Tight<'_>> {
        if self.with_run() >= soft_5h {
            return Some(Tight::FiveHour);
        }
        self.weeks
            .iter()
            .find(|w| w.with_run >= w.limit)
            .map(Tight::Week)
    }

    /// One more run fits: within the hour it brings no window near its
    /// limit.
    pub fn fits(&self, soft_5h: f64) -> bool {
        self.tight(soft_5h).is_none()
    }
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

/// The hours of new work a window takes before it resets (at most one: the
/// balancer looks one hour ahead).
fn horizon_hours(resets_at: Option<i64>, now: i64) -> f64 {
    match resets_at {
        Some(r) if r > now => ((r - now) as f64 / 3600.0).min(1.0),
        _ => 1.0,
    }
}

/// Percent of this account's 5h window one run uses per hour: the learned
/// rate, else `run_load` scaled by the plan (a 20x plan fills half as
/// fast). 0 when the load is off.
pub fn run_rate(input: &AccountInput, ctx: &ViewCtx) -> f64 {
    if ctx.run_load <= 0.0 {
        return 0.0;
    }
    input
        .run_rate
        .unwrap_or_else(|| ctx.run_load / capacity(input).max(0.1))
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
    let reserve = f64::from(input.reserve_7d);
    let limit_7d = (100.0 - reserve).min(hard);
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
        } else if u7d >= limit_7d {
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
    let known = snapshot.as_ref().is_some_and(|s| {
        let used_since = input.sessions_elsewhere > 0
            || input.active_here
            || input.last_used.is_some_and(|t| t > s.fetched_at);
        !(used_since && ctx.now - s.fetched_at > UNKNOWN_AFTER_SECS)
    });
    // The 5h points per hour the runs on the account are expected to use,
    // and what this run adds. The measured rate shows the runs that have
    // been there for a while as they really are (an open session is idle
    // most of the time); `fresh` adds the ones too new to show in it.
    let (burn, add) = if ctx.run_load > 0.0 {
        let per_run = run_rate(input, ctx);
        let burn = match input.slope_5h {
            Some(s) => s + input.fresh * per_run,
            None => input.load * per_run,
        };
        (burn, per_run * ctx.new_run.max(0.0))
    } else {
        (0.0, 0.0)
    };
    let h = horizon_hours(reset_5h, ctx.now);
    let per_5h = input.week_per_5h.unwrap_or(WEEK_PER_5H);
    let week = |label, used: f64, unused: f64, resets_at, limit| Week {
        label,
        unused: unused.max(0.0),
        resets_at,
        with_run: used + (burn + add) * per_5h * horizon_hours(resets_at, ctx.now),
        limit,
    };
    let mut weeks = vec![week(None, u7d, 100.0 - reserve - u7d, reset_7d, limit_7d)];
    if let (Some(s), Some(model)) = (&snapshot, &ctx.model) {
        // All the expected use counts on the model's window, also of runs
        // that may use another model: the cautious side.
        weeks.extend(
            s.scoped
                .iter()
                .filter(|w| model_matches(model, &w.label))
                .map(|w| {
                    week(
                        Some(w.label.clone()),
                        w.utilization,
                        100.0 - w.utilization,
                        w.resets_at,
                        hard,
                    )
                }),
        );
    }
    Assessed {
        snapshot,
        known,
        u7d,
        reset_5h,
        limited,
        eligible,
        load_5h: u5h + burn * h,
        add_5h: add * h,
        weeks,
    }
}

/// The smart score (higher is better): mostly how fast the account's week
/// must be used for none of it to expire, then the 5h room left with this
/// run. An account the run does not fit on scores half a point less.
pub(crate) fn smart_score(input: &AccountInput, a: &Assessed, ctx: &ViewCtx) -> f64 {
    let base = WEEK_WEIGHT * a.urgency(ctx.now) + ROOM_WEIGHT * (1.0 - a.with_run() / 100.0);
    let mut score = capacity(input) * input.weight * base;
    if !a.fits(ctx.settings.soft_5h) {
        score -= 0.50;
    }
    if !a.known {
        score -= 0.15;
    }
    score
}

/// The headroom metric: `min(100 - u5h, 100 - reserve - u7d)`, with the
/// expected 5h use of this run and the live ones.
pub(crate) fn headroom(input: &AccountInput, a: &Assessed) -> f64 {
    (100.0 - a.with_run()).min(100.0 - f64::from(input.reserve_7d) - a.u7d)
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
                Some((i, until))
            })
            .min_by_key(|(_, t)| *t);
        let reason = match (&earliest, considered.is_empty()) {
            (_, true) => "no eligible account".to_string(),
            (Some((i, t)), _) => {
                format!(
                    "all limited — {} resets in {}",
                    inputs[*i].label,
                    fmt_duration(t - ctx.now)
                )
            }
            (None, _) => "all limited".to_string(),
        };
        return PickResult {
            chosen: None,
            reason,
            earliest_reset: earliest.map(|(i, t)| (inputs[i].label.clone(), t)),
            earliest_id: earliest.map(|(i, _)| inputs[i].profile_id.clone()),
        };
    }
    let fits = |i: usize| assessed[i].fits(ctx.settings.soft_5h);
    // A resume goes back to the account that last ran the session while it
    // has room: its prompt cache is there.
    if let Some(p) = &ctx.prefer
        && let Some(&i) = cands.iter().find(|&&i| &inputs[i].profile_id == p)
        && fits(i)
    {
        return chosen(inputs, i, "resumes on the account that last ran it".into());
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
            Policy::Priority => match cands.iter().copied().find(|&i| fits(i)) {
                Some(i) if i == cands[0] => (i, "first in priority order".to_string()),
                Some(i) => (i, "first in priority order with room".to_string()),
                None => (cands[0], "first in priority order".to_string()),
            },
            Policy::RoundRobin => {
                let b = *cands
                    .iter()
                    .min_by(|&&a, &&b| {
                        inputs[a]
                            .load
                            .total_cmp(&inputs[b].load)
                            .then(lru_key(&inputs[a]).cmp(&lru_key(&inputs[b])))
                    })
                    .unwrap();
                let fewer = cands.iter().any(|&i| inputs[i].load > inputs[b].load);
                let r = if fewer {
                    "fewest runs now"
                } else if inputs[b].last_used.is_none() {
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
                // Where the run fits, whatever the scores (plan and weight
                // scale them); nowhere: the best score of all.
                let fitting: Vec<usize> = cands.iter().copied().filter(|&i| fits(i)).collect();
                let pool = if fitting.is_empty() { &cands } else { &fitting };
                let b = argmax(pool, inputs, |i| smart_score(&inputs[i], &assessed[i], ctx));
                let why = if pool.len() == 1 {
                    "the only account with room for this run".to_string()
                } else {
                    smart_reason(b, pool, inputs, assessed, ctx)
                };
                (b, why)
            }
        }
    };
    chosen(inputs, best, reason)
}

fn chosen(inputs: &[AccountInput], i: usize, reason: String) -> PickResult {
    PickResult {
        chosen: Some(inputs[i].profile_id.clone()),
        reason,
        earliest_reset: None,
        earliest_id: None,
    }
}

/// Where `n` new runs would go now, one pick after the other, each
/// counting the ones before it (nothing is claimed). Stops at the first
/// run with no account.
pub fn plan(
    inputs: &[AccountInput],
    usage: &HashMap<String, CachedUsage>,
    ctx: &ViewCtx,
    n: usize,
) -> Vec<PickResult> {
    let mut inputs = inputs.to_vec();
    let mut out = Vec::new();
    for _ in 0..n {
        let p = pick(&inputs, usage, ctx);
        let Some(id) = p.chosen.clone() else {
            out.push(p);
            break;
        };
        if let Some(i) = inputs.iter_mut().find(|i| i.profile_id == id) {
            i.load += ctx.new_run.max(0.0);
            i.fresh += ctx.new_run.max(0.0);
            i.sessions_elsewhere += 1;
            i.last_used = Some(ctx.now);
        }
        out.push(p);
    }
    out
}

/// The most runs [`capacity`] counts.
pub const MAX_CAPACITY: u32 = 50;

/// How many more runs the pool takes now before no usable account has
/// room for one (its expected 5h use would reach `soft_5h`, or a weekly
/// window its limit): each run goes to the account with the most 5h room.
/// `None` when the load is off (`run_load = 0`).
pub fn capacity_left(
    inputs: &[AccountInput],
    usage: &HashMap<String, CachedUsage>,
    ctx: &ViewCtx,
) -> Option<u32> {
    if ctx.run_load <= 0.0 {
        return None;
    }
    let mut inputs = inputs.to_vec();
    let mut n = 0;
    while n < MAX_CAPACITY {
        let best = inputs
            .iter()
            .enumerate()
            .map(|(i, x)| (i, assess(x, usage.get(&x.profile_id), ctx)))
            .filter(|(_, a)| a.eligible && a.limited.is_none() && a.fits(ctx.settings.soft_5h))
            .min_by(|(_, a), (_, b)| a.with_run().total_cmp(&b.with_run()));
        let Some((i, _)) = best else { break };
        inputs[i].load += 1.0;
        inputs[i].fresh += 1.0;
        n += 1;
    }
    Some(n)
}

/// What `agp status --json` adds per account about the runs on it.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadInfo {
    pub profile_id: String,
    /// Live agent-prism runs on the account.
    pub runs: u32,
    /// The expected 5h use within the hour with those runs (0–100);
    /// `None` without usage data.
    pub load_5h: Option<f64>,
    /// In the pool, logged in (an expired token counts: the next run
    /// refreshes it) and not limited.
    pub usable: bool,
}

/// [`LoadInfo`] for every input, in input order.
pub fn load_info(
    inputs: &[AccountInput],
    usage: &HashMap<String, CachedUsage>,
    ctx: &ViewCtx,
) -> Vec<LoadInfo> {
    inputs
        .iter()
        .map(|i| {
            let a = assess(i, usage.get(&i.profile_id), ctx);
            LoadInfo {
                profile_id: i.profile_id.clone(),
                runs: i.sessions_elsewhere + u32::from(i.active_here),
                load_5h: a.snapshot.as_ref().map(|_| a.load_5h.clamp(0.0, 100.0)),
                usable: a.eligible && a.limited.is_none(),
            }
        })
        .collect()
}

/// Why smart picked `b` from `pool` (two accounts or more): the term of
/// the score that put it ahead of the best of the others.
fn smart_reason(
    b: usize,
    pool: &[usize],
    inputs: &[AccountInput],
    assessed: &[Assessed],
    ctx: &ViewCtx,
) -> String {
    let a = &assessed[b];
    if !a.known {
        return "usage unknown, others busier".to_string();
    }
    let others: Vec<usize> = pool.iter().copied().filter(|&i| i != b).collect();
    let next = argmax(&others, inputs, |i| {
        smart_score(&inputs[i], &assessed[i], ctx)
    });
    // Both terms as the score counts them, plan and weight included.
    let terms = |i: usize| {
        let scale = capacity(&inputs[i]) * inputs[i].weight;
        (
            scale * WEEK_WEIGHT * assessed[i].urgency(ctx.now),
            scale * ROOM_WEIGHT * (1.0 - assessed[i].with_run() / 100.0),
        )
    };
    let ((week_b, room_b), (week_n, room_n)) = (terms(b), terms(next));
    let by_week = week_b - week_n;
    if by_week > 1e-9 && by_week > room_b - room_n {
        let (w, _) = a.week(ctx.now);
        let unused = w.unused;
        return match (&w.label, w.resets_at) {
            (None, Some(r)) => format!(
                "weekly resets in {} with {unused:.0}% unused",
                fmt_duration(r - ctx.now)
            ),
            (Some(l), Some(r)) => format!(
                "{l} week resets in {} with {unused:.0}% unused",
                fmt_duration(r - ctx.now)
            ),
            (None, None) => format!("most weekly room ({unused:.0}% unused)"),
            (Some(l), None) => format!("most {l} room ({unused:.0}% unused)"),
        };
    }
    let room = 100.0 - a.with_run();
    if others
        .iter()
        .all(|&i| 100.0 - assessed[i].with_run() <= room)
    {
        return if a.add_5h > 0.0 {
            format!("most 5h room ({room:.0}% left with this run)")
        } else {
            format!("most 5h headroom ({room:.0}%)")
        };
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

    /// a, b, c at 10%, 30%, 40% of the 5h window, 20% weekly each.
    fn lanes() -> (Vec<AccountInput>, HashMap<String, CachedUsage>, ViewCtx) {
        let mut c = ctx(Policy::Smart);
        c.run_load = 25.0;
        let now = c.now;
        let u = HashMap::from([
            usage("a", 10.0, 20.0, Some(now + 3 * 86_400), now),
            usage("b", 30.0, 20.0, Some(now + 3 * 86_400), now),
            usage("c", 40.0, 20.0, Some(now + 3 * 86_400), now),
        ]);
        (vec![input("a"), input("b"), input("c")], u, c)
    }

    fn ids(p: &[PickResult]) -> Vec<&str> {
        p.iter()
            .map(|p| p.chosen.as_deref().unwrap_or("-"))
            .collect()
    }

    #[test]
    fn heavy_lanes_spread_wider() {
        let (inputs, u, mut c) = lanes();
        // Lanes learned to burn 2x a typical run: three go to three accounts
        // (with weight 1 they go a, b, a).
        c.new_run = 2.0;
        let p = plan(&inputs, &u, &c, 3);
        assert_eq!(ids(&p), ["a", "b", "c"]);
        // b and c would end at soft_5h or over it (80, 90).
        assert_eq!(p[0].reason, "the only account with room for this run");
    }

    #[test]
    fn lanes_started_together_spread() {
        let (inputs, u, c) = lanes();
        // Without load every run goes to the emptiest account.
        let mut off = c.clone();
        off.run_load = 0.0;
        assert_eq!(ids(&plan(&inputs, &u, &off, 3)), ["a", "a", "a"]);
        // With it, each run counts: a has the most room, so it takes two.
        let p = plan(&inputs, &u, &c, 3);
        assert_eq!(ids(&p), ["a", "b", "a"]);
        assert_eq!(p[0].reason, "most 5h room (65% left with this run)");
        // A live run on a (another process) moves the next one to b.
        let mut busy = inputs.clone();
        busy[0].load = 1.0;
        busy[0].sessions_elsewhere = 1;
        assert_eq!(pick(&busy, &u, &c).chosen.as_deref(), Some("b"));
        // An idle interactive session weighs a quarter: a still wins.
        busy[0].load = 0.25;
        assert_eq!(pick(&busy, &u, &c).chosen.as_deref(), Some("a"));
    }

    #[test]
    fn burn_rate_counts_use_agp_does_not_see() {
        let (mut inputs, u, c) = lanes();
        // a fills at 60%/h (another machine, claude.ai): 10 + 60 + 25 > b's 55.
        inputs[0].slope_5h = Some(60.0);
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("b"));
        // Two runs that just came to a don't show in its rate yet: they
        // count on top of it (10 + 1 + 50 + 25).
        inputs[0].slope_5h = Some(1.0);
        inputs[0].load = 2.0;
        inputs[0].fresh = 2.0;
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("b"));
        // After an hour there the rate shows what they really use: two
        // sessions that wait for their user are not two working runs.
        inputs[0].fresh = 0.0;
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("a"));
        // Without a rate (no recent samples) the runs count in full.
        inputs[0].slope_5h = None;
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("b"));
        // A learned rate replaces run_load for that account.
        let (mut inputs, u, c) = lanes();
        inputs[0].run_rate = Some(80.0);
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("b"));
    }

    #[test]
    fn soon_reset_takes_less_load() {
        let (mut inputs, mut u, c) = lanes();
        let now = c.now;
        // b's window resets in 6 minutes: a run there adds 2.5 points.
        u.get_mut("b").unwrap().snapshot.as_mut().unwrap().five_hour =
            Some(prism_core::model::Window {
                utilization: 30.0,
                resets_at: Some(now + 360),
                inferred: false,
            });
        inputs[0].load = 1.0;
        let p = pick(&inputs, &u, &c);
        assert_eq!(p.chosen.as_deref(), Some("b"));
    }

    #[test]
    fn capacity_counts_runs_until_soft() {
        let (inputs, u, c) = lanes();
        // soft_5h is 80 here: a takes two (35, 60), b one (55), c one (65).
        assert_eq!(capacity_left(&inputs, &u, &c), Some(4));
        let mut full = inputs.clone();
        full[0].load = 3.0;
        assert_eq!(capacity_left(&full, &u, &c), Some(2));
        let mut off = c.clone();
        off.run_load = 0.0;
        assert_eq!(capacity_left(&inputs, &u, &off), None);
        let info = load_info(&full, &u, &c);
        assert_eq!(info[0].load_5h, Some(85.0));
        assert!(info.iter().all(|i| i.usable));
    }

    #[test]
    fn resume_prefers_the_last_account_while_it_has_room() {
        let (inputs, mut u, mut c) = lanes();
        c.prefer = Some("c".into());
        let p = pick(&inputs, &u, &c);
        assert_eq!(
            (p.chosen.as_deref(), p.reason.as_str()),
            (Some("c"), "resumes on the account that last ran it")
        );
        // c would end over soft_5h: pick as usual.
        u.get_mut("c").unwrap().snapshot.as_mut().unwrap().five_hour =
            Some(prism_core::model::Window {
                utilization: 70.0,
                resets_at: None,
                inferred: false,
            });
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("a"));
    }

    #[test]
    fn order_policies_count_runs() {
        let (mut inputs, u, mut c) = lanes();
        c.settings.policy = Policy::RoundRobin;
        inputs[0].load = 1.0;
        inputs[1].last_used = Some(c.now - 10);
        let p = pick(&inputs, &u, &c);
        assert_eq!(
            (p.chosen.as_deref(), p.reason.as_str()),
            (Some("c"), "fewest runs now")
        );
        // Priority fills the first account, then spills when it has no room.
        c.settings.policy = Policy::Priority;
        inputs[0].load = 2.0;
        let p = pick(&inputs, &u, &c);
        assert_eq!(
            (p.chosen.as_deref(), p.reason.as_str()),
            (Some("b"), "first in priority order with room")
        );
        inputs[0].load = 0.0;
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("a"));
    }

    /// The pool that started this (2026-11-25): pb2's week resets in 3h52m
    /// with 26% unused, pb5's in three days with an empty 5h window.
    fn expiring() -> (Vec<AccountInput>, HashMap<String, CachedUsage>, ViewCtx) {
        let mut c = ctx(Policy::Smart);
        c.run_load = 25.0;
        let now = c.now;
        let mut u = HashMap::from([
            usage("pb2", 22.0, 74.0, Some(now + 13_920), now),
            usage("pb5", 0.0, 41.0, Some(now + 3 * 86_400), now),
        ]);
        u.get_mut("pb2")
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .five_hour = Some(prism_core::model::Window {
            utilization: 22.0,
            resets_at: Some(now + 6960),
            inferred: false,
        });
        (vec![input("pb2"), input("pb5")], u, c)
    }

    /// The view's reason per profile id (the views come in display order).
    fn reasons(
        inputs: &[AccountInput],
        u: &HashMap<String, CachedUsage>,
        c: &ViewCtx,
    ) -> HashMap<String, Option<String>> {
        crate::views::build_views(inputs, u, &HashMap::new(), c)
            .into_iter()
            .map(|v| (v.profile_id, v.reason))
            .collect()
    }

    #[test]
    fn smart_uses_up_the_week_that_expires() {
        let (mut inputs, u, mut c) = expiring();
        // Three sessions open on pb2 for hours use 5%/h between them: the
        // run fits (22 + 5 + 25), and pb2's week is the one to use up.
        inputs[0].load = 2.25;
        inputs[0].sessions_elsewhere = 3;
        inputs[0].slope_5h = Some(5.0);
        let p = pick(&inputs, &u, &c);
        assert_eq!(
            (p.chosen.as_deref(), p.reason.as_str()),
            (Some("pb2"), "weekly resets in 3h52m with 26% unused")
        );
        assert_eq!(reasons(&inputs, &u, &c)["pb5"], None);
        // The same three, all started just now: within the hour pb2's 5h
        // window would not hold one more.
        inputs[0].fresh = 2.25;
        let p = pick(&inputs, &u, &c);
        assert_eq!(
            (p.chosen.as_deref(), p.reason.as_str()),
            (Some("pb5"), "the only account with room for this run")
        );
        assert_eq!(
            reasons(&inputs, &u, &c)["pb2"].as_deref(),
            Some("5h would fill within the hour, resets in 1h56m")
        );
        // The account a session moves from gets no verdict.
        c.exclude = Some("pb2".into());
        assert_eq!(reasons(&inputs, &u, &c)["pb2"], None);
    }

    #[test]
    fn old_usage_of_an_account_in_use_is_unknown() {
        let (mut inputs, mut u, c) = expiring();
        let now = c.now;
        // pb2's numbers are two days old (its login can't be read).
        u.get_mut("pb2")
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .fetched_at = now - 2 * 86_400;
        // Nothing ran there since: they still stand.
        inputs[0].last_used = Some(now - 3 * 86_400);
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("pb2"));
        // A session started there yesterday: what is left of its week is
        // anyone's guess, so it does not come first for it.
        inputs[0].last_used = Some(now - 86_400);
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("pb5"));
        // The same with a run on it now.
        inputs[0].last_used = None;
        inputs[0].sessions_elsewhere = 1;
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("pb5"));
        // It still takes a run when the others are busier, and says so:
        // pb5 at 54% of its 5h window and 88% of its week, pb2 idle.
        let window = |used| {
            Some(prism_core::model::Window {
                utilization: used,
                resets_at: None,
                inferred: false,
            })
        };
        u.get_mut("pb2")
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .five_hour = window(0.0);
        let s = u.get_mut("pb5").unwrap().snapshot.as_mut().unwrap();
        s.five_hour = window(54.0);
        s.seven_day.as_mut().unwrap().utilization = 88.0;
        let p = pick(&inputs, &u, &c);
        assert_eq!(
            (p.chosen.as_deref(), p.reason.as_str()),
            (Some("pb2"), "usage unknown, others busier")
        );
        // Usage a few minutes old is what polling gives: known.
        u.get_mut("pb2")
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .fetched_at = now - 600;
        assert_eq!(
            pick(&inputs, &u, &c).reason,
            "weekly resets in 3h52m with 26% unused"
        );
    }

    #[test]
    fn a_run_needs_an_hour_of_weekly_room() {
        let mut c = ctx(Policy::Smart);
        c.run_load = 25.0;
        let now = c.now;
        // a: an empty 5h window, but 92% of its week used (limited at 95)
        // with three days to go. An hour of the run is 5 points of a week.
        let inputs = vec![input("a"), input("b")];
        let mut u = HashMap::from([
            usage("a", 0.0, 92.0, Some(now + 3 * 86_400), now),
            usage("b", 50.0, 40.0, Some(now + 3 * 86_400), now),
        ]);
        let p = pick(&inputs, &u, &c);
        assert_eq!(
            (p.chosen.as_deref(), p.reason.as_str()),
            (Some("b"), "the only account with room for this run")
        );
        assert_eq!(
            reasons(&inputs, &u, &c)["a"].as_deref(),
            Some("under an hour of weekly room, resets in 3d0h")
        );
        // Where the week measured half as dear, the hour fits.
        let mut cheap = inputs.clone();
        cheap[0].week_per_5h = Some(0.1);
        assert_eq!(pick(&cheap, &u, &c).chosen.as_deref(), Some("a"));
        // Runs already there use the week too.
        cheap[0].load = 1.0;
        assert_eq!(pick(&cheap, &u, &c).chosen.as_deref(), Some("b"));
        // The week resets in 6 minutes: the run uses half a point of it.
        u.get_mut("a").unwrap().snapshot.as_mut().unwrap().seven_day =
            Some(prism_core::model::Window {
                utilization: 92.0,
                resets_at: Some(now + 360),
                inferred: false,
            });
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("a"));
    }

    #[test]
    fn without_load_only_limits_count() {
        let mut c = ctx(Policy::Smart);
        let now = c.now;
        let inputs = vec![input("a"), input("b")];
        // No forecast: a is under its weekly limit (95), so a run fits.
        let u = HashMap::from([
            usage("a", 0.0, 94.0, Some(now + 3 * 86_400), now),
            usage("b", 50.0, 40.0, Some(now + 3 * 86_400), now),
        ]);
        let p = pick(&inputs, &u, &c);
        assert_eq!(
            (p.chosen.as_deref(), p.reason.as_str()),
            (Some("a"), "most 5h headroom (100%)")
        );
        c.run_load = 25.0;
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("b"));
    }

    #[test]
    fn the_models_week_binds_its_runs() {
        let (inputs, mut u, mut c) = expiring();
        let now = c.now;
        let fable = |u: &mut HashMap<String, CachedUsage>, used: f64| {
            u.get_mut("pb2").unwrap().snapshot.as_mut().unwrap().scoped =
                vec![prism_core::model::ScopedWindow {
                    label: "Fable".into(),
                    kind: "weekly_scoped".into(),
                    utilization: used,
                    resets_at: Some(now + 13_920),
                    inferred: false,
                }];
        };
        fable(&mut u, 84.0);
        // No model known: the 7-day window alone.
        assert_eq!(
            pick(&inputs, &u, &c).reason,
            "weekly resets in 3h52m with 26% unused"
        );
        // A Fable run can use up 16 points, not 26.
        c.model = Some("claude-fable-5-2".into());
        let p = pick(&inputs, &u, &c);
        assert_eq!(
            (p.chosen.as_deref(), p.reason.as_str()),
            (Some("pb2"), "Fable week resets in 3h52m with 16% unused")
        );
        // At 92% an hour of the run would reach the model's limit.
        fable(&mut u, 92.0);
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("pb5"));
        assert_eq!(
            reasons(&inputs, &u, &c)["pb2"].as_deref(),
            Some("under an hour of Fable room, resets in 3h52m")
        );
        // A run of another model is not bound by it.
        c.model = Some("claude-opus-5-5".into());
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("pb2"));
    }

    #[test]
    fn room_comes_before_plan_and_weight() {
        let (mut inputs, u, c) = lanes();
        // c is a 20x plan the user weighs 5x, with runs on it: its score is
        // the highest, but the run does not fit (40 + 40 + 12.5).
        inputs[2].identity.rate_limit_tier = Some("default_claude_max_20x".into());
        inputs[2].weight = 5.0;
        inputs[2].load = 3.2;
        let a: Vec<Assessed> = inputs
            .iter()
            .map(|i| assess(i, u.get(&i.profile_id), &c))
            .collect();
        assert!(smart_score(&inputs[2], &a[2], &c) > smart_score(&inputs[0], &a[0], &c));
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("a"));
        // With room nowhere, the best score takes it.
        inputs[0].load = 3.0;
        inputs[1].load = 3.0;
        assert_eq!(pick(&inputs, &u, &c).chosen.as_deref(), Some("c"));
    }

    #[test]
    fn capacity_stops_at_a_weekly_limit() {
        let (inputs, mut u, c) = lanes();
        // Every week at 88% (limited at 95): one run each (93), not two.
        for id in ["a", "b", "c"] {
            let s = u.get_mut(id).unwrap().snapshot.as_mut().unwrap();
            s.seven_day.as_mut().unwrap().utilization = 88.0;
        }
        assert_eq!(capacity_left(&inputs, &u, &c), Some(3));
    }

    #[test]
    fn earliest_reset_names_the_profile() {
        let c = ctx(Policy::Smart);
        let now = c.now;
        let mut a = input("a");
        a.label = "Same".into();
        let mut ua = usage("a", 100.0, 0.0, None, now);
        ua.1.snapshot
            .as_mut()
            .unwrap()
            .five_hour
            .as_mut()
            .unwrap()
            .resets_at = Some(now + 60);
        let p = pick(&[a], &HashMap::from([ua]), &c);
        assert_eq!(p.earliest_id.as_deref(), Some("a"));
    }
}
