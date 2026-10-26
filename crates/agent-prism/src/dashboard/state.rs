//! `/api/state`: every account now, the live sessions and the next pick
//! per tool. Built from the same calls as `agp status --json`, mapped to
//! a dashboard shape that never carries a secret.

use std::collections::HashMap;

use prism_core::config::Config;
use prism_core::model::{AccountView, LimitKind, TokenState, Tool, UsageStatus, Window};
use prism_core::profile::Profile;
use prism_core::registry;
use prism_usage::balance;
use prism_usage::cache::CachedUsage;
use prism_usage::views;
use serde_json::{Value, json};

use super::activity::project_name;
use crate::app::AppCtx;
use crate::launch;
use crate::tokens::KeychainTokens;

/// The number of categorical colors; later accounts are gray.
pub const SLOTS: usize = 8;

/// The categorical color slot of each profile, in config order: a config
/// color takes its matching slot when that is free, the others take the
/// first free slot. Colors follow the account, never its rank.
pub fn color_slots(profiles: &[(String, Option<String>)]) -> HashMap<String, Option<usize>> {
    let wanted = |c: &str| match c {
        "blue" => Some(0),
        "cyan" => Some(2),
        "yellow" => Some(3),
        "magenta" => Some(4),
        "green" => Some(5),
        "red" => Some(7),
        _ => None,
    };
    let mut taken = [false; SLOTS];
    let mut out: HashMap<String, Option<usize>> = HashMap::new();
    for (id, color) in profiles {
        if let Some(s) = color.as_deref().and_then(wanted)
            && !taken[s]
        {
            taken[s] = true;
            out.insert(id.clone(), Some(s));
        }
    }
    for (id, _) in profiles {
        if out.contains_key(id) {
            continue;
        }
        let s = taken.iter().position(|t| !t);
        if let Some(s) = s {
            taken[s] = true;
        }
        out.insert(id.clone(), s);
    }
    out
}

fn window(w: Option<&Window>, projected: Option<f64>) -> Value {
    match w {
        None => Value::Null,
        Some(w) => json!({
            "util": w.utilization,
            "resets_at": w.resets_at,
            "inferred": w.inferred,
            "projected": projected,
        }),
    }
}

fn limit_code(k: LimitKind) -> &'static str {
    match k {
        LimitKind::FiveHour => "5h",
        LimitKind::SevenDay => "7d",
        LimitKind::Scoped => "scoped",
    }
}

