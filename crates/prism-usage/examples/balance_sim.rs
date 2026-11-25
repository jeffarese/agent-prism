//! Measures the `smart` policy (DESIGN §6.6): a pool of 5 accounts over 8
//! weeks, with every run placed by the real `balance::pick`.
//!
//! Sessions arrive at random, work part of the time (lanes all of it) and
//! move to another account only when theirs reaches a limit. Per level of
//! demand it prints the work served (percent of the pool's weekly quota),
//! the weekly quota that expired unused, the limit switches and the hours
//! a session waited with every account limited.
//!
//! Run it before and after a change to the score and compare:
//! `cargo run --release -p prism-usage --example balance_sim`

use std::collections::HashMap;

use prism_core::model::{AccountIdentity, Policy, TokenState, UsageSnapshot, Window};
use prism_usage::balance::{self, BalanceSettings};
use prism_usage::cache::{CachedUsage, Sample};
use prism_usage::infer;
use prism_usage::views::{AccountInput, ViewCtx};

/// Seconds per step.
const STEP: i64 = 180;
/// Points of the week one point of the 5h window costs.
const K: f64 = 0.16;
/// Percent of a 5h window a working run uses per hour (`balance.run_load`).
const RATE: f64 = 25.0;
const HARD: f64 = 98.0;
/// Accounts in the pool.
const N: usize = 5;
const DAY: i64 = 86_400;
const WEEKS: i64 = 8;
/// 5h points an uncached re-read of the conversation costs after a switch.
const REREAD: f64 = 2.0;

