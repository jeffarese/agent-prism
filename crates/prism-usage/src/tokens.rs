//! Access tokens and where they come from (DESIGN §6.1).
//!
//! This crate does not read the Keychain; the `agp` binary implements
//! [`TokenSource`].

use std::fmt;

use prism_core::model::Tool;

/// An OAuth access token. `Debug` never prints the secret.
#[derive(Clone)]
pub struct AccessToken {
    secret: String,
    /// Expiry in unix seconds, if known.
    pub expires_at: Option<i64>,
    /// Which usage endpoint the token is for.
    pub tool: Tool,
    /// The ChatGPT account id (Codex).
    pub account_id: Option<String>,
}

impl AccessToken {
    /// Wraps a Claude secret.
    pub fn new(secret: String, expires_at: Option<i64>) -> Self {
        Self {
            secret,
            expires_at,
            tool: Tool::Claude,
            account_id: None,
        }
    }

    /// Wraps a Codex (ChatGPT) secret and its account id.
    pub fn codex(secret: String, expires_at: Option<i64>, account_id: Option<String>) -> Self {
        Self {
            secret,
            expires_at,
            tool: Tool::Codex,
            account_id,
        }
    }

    /// The raw token. Only for the `Authorization` header.
    pub fn expose(&self) -> &str {
        &self.secret
    }

    /// True when the token has a known expiry at or before `now`.
    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|e| e <= now)
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessToken")
            .field("secret", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("tool", &self.tool)
            .finish()
    }
}

/// Supplies the current access token of a profile.
pub trait TokenSource: Send + Sync {
    /// The token for `profile_id`, or `None` when not logged in.
    fn token(&self, profile_id: &str) -> anyhow::Result<Option<AccessToken>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts() {
        let t = AccessToken::new("sk-ant-secret".into(), Some(1));
        let d = format!("{t:?}");
        assert!(d.contains("<redacted>") && !d.contains("sk-ant"));
        assert!(t.is_expired(1) && !t.is_expired(0));
    }
}
