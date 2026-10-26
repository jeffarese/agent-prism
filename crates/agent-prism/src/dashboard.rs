//! `agp dashboard`: a local web dashboard of usage, runs and limits.
//!
//! A small HTTP server on 127.0.0.1 serves one embedded page (HTML, CSS,
//! JS; no network access needed) and three JSON endpoints:
//!
//! - `/api/state`: every account now, the live sessions, the next pick
//!   per tool.
//! - `/api/history?range=24h|7d`: utilization samples per account.
//! - `/api/activity?range=24h|7d`: runs, events and distributions from the
//!   run journal, with tokens from the transcripts of the journal's
//!   sessions.
//!
//! Requests must be GET and name the server in `Host` (DNS rebinding).
//! While it runs, a background thread refreshes stale usage every
//! `usage.poll_idle_secs` with the same per-profile lock as the sessions.

mod activity;
mod history;
mod http;
mod state;
mod tokens;

use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use prism_core::config::Config;
use prism_core::journal;
use prism_core::model::Tool;
use prism_core::profile::Profile;
use prism_core::registry;
use serde_json::json;

use crate::app::AppCtx;
use crate::launch;
use crate::tokens::KeychainTokens;
use http::{Request, Response};

const INDEX_HTML: &str = include_str!("dashboard/index.html");
const APP_CSS: &str = include_str!("dashboard/app.css");
const APP_JS: &str = include_str!("dashboard/app.js");
const ICON_SVG: &str = include_str!("dashboard/icon.svg");

/// Connections served at the same time; more get a 503.
const MAX_CONNS: usize = 32;
/// Journal events this much older than the range are read too, so that
/// runs that started before it keep their start.
const JOURNAL_SLACK_SECS: i64 = 3 * 86_400;

/// Shared by the connection threads and the refresher.
struct Dash {
    ctx: Arc<AppCtx>,
    port: u16,
    config: Mutex<(Option<SystemTime>, Config, Vec<Profile>)>,
    tokens: Arc<KeychainTokens>,
    scanner: tokens::Scanner,
    conns: AtomicUsize,
    stop: Arc<AtomicBool>,
}

impl Dash {
    /// The config and profiles, reloaded when the file's mtime changes.
    fn config(&self) -> (Config, Vec<Profile>) {
        let file = self.ctx.paths.config_file();
        let mtime = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
        let mut c = self.config.lock().unwrap_or_else(|e| e.into_inner());
        if c.0 != mtime {
            match self.ctx.load_config() {
                Ok(cfg) => {
                    let profiles = cfg.profiles(&self.ctx.paths);
                    self.tokens.set_profiles(profiles.clone());
                    *c = (mtime, cfg, profiles);
                }
                Err(e) => log::warn!("dashboard: config reload: {e:#}"),
            }
        }
        (c.1.clone(), c.2.clone())
    }
}

