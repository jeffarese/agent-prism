//! Builds the [`AccountView`]s the dock and the balancer share (DESIGN §6.6).

use std::collections::HashMap;

use prism_core::model::{
    ACCOUNT_COLORS, AccountIdentity, AccountView, LimitKind, Projection, TokenState, UsageStatus,
};

use crate::balance::{self, BalanceSettings, Tight, fmt_duration};
use crate::cache::CachedUsage;

/// One configured account, as the `agp` binary sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountInput {
    pub profile_id: String,
    pub label: String,
    pub color: Option<String>,
    pub in_pool: bool,
    /// Multiplier from the config (1.0 by default).
    pub weight: f64,
    /// Weekly percent kept in reserve.
    pub reserve_7d: u8,
    pub identity: AccountIdentity,
    pub token: TokenState,
    /// Live agent-prism runs on this account in other processes (claims
    /// from the run registry and watched background sessions).
    pub sessions_elsewhere: u32,
    /// Their weight for the balancer: a working run is 1, an idle
    /// interactive session a quarter.
    pub load: f64,
    /// The part of `load` that `slope_5h` does not show yet: runs that
    /// came to the account within the last hour, less as they age.
    pub fresh: f64,
    /// The 5h burn rate in this account's usage samples of the last hour,
    /// percent per hour (all use, also from outside agent-prism).
    pub slope_5h: Option<f64>,
    /// Points of the weekly window one point of the 5h window costs on
    /// this account, from its samples; `None`: [`balance::WEEK_PER_5H`].
    pub week_per_5h: Option<f64>,
    /// Learned percent of this account's 5h window one run uses per hour;
    /// `None`: `run_load` scaled by the plan.
    pub run_rate: Option<f64>,
    /// This agent-prism session runs on this account.
    pub active_here: bool,
    /// When a session last started on this account (unix seconds).
    pub last_used: Option<i64>,
}

/// Context for building views and picking.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewCtx {
    pub now: i64,
    /// Model id in use, for scoped windows.
    pub model: Option<String>,
    pub settings: BalanceSettings,
    /// Profile never picked (e.g. the current one when switching).
    pub exclude: Option<String>,
    /// Usage older than this is shown as stale.
    pub stale_after_secs: i64,
    /// `balance.run_load`: percent of a 5h window one run uses per hour on
    /// a capacity-1.0 plan; 0 turns the load off.
    pub run_load: f64,
    /// Start on this account when it is usable and has room (a resume on
    /// the account that last ran the session).
    pub prefer: Option<String>,
    /// How many typical runs the run being placed counts as: 1, or what
    /// agent-prism learned from past runs like it (DESIGN §11f).
    pub new_run: f64,
}

fn status(cached: Option<&CachedUsage>, ctx: &ViewCtx) -> UsageStatus {
    match cached {
        Some(CachedUsage {
            snapshot: Some(s),
            last_error,
            ..
        }) => {
            let age = (ctx.now - s.fetched_at).max(0);
            if age > ctx.stale_after_secs || last_error.is_some() {
                UsageStatus::Stale { age_secs: age }
            } else {
                UsageStatus::Fresh
            }
        }
        Some(CachedUsage {
            last_error: Some(e),
            ..
        }) => UsageStatus::Unavailable(e.clone()),
        _ => UsageStatus::Unavailable("no usage data yet".into()),
    }
}

/// When the weekly window of `v` resets, if that is known and still ahead.
fn reset_7d(v: &AccountView, now: i64) -> Option<i64> {
    v.usage
        .as_ref()?
        .seven_day
        .as_ref()?
        .resets_at
        .filter(|t| *t > now)
}

