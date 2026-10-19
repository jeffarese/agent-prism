//! The Codex login in `<CODEX_HOME>/auth.json` (DESIGN §12).
//!
//! Secrets rule: tokens are never printed, logged or put in an error. Only
//! the claims agent-prism needs (email, name, plan, account id, expiry) are read
//! from the token payloads.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::keychain::{Credentials, Secret};
use crate::model::{AccountIdentity, Tool};

#[derive(Deserialize, Default)]
struct AuthFile {
    #[serde(rename = "OPENAI_API_KEY")]
    api_key: Option<String>,
    tokens: Option<Tokens>,
}

#[derive(Deserialize)]
struct Tokens {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

/// What `auth.json` holds.
#[derive(Clone, Debug, PartialEq)]
pub enum CodexLogin {
    /// No file, or a file without a login.
    None,
    /// An API key (usage-based billing): not a subscription, never offered.
    ApiKey,
    /// A ChatGPT login.
    ChatGpt(Box<Credentials>, AccountIdentity),
}

/// Decode base64url without padding (the JWT alphabet).
fn b64url(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The payload of a JWT, without checking the signature (the token is only
/// read for display and expiry; the service checks it).
pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&b64url(payload)?).ok()
}

fn auth_claim<'a>(claims: &'a Value, key: &str) -> Option<&'a str> {
    claims
        .get("https://api.openai.com/auth")?
        .get(key)?
        .as_str()
}

fn identity_from_claims(id: Option<&Value>, access: Option<&Value>) -> AccountIdentity {
    let str_of = |v: Option<&Value>, k: &str| {
        v.and_then(|v| v.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let plan = id
        .and_then(|c| auth_claim(c, "chatgpt_plan_type"))
        .or_else(|| access.and_then(|c| auth_claim(c, "chatgpt_plan_type")))
        .map(str::to_string);
    // A workspace name, when the default organization is not "Personal".
    let org = id
        .and_then(|c| c.get("https://api.openai.com/auth")?.get("organizations"))
        .and_then(Value::as_array)
        .and_then(|orgs| {
            orgs.iter()
                .find(|o| o.get("is_default").and_then(Value::as_bool) == Some(true))
                .or_else(|| orgs.first())
        })
        .and_then(|o| o.get("title").and_then(Value::as_str))
        .filter(|t| !t.is_empty() && *t != "Personal")
        .map(str::to_string);
    AccountIdentity {
        display_name: str_of(id, "name"),
        email: str_of(id, "email").or_else(|| {
            access
                .and_then(|c| c.get("https://api.openai.com/profile"))
                .and_then(|p| p.get("email"))
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
        org,
        subscription: plan,
        rate_limit_tier: None,
        tool: Tool::Codex,
    }
}

/// Parse an `auth.json` text. Errors never contain the input.
pub fn parse_auth(text: &str) -> Result<CodexLogin> {
    let file: AuthFile = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("auth.json is not valid JSON (line {})", e.line()))?;
    let Some(t) = file.tokens else {
        return Ok(if file.api_key.is_some_and(|k| !k.is_empty()) {
            CodexLogin::ApiKey
        } else {
            CodexLogin::None
        });
    };
    let Some(access) = t.access_token.filter(|a| !a.is_empty()) else {
        return Ok(CodexLogin::None);
    };
    let access_claims = jwt_claims(&access);
    let id_claims = t.id_token.as_deref().and_then(jwt_claims);
    let identity = identity_from_claims(id_claims.as_ref(), access_claims.as_ref());
    let account_id = t.account_id.filter(|a| !a.is_empty()).or_else(|| {
        id_claims
            .as_ref()
            .and_then(|c| auth_claim(c, "chatgpt_account_id"))
            .map(str::to_string)
    });
    let creds = Credentials {
        access_token: Secret::new(access),
        refresh_token: t.refresh_token.filter(|r| !r.is_empty()).map(Secret::new),
        expires_at: access_claims
            .as_ref()
            .and_then(|c| c.get("exp"))
            .and_then(Value::as_i64),
        scopes: Vec::new(),
        subscription_type: identity.subscription.clone(),
        rate_limit_tier: None,
        account_id,
    };
    Ok(CodexLogin::ChatGpt(Box::new(creds), identity))
}

/// Read `<dir>/auth.json`. A missing file is [`CodexLogin::None`].
pub fn read_login(dir: &Path) -> Result<CodexLogin> {
    let file = dir.join("auth.json");
    match std::fs::read_to_string(&file) {
        Ok(t) => parse_auth(&t).with_context(|| format!("in {}", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CodexLogin::None),
        Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn b64(data: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let n =
                chunk.iter().fold(0u32, |a, &b| (a << 8) | u32::from(b)) << (8 * (3 - chunk.len()));
            for i in 0..=chunk.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        out
    }

    /// A fake JWT with `claims` as payload.
    pub fn jwt(claims: &Value) -> String {
        format!(
            "{}.{}.SIGSECRET",
            b64(br#"{"alg":"none"}"#),
            b64(claims.to_string().as_bytes())
        )
    }

    /// An `auth.json` for a ChatGPT login.
    pub fn sample_auth(email: &str, plan: &str, account: &str, exp: i64) -> String {
        let id = jwt(&serde_json::json!({
            "email": email,
            "name": "Neo",
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": plan,
                "chatgpt_account_id": account,
                "organizations": [{"title": "Personal", "is_default": true}],
            },
        }));
        let access = jwt(&serde_json::json!({"exp": exp, "SECRET": "x"}));
        serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": id,
                "access_token": access,
                "refresh_token": "rt-SECRET",
                "account_id": account,
            },
            "last_refresh": "2026-10-10T16:49:36.455Z",
        })
        .to_string()
    }

    #[test]
    fn base64url_round_trip() {
        for s in ["", "a", "ab", "abc", "abcd", "{\"x\":\"é\"}"] {
            assert_eq!(b64url(&b64(s.as_bytes())).unwrap(), s.as_bytes(), "{s}");
        }
        assert!(b64url("a*b").is_none());
    }

    #[test]
    fn parses_chatgpt_login() {
        let text = sample_auth("neo@example.com", "pro", "acc-1", 1_800_000_000);
        let CodexLogin::ChatGpt(c, id) = parse_auth(&text).unwrap() else {
            panic!("not a login");
        };
        assert_eq!(id.email.as_deref(), Some("neo@example.com"));
        assert_eq!(id.display_name.as_deref(), Some("Neo"));
        assert_eq!(id.subscription.as_deref(), Some("pro"));
        assert_eq!(id.org, None, "Personal is not shown");
        assert_eq!(id.tool, Tool::Codex);
        assert_eq!(c.expires_at, Some(1_800_000_000));
        assert_eq!(c.account_id.as_deref(), Some("acc-1"));
        assert!(!format!("{c:?}").contains("SECRET"));
    }

    #[test]
    fn api_key_and_errors() {
        let k = parse_auth(r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-SECRET"}"#).unwrap();
        assert_eq!(k, CodexLogin::ApiKey);
        assert_eq!(parse_auth("{}").unwrap(), CodexLogin::None);
        let e = parse_auth("SECRET not json").unwrap_err();
        assert!(!format!("{e:#}").contains("SECRET"));
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_login(tmp.path()).unwrap(), CodexLogin::None);
    }
}