struct Rng(u64);
impl Rng {
    fn f(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Acct {
    u5: f64,
    reset5: Option<i64>,
    u7: f64,
    reset7: i64,
    samples: Vec<Sample>,
    last_used: Option<i64>,
}

struct Sess {
    acct: Option<usize>,
    born: i64,
    on_since: i64,
    dies: i64,
    lane: bool,
    busy: bool,
    rng: Rng,
}

#[derive(Default, Debug)]
struct Stats {
    starts: u32,
    served: f64,
    waste: f64,
    resets: u32,
    switch_5h: u32,
    switch_wk: u32,
    blocked_h: f64,
}

fn pick(
    accts: &[Acct],
    sess: &[Sess],
    me: usize,
    exclude: Option<usize>,
    now: i64,
) -> Option<usize> {
    let mut inputs = Vec::new();
    let mut usage = HashMap::new();
    for (a, acct) in accts.iter().enumerate() {
        let (mut load, mut n) = (0.0, 0);
        let mut fresh = 0.0;
        for (i, s) in sess.iter().enumerate() {
            if i == me || s.acct != Some(a) {
                continue;
            }
            let w = if s.busy { 1.0 } else { 0.25 };
            load += w;
            n += 1;
            fresh += w * (1.0 - (now - s.on_since) as f64 / 3600.0).clamp(0.0, 1.0);
        }
        let id = format!("a{a}");
        inputs.push(AccountInput {
            profile_id: id.clone(),
            label: id.clone(),
            color: None,
            in_pool: true,
            weight: 1.0,
            reserve_7d: 0,
            identity: AccountIdentity::default(),
            token: TokenState::Valid {
                expires_at: i64::MAX,
            },
            sessions_elsewhere: n,
            load,
            fresh,
            slope_5h: infer::rate_5h(&acct.samples, now),
            week_per_5h: infer::week_per_5h(&acct.samples),
            run_rate: None,
            active_here: false,
            last_used: acct.last_used,
        });
        let win = |u: f64, r| {
            Some(Window {
                utilization: u.floor(),
                resets_at: r,
                inferred: false,
            })
        };
        usage.insert(
            id,
            CachedUsage {
                snapshot: Some(UsageSnapshot {
                    fetched_at: now,
                    five_hour: win(acct.u5, acct.reset5),
                    seven_day: win(acct.u7, Some(acct.reset7)),
                    scoped: vec![],
                    extra_usage_enabled: false,
                    limit_reached: false,
                }),
                last_attempt: now,
                ..Default::default()
            },
        );
    }
    let ctx = ViewCtx {
        now,
        model: None,
        settings: BalanceSettings {
            policy: Policy::Smart,
            soft_5h: 90.0,
            hard: HARD,
        },
        exclude: exclude.map(|a| format!("a{a}")),
        stale_after_secs: 600,
        run_load: RATE,
        prefer: None,
        new_run: 1.0,
    };
    let p = balance::pick(&inputs, &usage, &ctx);
    p.chosen.map(|id| id[1..].parse().unwrap())
}

fn burn(a: &mut Acct, pts: f64, now: i64) {
    if a.reset5.is_none() {
        a.reset5 = Some(now + 5 * 3600);
    }
    a.u5 = (a.u5 + pts).min(100.0);
    a.u7 = (a.u7 + pts * K).min(100.0);
}

fn run(per_day: f64, lanes: f64, seed: u64) -> Stats {
    let t0 = 1_800_000_000i64;
    // Weekly resets spread over the week, as on a real pool.
    let offs = [0.6, 1.9, 3.1, 4.4, 6.2];
    let mut accts: Vec<Acct> = (0..N)
        .map(|i| Acct {
            u5: 0.0,
            reset5: None,
            u7: 0.0,
            reset7: t0 + (offs[i] * DAY as f64) as i64,
            samples: Vec::new(),
            last_used: None,
        })
        .collect();
    let mut sess: Vec<Sess> = Vec::new();
    let mut st = Stats::default();
    let mut arr = Rng(seed);
    let mut born = 0u64;
    let step_h = STEP as f64 / 3600.0;
    let pts = RATE * step_h;
    // The first week only fills the pool: it is not counted.
    let warm = t0 + 7 * DAY;
    let mut now = t0;
    while now < t0 + (WEEKS + 1) * 7 * DAY {
        let count = now >= warm;
        for a in &mut accts {
            if a.reset5.is_some_and(|r| now >= r) {
                a.u5 = 0.0;
                a.reset5 = None;
            }
            if now >= a.reset7 {
                if count {
                    st.waste += 100.0 - a.u7;
                    st.resets += 1;
                }
                a.u7 = 0.0;
                a.reset7 += 7 * DAY;
            }
        }
        // Arrivals: 14 day hours, a fifth of the rate at night.
        let hour = (now - t0) % DAY / 3600;
        let lam = per_day / 16.0 * if (9..23).contains(&hour) { 1.0 } else { 0.2 };
        if arr.f() < lam * step_h {
            born += 1;
            let mut rng = Rng(seed ^ born.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let lane = rng.f() < lanes;
            let life = if lane {
                0.5 + 1.5 * rng.f()
            } else {
                (-3.0 * (1.0 - rng.f()).ln()).clamp(0.2, 10.0)
            };
            sess.push(Sess {
                acct: None,
                born: now,
                on_since: now,
                dies: now + (life * 3600.0) as i64,
                lane,
                busy: true,
                rng,
            });
            if count {
                st.starts += 1;
            }
        }
        sess.retain(|s| s.dies > now);
        for i in 0..sess.len() {
            let age = now - sess[i].born;
            let duty = if sess[i].lane {
                1.0
            } else if age < 3600 {
                0.5
            } else {
                0.2
            };
            sess[i].busy = age == 0 || sess[i].rng.f() < duty;
            if sess[i].acct.is_none() {
                match pick(&accts, &sess, i, None, now) {
                    Some(a) => {
                        sess[i].acct = Some(a);
                        sess[i].on_since = now;
                        accts[a].last_used = Some(now);
                    }
                    None => {
                        if count {
                            st.blocked_h += step_h;
                        }
                        continue;
                    }
                }
            }
            if !sess[i].busy {
                continue;
            }
            let a = sess[i].acct.unwrap();
            let (l5, l7) = (accts[a].u5 >= HARD, accts[a].u7 >= HARD);
            if l5 || l7 {
                match pick(&accts, &sess, i, Some(a), now) {
                    Some(b) => {
                        sess[i].acct = Some(b);
                        sess[i].on_since = now;
                        accts[b].last_used = Some(now);
                        burn(&mut accts[b], REREAD, now);
                        if count {
                            if l5 {
                                st.switch_5h += 1;
                            } else {
                                st.switch_wk += 1;
                            }
                        }
                    }
                    None => {
                        if count {
                            st.blocked_h += step_h;
                        }
                    }
                }
                continue;
            }
            burn(&mut accts[a], pts, now);
            if count {
                st.served += pts;
            }
        }
        for a in &mut accts {
            a.samples.push(Sample {
                ts: now,
                u5h: Some(a.u5.floor()),
                u7d: Some(a.u7.floor()),
            });
            if a.samples.len() > 3400 {
                a.samples.drain(..40);
            }
        }
        now += STEP;
    }
    st
}

fn main() {
    println!("per week, mean of 6 seeds x {WEEKS} weeks");
    println!(
        "{:<26} {:>7} {:>8} {:>8} {:>10} {:>10} {:>9}",
        "demand", "starts", "served%", "unused%", "sw 5h/wk", "sw week/wk", "blocked h"
    );
    // Sessions per day; the pool serves about 125 busy hours a week.
    for (name, per_day, lanes) in [
        ("light (~45%)", 8.0, 0.2),
        ("normal (~65%)", 12.0, 0.2),
        ("heavy (~90%)", 17.0, 0.2),
        ("over (~115%)", 22.0, 0.2),
        ("heavy, half lanes", 14.0, 0.5),
    ] {
        let mut t = Stats::default();
        let seeds = [11u64, 22, 33, 44, 55, 66];
        for s in seeds {
            let r = run(per_day, lanes, s * 7919);
            t.starts += r.starts;
            t.served += r.served;
            t.waste += r.waste;
            t.resets += r.resets;
            t.switch_5h += r.switch_5h;
            t.switch_wk += r.switch_wk;
            t.blocked_h += r.blocked_h;
        }
        let wk = (seeds.len() as i64 * WEEKS) as f64;
        let cap = N as f64 * 100.0 / K;
        println!(
            "{:<26} {:>7.0} {:>8.1} {:>8.1} {:>10.1} {:>10.1} {:>9.1}",
            name,
            t.starts as f64 / wk,
            100.0 * t.served / wk / cap,
            t.waste / t.resets.max(1) as f64,
            t.switch_5h as f64 / wk,
            t.switch_wk as f64 / wk,
            t.blocked_h / wk
        );
    }
}