/// Serves the dashboard on 127.0.0.1 until interrupted.
pub fn run(ctx: Arc<AppCtx>, port: Option<u16>, open: bool) -> Result<i32> {
    let listener = TcpListener::bind(("127.0.0.1", port.unwrap_or(0)))
        .with_context(|| format!("binding 127.0.0.1:{}", port.unwrap_or(0)))?;
    let port = listener.local_addr()?.port();
    let config = ctx.load_config()?;
    let profiles = config.profiles(&ctx.paths);
    let scanner = tokens::Scanner::new(
        &config.common_home_for(&ctx.paths, Tool::Claude),
        &config.common_home_for(&ctx.paths, Tool::Codex),
    );
    let mtime = std::fs::metadata(ctx.paths.config_file())
        .and_then(|m| m.modified())
        .ok();
    let stop = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(sig, stop.clone()).context("installing signal handlers")?;
    }
    let dash = Arc::new(Dash {
        tokens: Arc::new(KeychainTokens::new(ctx.store.clone(), profiles.clone())),
        ctx,
        port,
        config: Mutex::new((mtime, config, profiles)),
        scanner,
        conns: AtomicUsize::new(0),
        stop: stop.clone(),
    });
    let refresher = {
        let d = dash.clone();
        std::thread::spawn(move || refresh_loop(&d))
    };

    let url = format!("http://127.0.0.1:{port}/");
    println!("agent-prism dashboard: {url}");
    println!("Ctrl+C stops it.");
    log::info!("dashboard on {url}");
    if open {
        match std::process::Command::new("/usr/bin/open")
            .arg(&url)
            .spawn()
        {
            // Reap it in the background; `open` returns at once.
            Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
            Err(e) => eprintln!("agp: could not open the browser: {e}"),
        }
    }

    listener.set_nonblocking(true)?;
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if dash.conns.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
                    dash.conns.fetch_sub(1, Ordering::SeqCst);
                    let mut s = stream;
                    let _ = s.set_nonblocking(false);
                    let _ = Response::text(503, "busy").write_to(&mut s);
                    continue;
                }
                let d = dash.clone();
                std::thread::spawn(move || {
                    serve(&d, stream);
                    d.conns.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                log::warn!("dashboard accept: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    let _ = refresher.join();
    println!("\nDashboard stopped.");
    Ok(0)
}

/// Refreshes stale usage of every account now and then every
/// `poll_idle_secs`, until the server stops.
fn refresh_loop(d: &Dash) {
    loop {
        let (config, profiles) = d.config();
        launch::refresh_stale(
            &d.ctx.cache,
            &config,
            &profiles,
            &d.tokens,
            Duration::from_secs(15),
        );
        let wait = Duration::from_secs(config.usage.poll_idle_secs.max(30));
        let until = std::time::Instant::now() + wait;
        while std::time::Instant::now() < until {
            if d.stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn serve(d: &Dash, mut stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let resp = match http::read_request(&mut stream) {
        Ok(req) => route(d, &req),
        Err(_) => Response::text(400, "bad request"),
    };
    let _ = resp.write_to(&mut stream);
}

fn route(d: &Dash, req: &Request) -> Response {
    if !http::host_ok(req.header("host"), d.port) {
        return Response::text(403, "forbidden host");
    }
    if req.method != "GET" {
        return Response::text(405, "GET only");
    }
    // A page from another site may not read or trigger anything here.
    if req
        .header("sec-fetch-site")
        .is_some_and(|s| s == "cross-site" || s == "same-site")
    {
        return Response::text(403, "cross-site request");
    }
    let range = parse_range(req.query.get("range").map(String::as_str));
    let tool = match req.query.get("tool").map(String::as_str) {
        Some("claude") => Some(Tool::Claude),
        Some("codex") => Some(Tool::Codex),
        _ => None,
    };
    match req.path.as_str() {
        "/" | "/index.html" => Response::new(200, "text/html; charset=utf-8", INDEX_HTML),
        "/app.css" => Response::new(200, "text/css; charset=utf-8", APP_CSS),
        "/app.js" => Response::new(200, "text/javascript; charset=utf-8", APP_JS),
        "/icon.svg" | "/favicon.ico" => Response::new(200, "image/svg+xml", ICON_SVG),
        "/api/state" => {
            let (config, profiles) = d.config();
            let now = launch::unix_now();
            Response::json(&state::build(&d.ctx, &config, &profiles, &d.tokens, now))
        }
        "/api/history" => Response::json(&history_json(d, range)),
        "/api/activity" => Response::json(&activity_json(d, range, tool)),
        _ => Response::text(404, "not found"),
    }
}

/// `24h` or `7d` (the default) in seconds.
fn parse_range(s: Option<&str>) -> i64 {
    match s {
        Some("24h") => 86_400,
        _ => 7 * 86_400,
    }
}

fn history_json(d: &Dash, range: i64) -> serde_json::Value {
    let (_, profiles) = d.config();
    let now = launch::unix_now();
    let since = now - range;
    let bucket = history::bucket_secs(range);
    let series: Vec<_> = profiles
        .iter()
        .map(|p| {
            let samples = d.ctx.cache.samples(&p.id, since);
            json!({"id": p.id, "points": history::bucket(&samples, since, now, bucket)})
        })
        .collect();
    json!({"since": since, "now": now, "bucket_secs": bucket, "series": series})
}

fn activity_json(d: &Dash, range: i64, tool: Option<Tool>) -> activity::Activity {
    let now = launch::unix_now();
    let since = now - range;
    let mut events = journal::read_since(&d.ctx.paths, since - JOURNAL_SLACK_SECS);
    if let Some(t) = tool {
        events.retain(|e| e.tool == t);
    }
    let live: Vec<(u32, i64)> = registry::list_live(&d.ctx.paths)
        .iter()
        .map(|r| (r.pid, r.started_at))
        .collect();
    let runs = activity::build_runs(&events, &live, now);
    let mut recs: HashMap<String, Vec<activity::TokenRec>> = HashMap::new();
    for r in runs.iter().filter(|r| r.end >= since || !r.ended) {
        for s in &r.sessions {
            if !recs.contains_key(s) {
                let v = d.scanner.records(r.tool, s, r.cwd.as_deref());
                recs.insert(s.clone(), v);
            }
        }
    }
    let tz = jiff::tz::TimeZone::system();
    let local = move |ts: i64| -> (usize, usize, i64) {
        let Ok(t) = jiff::Timestamp::from_second(ts) else {
            return (0, 0, 0);
        };
        let z = t.to_zoned(tz.clone());
        (
            z.weekday().to_monday_zero_offset() as usize,
            z.hour() as usize,
            i64::from(z.minute()) * 60 + i64::from(z.second()),
        )
    };
    activity::summarize(runs, &recs, since, now, &local)
}

#[cfg(test)]
mod tests;
