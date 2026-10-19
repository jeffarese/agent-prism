//! Clients for the usage endpoints (DESIGN §6.2): Claude's OAuth usage and
//! the ChatGPT usage that Codex shows.

use std::time::Duration;

use prism_core::model::{ScopedWindow, UsageSnapshot, Window};
use serde::Deserialize;

/// The usage endpoint.
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The ChatGPT usage endpoint (Codex's own `/status` reads it).
pub const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// Default back-off after a 429 without `Retry-After`.
pub const DEFAULT_BACKOFF_SECS: u64 = 300;

/// The usage body could not be parsed.
#[derive(Debug, thiserror::Error)]
#[error("invalid usage response: {0}")]
pub struct ParseError(String);

/// A failed usage fetch. Never contains the token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// 401: the token is expired or invalid.
    #[error("token rejected (401)")]
    Unauthorized,
    /// 429: back off.
    #[error("rate limited (429)")]
    RateLimited { retry_after_secs: Option<u64> },
    /// Any other non-success status.
    #[error("http status {0}")]
    Http(u16),
    /// Transport failure.
    #[error("network error: {0}")]
    Network(String),
    /// The body could not be parsed.
    #[error("parse error: {0}")]
    Parse(String),
}

#[derive(Deserialize, Default)]
struct RawBody {
    five_hour: Option<RawWindow>,
    seven_day: Option<RawWindow>,
    #[serde(default)]
    limits: Option<Vec<RawLimit>>,
    extra_usage: Option<RawExtra>,
}

#[derive(Deserialize)]
struct RawWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct RawLimit {
    kind: Option<String>,
    percent: Option<f64>,
    resets_at: Option<String>,
    scope: Option<RawScope>,
}

#[derive(Deserialize)]
struct RawScope {
    model: Option<RawModel>,
}

