//! Claude's OAuth credentials (DESIGN §6.1): from the macOS Keychain, or on
//! Linux and WSL from `<config dir>/.credentials.json`, where Claude Code
//! keeps them.
//!
//! Secrets rule: tokens are never printed, logged or put in an error.

use std::fmt;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::model::{TokenState, Tool};
use crate::profile::Profile;

/// A secret string whose `Debug`/`Display` print `<redacted>`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret value.
    pub fn new(s: impl Into<String>) -> Secret {
        Secret(s.into())
    }

    /// The raw value. Only for building a request header.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Parsed `claudeAiOauth` credentials.
#[derive(Clone, Debug, PartialEq)]
pub struct Credentials {
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
    /// Unix seconds (the JSON has milliseconds).
    pub expires_at: Option<i64>,
    pub scopes: Vec<String>,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    /// The ChatGPT account id (Codex only; the usage call needs it).
    pub account_id: Option<String>,
}

/// Something that can read a generic password by service name.
pub trait SecretStore: Send + Sync {
    /// The stored value, or `None` if there is no such item.
    fn read(&self, service: &str) -> Result<Option<String>>;
}

/// Reads the Keychain via `/usr/bin/security`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SecurityCli;

impl SecretStore for SecurityCli {
    fn read(&self, service: &str) -> Result<Option<String>> {
        let out = Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", service, "-w"])
            .output()
            .context("running /usr/bin/security")?;
        match out.status.code() {
            Some(0) => {
                let s = String::from_utf8(out.stdout)
                    .map_err(|_| anyhow::anyhow!("Keychain item {service} is not UTF-8"))?;
                Ok(Some(s.trim_end_matches(['\n', '\r']).to_string()))
            }
            Some(44) => Ok(None),
            code => bail!("security failed for {service} (exit {code:?})"),
        }
    }
}

/// The store of a system without a Keychain (Linux, WSL): it holds nothing,
/// so [`read_credentials`] reads `<dir>/.credentials.json`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoKeychain;

