//! `agp status` and `agp pick`.

use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use prism_core::model::{AccountView, TokenState, Tool, UsageStatus, Window};
use prism_usage::balance::{PickResult, fmt_duration};
use prism_usage::views;

use crate::app::AppCtx;
use crate::launch;
use crate::tokens::KeychainTokens;

struct Out {
    tool: Tool,
    views: Vec<AccountView>,
    pick: PickResult,
    now: i64,
}

/// The accounts of `tool` and the balancer's pick among them.
fn gather(ctx: &AppCtx, tool: Tool) -> Result<Out> {
    let config = ctx.load_config()?;
    let profiles = config.profiles_for(&ctx.paths, tool);
    let tokens = Arc::new(KeychainTokens::new(ctx.store.clone(), profiles.clone()));
    let model = match tool {
        Tool::Claude => launch::session_model(&[], &config.common_home_for(&ctx.paths, tool)),
        Tool::Codex => None,
    };
    let snap = launch::snapshot(ctx, &config, &profiles, &tokens, None, model);
    let proj = launch::projections(&ctx.cache, &snap.usage, snap.ctx.now);
    let views = views::build_views(&snap.inputs, &snap.usage, &proj, &snap.ctx);
    Ok(Out {
        tool,
        views,
        pick: snap.pick,
        now: snap.ctx.now,
    })
}

/// One [`Out`] per tool: Claude always, Codex when it has accounts.
fn gather_all(ctx: &AppCtx) -> Result<Vec<Out>> {
    let mut outs = vec![gather(ctx, Tool::Claude)?];
    let config = ctx.load_config()?;
    if !config.profiles_for(&ctx.paths, Tool::Codex).is_empty() {
        outs.push(gather(ctx, Tool::Codex)?);
    }
    Ok(outs)
}

fn pick_json(p: &PickResult) -> serde_json::Value {
    serde_json::json!({ "chosen": p.chosen, "reason": p.reason,
                        "earliest_reset": p.earliest_reset })
}

fn paint(on: bool, code: &str, s: &str) -> String {
    if on {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn level(u: f64) -> &'static str {
    if u >= 85.0 {
        "31"
    } else if u >= 60.0 {
        "33"
    } else {
        "32"
    }
}

fn window(w: Option<&Window>, now: i64, color: bool) -> (String, String) {
    match w {
        None => ("—".into(), String::new()),
        Some(w) => {
            let pct = format!("{}{:.0}%", if w.inferred { "~" } else { "" }, w.utilization);
            let reset = w
                .resets_at
                .map(|t| format!("↻{}", fmt_duration(t - now)))
                .unwrap_or_default();
            (paint(color, level(w.utilization), &pct), reset)
        }
    }
}

/// The state column.
pub fn state(v: &AccountView) -> &'static str {
    if v.limited.is_some() {
        "limited"
    } else if matches!(v.token, TokenState::Missing) {
        "no login"
    } else if matches!(v.token, TokenState::Expired { .. }) {
        "expired"
    } else {
        match v.usage_status {
            UsageStatus::Fresh => "fresh",
            UsageStatus::Stale { .. } => "stale",
            UsageStatus::Unavailable(_) => "no data",
        }
    }
}

fn print_table(o: &Out, color: bool) {
    // Pads to `w` visible columns (ANSI sequences take none).
    let pad = |s: &str, w: usize| {
        let visible = strip_ansi(s).chars().count();
        format!("{s}{}", " ".repeat(w.saturating_sub(visible)))
    };
    println!(
        "{}",
        paint(
            color,
            "1",
            &format!(
                "  {:<14} {:<28} {:<8} {:<6} {:<8} {:<6} {:<8} {:<12} {:<8} {:<4}",
                "ACCOUNT", "EMAIL", "PLAN", "5H", "RESET", "7D", "RESET", "SCOPED", "STATE", "POOL"
            )
        )
    );
    for v in &o.views {
        let u = v.usage.as_ref();
        let (p5, r5) = window(u.and_then(|u| u.five_hour.as_ref()), o.now, color);
        let (p7, r7) = window(u.and_then(|u| u.seven_day.as_ref()), o.now, color);
        let scoped = u
            .map(|u| {
                u.scoped
                    .iter()
                    .map(|s| format!("{} {:.0}%", s.label, s.utilization))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let plan = v
            .identity
            .subscription
            .clone()
            .unwrap_or_else(|| "—".into());
        let st = state(v);
        let st_col = match st {
            "fresh" => paint(color, "32", st),
            "limited" => paint(color, "31", st),
            _ => paint(color, "2", st),
        };
        let marker = if v.next_pick { "▸" } else { " " };
        let reason = v.reason.clone().unwrap_or_default();
        println!(
            "{marker} {:<14} {:<28} {:<8} {} {:<8} {} {:<8} {:<12} {} {:<4} {}",
            v.label,
            v.identity.email.clone().unwrap_or_else(|| "—".into()),
            plan,
            pad(&p5, 6),
            r5,
            pad(&p7, 6),
            r7,
            scoped,
            pad(&st_col, 8),
            if v.in_pool { "yes" } else { "no" },
            paint(color, "2", &reason),
        );
    }
    match &o.pick.chosen {
        Some(id) => println!("\nnext: {id} — {}", o.pick.reason),
        None => println!("\nnext: none — {}", o.pick.reason),
    }
}

fn print_all(outs: &[Out], color: bool) {
    let titled = outs.len() > 1;
    for (i, o) in outs.iter().enumerate() {
        if titled {
            if i > 0 {
                println!();
            }
            let title = format!("{} ({} plans)", o.tool.name(), o.tool.plan_name());
            println!("{}", paint(color, "1;4", &title));
        }
        print_table(o, color);
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in s.chars() {
        if esc {
            if c == 'm' {
                esc = false;
            }
        } else if c == '\x1b' {
            esc = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// `agp status [--json] [--watch]`.
pub fn run_status(ctx: Arc<AppCtx>, json: bool, watch: bool) -> Result<i32> {
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    loop {
        let outs = gather_all(&ctx)?;
        if json {
            // `pick` is the Claude pick (the old shape); `picks` has one
            // per tool. Each view names its tool in `identity.tool`.
            let views: Vec<&AccountView> = outs.iter().flat_map(|o| &o.views).collect();
            let picks: serde_json::Map<String, serde_json::Value> = outs
                .iter()
                .map(|o| (o.tool.bin().to_string(), pick_json(&o.pick)))
                .collect();
            let v = serde_json::json!({
                "views": views,
                "pick": pick_json(&outs[0].pick),
                "picks": picks,
            });
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            if watch && color {
                print!("\x1b[H\x1b[2J");
            }
            print_all(&outs, color);
        }
        if !watch {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_secs(30));
    }
}

/// `agp pick [--codex] [--json]`.
pub fn run_pick(ctx: Arc<AppCtx>, json: bool, tool: Tool) -> Result<i32> {
    let o = gather(&ctx, tool)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&pick_json(&o.pick))?);
    } else {
        match &o.pick.chosen {
            Some(id) => println!("{id}\t{}", o.pick.reason),
            None => println!("-\t{}", o.pick.reason),
        }
    }
    Ok(if o.pick.chosen.is_some() { 0 } else { 1 })
}