/// One account for the page. Token errors become a fixed word: their
/// text is never sent.
pub fn account_json(
    v: &AccountView,
    cached: Option<&CachedUsage>,
    slot: Option<usize>,
    last_used: Option<i64>,
) -> Value {
    let token = match &v.token {
        TokenState::Valid { expires_at } => {
            json!({"state": "valid", "expires_at": (*expires_at != i64::MAX).then_some(*expires_at)})
        }
        TokenState::Expired { since } => json!({"state": "expired", "since": since}),
        TokenState::Missing => json!({"state": "missing"}),
        TokenState::Unreadable(_) => json!({"state": "unreadable"}),
    };
    let usage_status = match &v.usage_status {
        UsageStatus::Fresh => json!({"state": "fresh"}),
        UsageStatus::Stale { age_secs } => json!({"state": "stale", "age_secs": age_secs}),
        UsageStatus::Unavailable(m) => json!({"state": "unavailable", "message": m}),
    };
    let u = v.usage.as_ref();
    let scoped: Vec<Value> = u
        .map(|u| {
            u.scoped
                .iter()
                .map(|s| {
                    json!({"label": s.label, "util": s.utilization,
                           "resets_at": s.resets_at, "inferred": s.inferred})
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": v.profile_id,
        "label": v.label,
        "tool": v.identity.tool.bin(),
        "slot": slot,
        "name": v.identity.display_name,
        "email": v.identity.email,
        "org": v.identity.org,
        "plan": v.identity.subscription,
        "tier": v.identity.rate_limit_tier,
        "capacity": v.identity.capacity(),
        "token": token,
        "usage_status": usage_status,
        "state": crate::status::state(v),
        "fetched_at": u.map(|u| u.fetched_at),
        "last_attempt": cached.map(|c| c.last_attempt).filter(|t| *t > 0),
        "last_error": cached.and_then(|c| c.last_error.clone()),
        "backoff_until": cached.and_then(|c| c.backoff_until),
        "five_hour": window(u.and_then(|u| u.five_hour.as_ref()), v.projection.five_hour),
        "seven_day": window(u.and_then(|u| u.seven_day.as_ref()), v.projection.seven_day),
        "scoped": scoped,
        "limit_reached": u.is_some_and(|u| u.limit_reached),
        "extra_usage": u.is_some_and(|u| u.extra_usage_enabled),
        "limited": v.limited.as_ref().map(|l| json!({"kind": limit_code(l.kind), "until": l.until})),
        "in_pool": v.in_pool,
        "next_pick": v.next_pick,
        "score": v.score,
        "reason": v.reason,
        "sessions": v.active_elsewhere,
        "last_used": last_used,
    })
}

/// The whole `/api/state` value.
pub fn build(
    app: &AppCtx,
    config: &Config,
    profiles: &[Profile],
    tokens: &KeychainTokens,
    now: i64,
) -> Value {
    let slots = color_slots(
        &profiles
            .iter()
            .map(|p| (p.id.clone(), p.color.clone()))
            .collect::<Vec<_>>(),
    );
    let last = launch::load_last_used(&app.paths);
    let mut accounts = Vec::new();
    let mut picks = Vec::new();
    for tool in Tool::ALL {
        let ps: Vec<Profile> = profiles
            .iter()
            .filter(|p| p.tool == tool)
            .cloned()
            .collect();
        if ps.is_empty() {
            continue;
        }
        let model = match tool {
            Tool::Claude => launch::session_model(&[], &config.common_home_for(&app.paths, tool)),
            Tool::Codex => None,
        };
        let inputs = launch::account_inputs(&app.paths, &ps, tokens, None, now);
        let usage = launch::load_usage(&app.cache, &ps);
        let proj = launch::projections(&app.cache, &usage, now);
        let ctx = launch::view_ctx(config, None, model.clone(), None, now);
        let views = views::build_views(&inputs, &usage, &proj, &ctx);
        let pick = balance::pick(&inputs, &usage, &ctx);
        let label = |id: &str| {
            ps.iter()
                .find(|p| p.id == id)
                .map_or(id.to_string(), |p| p.label.clone())
        };
        picks.push(json!({
            "tool": tool.bin(),
            "name": tool.name(),
            "plan_name": tool.plan_name(),
            "model": model,
            "chosen": pick.chosen,
            "chosen_label": pick.chosen.as_deref().map(label),
            "reason": pick.reason,
            "earliest_reset": pick.earliest_reset.as_ref().map(|(l, t)| json!({"label": l, "at": t})),
        }));
        for v in &views {
            let slot = slots.get(&v.profile_id).copied().flatten();
            accounts.push(account_json(
                v,
                usage.get(&v.profile_id),
                slot,
                last.get(&v.profile_id).copied(),
            ));
        }
    }
    let labels: HashMap<&str, &str> = profiles
        .iter()
        .map(|p| (p.id.as_str(), p.label.as_str()))
        .collect();
    let live: Vec<Value> = registry::list_live(&app.paths)
        .into_iter()
        .map(|r| {
            let cwd = r.cwd.to_string_lossy().into_owned();
            json!({
                "pid": r.pid,
                "profile": r.profile,
                "label": labels.get(r.profile.as_str()),
                "session": r.session_id.as_deref().map(|s| s.chars().take(8).collect::<String>()),
                "project": project_name(Some(&cwd)),
                "cwd": cwd,
                "started_at": r.started_at,
                "switched_at": r.switched_at,
            })
        })
        .collect();
    let journal = prism_core::journal::file(&app.paths);
    json!({
        "now": now,
        "version": env!("CARGO_PKG_VERSION"),
        "settings": {
            "policy": config.balance.policy,
            "on_limit": config.balance.on_limit,
            "soft_5h": config.balance.soft_5h,
            "hard": config.balance.hard,
            "poll_idle_secs": config.usage.poll_idle_secs,
            "polling": !launch::polling_disabled(),
        },
        "accounts": accounts,
        "picks": picks,
        "live": live,
        "journal": {
            "path": journal.to_string_lossy(),
            "exists": journal.is_file(),
        },
        "slots": SLOTS,
    })
}