/// Builds one view per input, with limits, scores, reasons and the next
/// pick marked.
///
/// The views are in display order: the account whose weekly window resets
/// first comes first, and accounts without a coming weekly reset follow in
/// input order. An account without a config color gets the color of its
/// input position, so a color stays with its account when the order changes.
pub fn build_views(
    inputs: &[AccountInput],
    usage: &HashMap<String, CachedUsage>,
    projections: &HashMap<String, Projection>,
    ctx: &ViewCtx,
) -> Vec<AccountView> {
    let assessed: Vec<_> = inputs
        .iter()
        .map(|i| balance::assess(i, usage.get(&i.profile_id), ctx))
        .collect();
    let pick = balance::pick_assessed(inputs, &assessed, ctx);
    let mut views: Vec<AccountView> = inputs
        .iter()
        .zip(assessed)
        .enumerate()
        .map(|(n, (i, a))| {
            let cached = usage.get(&i.profile_id);
            let chosen = pick.chosen.as_deref() == Some(i.profile_id.as_str());
            let score = (a.eligible && a.limited.is_none())
                .then(|| balance::policy_score(i, &a, ctx))
                .flatten();
            let reason = if chosen {
                Some(pick.reason.clone())
            } else if let Some(l) = &a.limited {
                let what = match l.kind {
                    LimitKind::FiveHour => "5h limit",
                    LimitKind::SevenDay => "weekly limit",
                    LimitKind::Scoped => "model limit",
                };
                Some(match l.until {
                    Some(t) => format!("{what}, resets in {}", fmt_duration(t - ctx.now)),
                    None => what.to_string(),
                })
            } else if matches!(i.token, TokenState::Missing) {
                Some("not logged in".into())
            } else if !i.in_pool {
                Some("not in pool".into())
            } else if ctx.exclude.as_deref() == Some(i.profile_id.as_str()) {
                None
            } else {
                // Why the run does not fit here (DESIGN §6.6): a forecast,
                // so the words say "would", not "is".
                a.tight(ctx.settings.soft_5h).map(|t| {
                    let (what, until) = match t {
                        Tight::FiveHour => {
                            ("5h would fill within the hour".to_string(), a.reset_5h)
                        }
                        Tight::Week(w) => (
                            format!(
                                "under an hour of {} room",
                                w.label.as_deref().unwrap_or("weekly")
                            ),
                            w.resets_at,
                        ),
                    };
                    match until {
                        Some(t) => format!("{what}, resets in {}", fmt_duration(t - ctx.now)),
                        None => what,
                    }
                })
            };
            AccountView {
                profile_id: i.profile_id.clone(),
                label: i.label.clone(),
                color: i
                    .color
                    .clone()
                    .or_else(|| Some(ACCOUNT_COLORS[n % ACCOUNT_COLORS.len()].into())),
                identity: i.identity.clone(),
                token: i.token.clone(),
                usage_status: status(cached, ctx),
                usage: a.snapshot.clone(),
                projection: projections.get(&i.profile_id).cloned().unwrap_or_default(),
                in_pool: i.in_pool,
                active_here: i.active_here,
                active_elsewhere: i.sessions_elsewhere,
                limited: a.limited.clone(),
                next_pick: chosen,
                score,
                reason,
            }
        })
        .collect();
    // Stable: accounts with the same key keep their input order.
    views.sort_by_key(|v| {
        let t = reset_7d(v, ctx.now);
        (t.is_none(), t)
    });
    views
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use prism_core::model::{Policy, UsageSnapshot, Window};

    pub(crate) fn input(id: &str) -> AccountInput {
        AccountInput {
            profile_id: id.into(),
            label: id.to_uppercase(),
            color: None,
            in_pool: true,
            weight: 1.0,
            reserve_7d: 0,
            identity: AccountIdentity::default(),
            token: TokenState::Valid {
                expires_at: i64::MAX,
            },
            sessions_elsewhere: 0,
            load: 0.0,
            fresh: 0.0,
            slope_5h: None,
            week_per_5h: None,
            run_rate: None,
            active_here: false,
            last_used: None,
        }
    }

    pub(crate) fn ctx(policy: Policy) -> ViewCtx {
        ViewCtx {
            now: 1_800_000_000,
            model: None,
            settings: BalanceSettings {
                policy,
                soft_5h: 80.0,
                hard: 95.0,
            },
            exclude: None,
            stale_after_secs: 600,
            run_load: 0.0,
            prefer: None,
            new_run: 1.0,
        }
    }

    pub(crate) fn usage(
        id: &str,
        u5h: f64,
        u7d: f64,
        reset_7d: Option<i64>,
        fetched_at: i64,
    ) -> (String, CachedUsage) {
        let snap = UsageSnapshot {
            fetched_at,
            five_hour: Some(Window {
                utilization: u5h,
                resets_at: None,
                inferred: false,
            }),
            seven_day: Some(Window {
                utilization: u7d,
                resets_at: reset_7d,
                inferred: false,
            }),
            scoped: vec![],
            extra_usage_enabled: false,
            limit_reached: false,
        };
        (
            id.into(),
            CachedUsage {
                snapshot: Some(snap),
                last_attempt: fetched_at,
                ..Default::default()
            },
        )
    }

    #[test]
    fn views_are_ordered_by_weekly_reset() {
        let c = ctx(Policy::Smart);
        let now = c.now;
        let day = 86_400;
        let mut e = input("e");
        e.color = Some("white".into());
        let u = HashMap::from([
            usage("a", 0.0, 10.0, Some(now + 5 * day), now),
            // No weekly reset known, and one that has passed.
            usage("b", 0.0, 10.0, None, now),
            usage("c", 0.0, 10.0, Some(now - 60), now),
            usage("d", 0.0, 10.0, Some(now + day), now),
            usage("e", 0.0, 10.0, Some(now + 3 * day), now),
            // "f" has no usage at all.
        ]);
        let v = build_views(
            &[
                input("a"),
                input("b"),
                input("c"),
                input("d"),
                e,
                input("f"),
            ],
            &u,
            &HashMap::new(),
            &c,
        );
        let ids: Vec<&str> = v.iter().map(|v| v.profile_id.as_str()).collect();
        assert_eq!(ids, ["d", "e", "a", "b", "c", "f"]);
        // Colors follow the account's place in the config, not in the list.
        let colors: Vec<&str> = v.iter().filter_map(|v| v.color.as_deref()).collect();
        assert_eq!(colors, ["green", "white", "cyan", "magenta", "blue", "red"]);
    }

    #[test]
    fn views_status_limits_and_pick() {
        let c = ctx(Policy::Smart);
        let now = c.now;
        let mut stale = usage("b", 99.0, 0.0, None, now - 3600);
        stale
            .1
            .snapshot
            .as_mut()
            .unwrap()
            .five_hour
            .as_mut()
            .unwrap()
            .resets_at = Some(now + 600);
        let mut past = usage("c", 99.0, 10.0, None, now);
        past.1
            .snapshot
            .as_mut()
            .unwrap()
            .five_hour
            .as_mut()
            .unwrap()
            .resets_at = Some(now - 1);
        let mut d = input("d");
        d.token = TokenState::Missing;
        let u = HashMap::from([usage("a", 20.0, 20.0, None, now), stale, past]);
        let v = build_views(
            &[input("a"), input("b"), input("c"), d],
            &u,
            &HashMap::new(),
            &c,
        );
        assert_eq!(v[0].usage_status, UsageStatus::Fresh);
        assert_eq!(v[1].usage_status, UsageStatus::Stale { age_secs: 3600 });
        assert_eq!(v[1].reason.as_deref(), Some("5h limit, resets in 10m"));
        assert!(v[1].score.is_none());
        // Past reset inferred to 0%: not limited.
        let w = v[2].usage.as_ref().unwrap().five_hour.as_ref().unwrap();
        assert!(w.inferred && v[2].limited.is_none());
        assert!(matches!(v[3].usage_status, UsageStatus::Unavailable(_)));
        assert_eq!(v[3].reason.as_deref(), Some("not logged in"));
        assert_eq!(v.iter().filter(|x| x.next_pick).count(), 1);
        assert!(
            v[2].next_pick,
            "{:?}",
            v.iter().map(|x| &x.reason).collect::<Vec<_>>()
        );
    }
}