#[derive(Deserialize)]
struct RawModel {
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct RawExtra {
    is_enabled: Option<bool>,
}

fn parse_ts(s: Option<&str>) -> Option<i64> {
    s.and_then(|s| s.parse::<jiff::Timestamp>().ok())
        .map(|t| t.as_second())
}

fn window(w: Option<RawWindow>) -> Option<Window> {
    let w = w?;
    Some(Window {
        utilization: w.utilization.unwrap_or(0.0),
        resets_at: parse_ts(w.resets_at.as_deref()),
        inferred: false,
    })
}

/// Parses a usage response body. Every field is optional; unknown fields are
/// ignored.
pub fn parse_usage(body: &str, now: i64) -> Result<UsageSnapshot, ParseError> {
    let raw: RawBody = serde_json::from_str(body).map_err(|e| ParseError(e.to_string()))?;
    let scoped = raw
        .limits
        .unwrap_or_default()
        .into_iter()
        .filter(|l| l.kind.as_deref() == Some("weekly_scoped"))
        .map(|l| ScopedWindow {
            label: l
                .scope
                .and_then(|s| s.model)
                .and_then(|m| m.display_name)
                .unwrap_or_else(|| "scoped".to_string()),
            kind: l.kind.unwrap_or_default(),
            utilization: l.percent.unwrap_or(0.0),
            resets_at: parse_ts(l.resets_at.as_deref()),
            inferred: false,
        })
        .collect();
    Ok(UsageSnapshot {
        fetched_at: now,
        five_hour: window(raw.five_hour),
        seven_day: window(raw.seven_day),
        scoped,
        extra_usage_enabled: raw.extra_usage.and_then(|e| e.is_enabled).unwrap_or(false),
        limit_reached: false,
    })
}

/// Fetches and parses usage for one access token.
pub fn fetch_usage(
    agent: &ureq::Agent,
    access_token: &str,
    now: i64,
) -> Result<UsageSnapshot, FetchError> {
    let req = agent
        .get(USAGE_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header(
            "User-Agent",
            &format!("agent-prism/{}", env!("CARGO_PKG_VERSION")),
        );
    let body = call(req)?;
    parse_usage(&body, now).map_err(|e| FetchError::Parse(e.0))
}

/// Fetches and parses the ChatGPT usage of a Codex login.
pub fn fetch_codex_usage(
    agent: &ureq::Agent,
    access_token: &str,
    account_id: Option<&str>,
    now: i64,
) -> Result<UsageSnapshot, FetchError> {
    let mut req = agent
        .get(CODEX_USAGE_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header(
            "User-Agent",
            &format!("agent-prism/{}", env!("CARGO_PKG_VERSION")),
        );
    if let Some(a) = account_id {
        req = req.header("ChatGPT-Account-Id", a);
    }
    let body = call(req)?;
    parse_codex_usage(&body, now).map_err(|e| FetchError::Parse(e.0))
}

/// Sends a GET and returns the body of a 2xx reply.
fn call(req: ureq::RequestBuilder<ureq::typestate::WithoutBody>) -> Result<String, FetchError> {
    let resp = req
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .call();
    // ureq errors do not include request headers, so the token cannot leak.
    let mut resp = resp.map_err(|e| FetchError::Network(e.to_string()))?;
    match resp.status().as_u16() {
        200..=299 => {}
        401 => return Err(FetchError::Unauthorized),
        429 => {
            let retry_after_secs = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok());
            return Err(FetchError::RateLimited { retry_after_secs });
        }
        s => return Err(FetchError::Http(s)),
    }
    resp.body_mut()
        .read_to_string()
        .map_err(|e| FetchError::Network(e.to_string()))
}

#[derive(Deserialize, Default)]
struct CodexBody {
    rate_limit: Option<CodexLimit>,
}

#[derive(Deserialize)]
struct CodexLimit {
    allowed: Option<bool>,
    limit_reached: Option<bool>,
    primary_window: Option<CodexWindow>,
    secondary_window: Option<CodexWindow>,
}

#[derive(Deserialize)]
struct CodexWindow {
    used_percent: Option<f64>,
    limit_window_seconds: Option<i64>,
    reset_after_seconds: Option<i64>,
    reset_at: Option<i64>,
}

/// Windows up to this long count as the short ("5h") window; longer ones
/// as the weekly window.
const SHORT_WINDOW_MAX_SECS: i64 = 24 * 3600;

/// Parses a ChatGPT usage body. `primary_window`/`secondary_window` are
/// placed by their length: the short one is `five_hour`, the long one
/// `seven_day` (a plan can have only one of them).
pub fn parse_codex_usage(body: &str, now: i64) -> Result<UsageSnapshot, ParseError> {
    let raw: CodexBody = serde_json::from_str(body).map_err(|e| ParseError(e.to_string()))?;
    let mut snap = UsageSnapshot {
        fetched_at: now,
        five_hour: None,
        seven_day: None,
        scoped: Vec::new(),
        extra_usage_enabled: false,
        limit_reached: false,
    };
    let Some(rl) = raw.rate_limit else {
        return Ok(snap);
    };
    snap.limit_reached = rl.limit_reached == Some(true) || rl.allowed == Some(false);
    for w in [rl.primary_window, rl.secondary_window]
        .into_iter()
        .flatten()
    {
        let window = Window {
            utilization: w.used_percent.unwrap_or(0.0),
            resets_at: w
                .reset_at
                .or_else(|| w.reset_after_seconds.map(|s| now + s)),
            inferred: false,
        };
        let short = w
            .limit_window_seconds
            .is_some_and(|s| s <= SHORT_WINDOW_MAX_SECS);
        let slot = if short {
            &mut snap.five_hour
        } else {
            &mut snap.seven_day
        };
        if slot.is_none() {
            *slot = Some(window);
        }
    }
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "five_hour": {"utilization": 42.5, "resets_at": "2026-10-15T09:00:00.123456+00:00"},
      "seven_day": {"utilization": 71, "resets_at": "2026-10-20T11:30:00.5+02:00"},
      "seven_day_opus": {"utilization": 3.0, "resets_at": null},
      "seven_day_oauth_apps": null,
      "extra_usage": {"is_enabled": true, "monthly_limit": null},
      "limits": [
        {"kind": "weekly_scoped", "percent": 12.25, "resets_at": "2026-10-19T00:00:00.000001+00:00",
         "scope": {"model": {"display_name": "Fable", "id": "x"}}},
        {"kind": "weekly_scoped", "percent": 5, "resets_at": null, "scope": null},
        {"kind": "something_else", "percent": 99}
      ],
      "unknown_field": [1, 2, 3]
    }"#;

    #[test]
    fn parses_realistic_body() {
        let s = parse_usage(FIXTURE, 100).unwrap();
        assert_eq!(s.fetched_at, 100);
        let f = s.five_hour.unwrap();
        assert_eq!(f.utilization, 42.5);
        assert_eq!(f.resets_at, Some(1_792_054_800));
        let w = s.seven_day.unwrap();
        assert_eq!(w.utilization, 71.0);
        assert_eq!(w.resets_at, Some(1_792_488_600));
        assert!(s.extra_usage_enabled);
        assert_eq!(s.scoped.len(), 2);
        assert_eq!(s.scoped[0].label, "Fable");
        assert_eq!(s.scoped[0].kind, "weekly_scoped");
        assert_eq!(s.scoped[0].utilization, 12.25);
        assert_eq!(s.scoped[0].resets_at, Some(1_792_368_000));
        assert_eq!(s.scoped[1].label, "scoped");
        assert_eq!(s.scoped[1].resets_at, None);
    }

    #[test]
    fn parses_empty_object() {
        let s = parse_usage("{}", 5).unwrap();
        assert!(s.five_hour.is_none() && s.seven_day.is_none());
        assert!(s.scoped.is_empty() && !s.extra_usage_enabled);
    }

    #[test]
    fn parses_codex_body() {
        let body = r#"{
          "plan_type": "pro",
          "rate_limit": {
            "allowed": true, "limit_reached": false,
            "primary_window": {"used_percent": 22, "limit_window_seconds": 604800,
                               "reset_after_seconds": 555111, "reset_at": 1792874437},
            "secondary_window": null
          },
          "credits": {"has_credits": true, "balance": "1.0"}
        }"#;
        let s = parse_codex_usage(body, 7).unwrap();
        assert!(s.five_hour.is_none());
        let w = s.seven_day.unwrap();
        assert_eq!(w.utilization, 22.0);
        assert_eq!(w.resets_at, Some(1_792_874_437));
        assert!(!s.limit_reached);
        let both = r#"{"rate_limit":{"limit_reached":true,
          "primary_window":{"used_percent":100,"limit_window_seconds":18000,"reset_after_seconds":60},
          "secondary_window":{"used_percent":40.5,"limit_window_seconds":604800}}}"#;
        let s = parse_codex_usage(both, 1000).unwrap();
        assert_eq!(s.five_hour.as_ref().unwrap().utilization, 100.0);
        assert_eq!(s.five_hour.unwrap().resets_at, Some(1060));
        assert_eq!(s.seven_day.unwrap().utilization, 40.5);
        assert!(s.limit_reached);
        let none = parse_codex_usage("{}", 1).unwrap();
        assert!(none.five_hour.is_none() && none.seven_day.is_none());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_usage("not json", 0).is_err());
    }

    #[test]
    #[ignore = "network; needs AGP_TEST_TOKEN"]
    fn live_fetch() {
        let token = std::env::var("AGP_TEST_TOKEN").unwrap();
        let agent = ureq::Agent::new_with_defaults();
        fetch_usage(&agent, &token, 0).unwrap();
    }
}