impl SecretStore for NoKeychain {
    fn read(&self, _service: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

/// The secret store of this system: the Keychain on macOS, none elsewhere.
pub fn platform_store() -> std::sync::Arc<dyn SecretStore> {
    if crate::platform::has_keychain() {
        std::sync::Arc::new(SecurityCli)
    } else {
        std::sync::Arc::new(NoKeychain)
    }
}

/// Where a Claude login lives, as `(label, place)`: `("keychain", <item>)`
/// on macOS, else `("login", <dir>/.credentials.json)`.
pub fn login_location(profile: &Profile) -> (&'static str, String) {
    if crate::platform::has_keychain() {
        ("keychain", profile.keychain_service())
    } else {
        (
            "login",
            profile.dir.join(".credentials.json").display().to_string(),
        )
    }
}

#[derive(Deserialize)]
struct Blob {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OauthJson>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OauthJson {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
    #[serde(default)]
    scopes: Vec<String>,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
}

/// True for what Claude leaves when the server refuses a token refresh: a
/// `claudeAiOauth` with no access token and no refresh token. The account
/// needs `/login`.
fn logged_out(json: &str) -> bool {
    let Ok(Blob { oauth: Some(o) }) = serde_json::from_str::<Blob>(json) else {
        return false;
    };
    o.access_token.is_none_or(|t| t.is_empty()) && o.refresh_token.is_none_or(|t| t.is_empty())
}

/// Parse the credentials JSON. Errors never contain the input.
pub fn parse_credentials(json: &str) -> Result<Credentials> {
    let blob: Blob = serde_json::from_str(json)
        .map_err(|e| anyhow::anyhow!("credentials are not valid JSON (line {})", e.line()))?;
    let o = blob.oauth.context("credentials have no claudeAiOauth")?;
    let access = o
        .access_token
        .filter(|t| !t.is_empty())
        .context("credentials have no accessToken")?;
    Ok(Credentials {
        access_token: Secret(access),
        refresh_token: o.refresh_token.filter(|t| !t.is_empty()).map(Secret),
        expires_at: o.expires_at.map(|ms| ms / 1000),
        scopes: o.scopes,
        subscription_type: o.subscription_type,
        rate_limit_tier: o.rate_limit_tier,
        account_id: None,
    })
}

/// Read a profile's credentials. Claude: Keychain first, then
/// `<dir>/.credentials.json`. Codex: `<dir>/auth.json` (an API-key login
/// gives `None`).
pub fn read_credentials(store: &dyn SecretStore, profile: &Profile) -> Result<Option<Credentials>> {
    if profile.tool == Tool::Codex {
        return Ok(match crate::codex::read_login(&profile.dir)? {
            crate::codex::CodexLogin::ChatGpt(c, _) => Some(*c),
            _ => None,
        });
    }
    if let Some(json) = store.read(&profile.keychain_service())? {
        if logged_out(&json) {
            return Ok(None);
        }
        return parse_credentials(&json).map(Some);
    }
    let file = profile.dir.join(".credentials.json");
    match std::fs::read_to_string(&file) {
        Ok(json) if logged_out(&json) => Ok(None),
        Ok(json) => parse_credentials(&json)
            .map(Some)
            .with_context(|| format!("in {}", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
    }
}

/// The token state at `now`. Credentials without an expiry count as valid
/// with `expires_at = i64::MAX`.
pub fn token_state(creds: Option<&Credentials>, now: i64) -> TokenState {
    match creds {
        None => TokenState::Missing,
        Some(c) => match c.expires_at {
            Some(t) if t <= now => TokenState::Expired { since: t },
            Some(t) => TokenState::Valid { expires_at: t },
            None => TokenState::Valid {
                expires_at: i64::MAX,
            },
        },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;

    /// In-memory store for tests.
    #[derive(Default)]
    pub struct FakeStore(pub HashMap<String, String>);

    impl SecretStore for FakeStore {
        fn read(&self, service: &str) -> Result<Option<String>> {
            Ok(self.0.get(service).cloned())
        }
    }

    pub const SAMPLE: &str = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-SECRET","refreshToken":"sk-ant-ort-SECRET","expiresAt":1760500000123,"scopes":["user:inference"],"subscriptionType":"team","rateLimitTier":"default_claude_max_5x"}}"#;

    #[test]
    fn parse_full() {
        let c = parse_credentials(SAMPLE).unwrap();
        assert_eq!(c.access_token.expose(), "sk-ant-oat-SECRET");
        assert_eq!(c.expires_at, Some(1_760_500_000));
        assert_eq!(c.scopes, vec!["user:inference"]);
        assert_eq!(c.subscription_type.as_deref(), Some("team"));
        assert_eq!(c.rate_limit_tier.as_deref(), Some("default_claude_max_5x"));
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("SECRET"), "{dbg}");
        assert_eq!(format!("{}", c.access_token), "<redacted>");
    }

    #[test]
    fn parse_minimal_and_errors() {
        let c = parse_credentials(r#"{"claudeAiOauth":{"accessToken":"a"}}"#).unwrap();
        assert!(c.refresh_token.is_none() && c.expires_at.is_none() && c.scopes.is_empty());
        let e = parse_credentials(r#"{"claudeAiOauth":{"refreshToken":"SECRET"}}"#).unwrap_err();
        assert!(!format!("{e:#}").contains("SECRET"));
        let e = parse_credentials("SECRET not json").unwrap_err();
        assert!(!format!("{e:#}").contains("SECRET"));
    }

    #[test]
    fn states() {
        let c = parse_credentials(SAMPLE).unwrap();
        assert_eq!(token_state(None, 0), TokenState::Missing);
        assert_eq!(
            token_state(Some(&c), 1_760_000_000),
            TokenState::Valid {
                expires_at: 1_760_500_000
            }
        );
        assert_eq!(
            token_state(Some(&c), 1_760_600_000),
            TokenState::Expired {
                since: 1_760_500_000
            }
        );
    }

    #[test]
    fn without_a_keychain_the_file_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".claude-a");
        std::fs::create_dir_all(&dir).unwrap();
        let p = Profile::bare("a", Tool::Claude, dir.clone(), tmp.path().join(".claude"));
        assert!(read_credentials(&NoKeychain, &p).unwrap().is_none());
        std::fs::write(dir.join(".credentials.json"), SAMPLE).unwrap();
        let c = read_credentials(&NoKeychain, &p).unwrap().unwrap();
        assert_eq!(c.subscription_type.as_deref(), Some("team"));
        std::fs::write(dir.join(".credentials.json"), "SECRET garbage").unwrap();
        let e = read_credentials(&NoKeychain, &p).unwrap_err();
        assert!(!format!("{e:#}").contains("SECRET"), "{e:#}");
        // What Claude leaves after a refused refresh: no login, not an error.
        std::fs::write(
            dir.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"","refreshToken":"","expiresAt":0}}"#,
        )
        .unwrap();
        assert!(read_credentials(&NoKeychain, &p).unwrap().is_none());
        let at = login_location(&p);
        if crate::platform::has_keychain() {
            assert_eq!(at, ("keychain", p.keychain_service()));
        } else {
            let file = dir.join(".credentials.json").display().to_string();
            assert_eq!(at, ("login", file));
        }
    }

    #[test]
    fn refused_refresh_is_no_login() {
        let p = Profile::bare(
            "a",
            Tool::Claude,
            "/h/.claude-a".into(),
            "/h/.claude".into(),
        );
        let mut store = FakeStore::default();
        // What Claude writes when the server refuses a refresh.
        store.0.insert(
            p.keychain_service(),
            r#"{"claudeAiOauth":{"accessToken":"","refreshToken":"","expiresAt":0}}"#.into(),
        );
        assert!(read_credentials(&store, &p).unwrap().is_none());
        // An empty access token with a refresh token is only unreadable.
        store.0.insert(
            p.keychain_service(),
            r#"{"claudeAiOauth":{"accessToken":"","refreshToken":"r"}}"#.into(),
        );
        assert!(read_credentials(&store, &p).is_err());
    }
}
