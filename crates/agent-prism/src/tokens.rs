//! Access tokens from the Keychain (Claude) and `auth.json` (Codex) for the
//! usage poller, and a file-backed secret store for automated tests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use prism_core::keychain::{self, Credentials, SecretStore};
use prism_core::profile::Profile;
use prism_usage::tokens::{AccessToken, TokenSource};

/// How long credentials read from the store are reused.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// Test hook (`AGP_SECRET_STORE_FILE`): a JSON map `service → value` used
/// instead of `/usr/bin/security`. Never used in normal runs.
pub struct FileStore {
    file: PathBuf,
}

impl FileStore {
    /// A store backed by `file`.
    pub fn new(file: PathBuf) -> Self {
        Self { file }
    }
}

impl SecretStore for FileStore {
    fn read(&self, service: &str) -> Result<Option<String>> {
        let text = match std::fs::read_to_string(&self.file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("reading the secret store file"),
        };
        let map: HashMap<String, serde_json::Value> =
            serde_json::from_str(&text).context("secret store file is not a JSON map")?;
        Ok(map.get(service).map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }))
    }
}

/// [`TokenSource`] over the Keychain, caching credentials per profile.
pub struct KeychainTokens {
    store: std::sync::Arc<dyn SecretStore>,
    profiles: Mutex<Vec<Profile>>,
    cache: Mutex<HashMap<String, (Instant, Option<Credentials>)>>,
}

impl KeychainTokens {
    /// Tokens for `profiles`, read through `store`.
    pub fn new(store: std::sync::Arc<dyn SecretStore>, profiles: Vec<Profile>) -> Self {
        Self {
            store,
            profiles: Mutex::new(profiles),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Replaces the known profiles (after a config reload).
    pub fn set_profiles(&self, profiles: Vec<Profile>) {
        *self.profiles.lock().unwrap_or_else(|e| e.into_inner()) = profiles;
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Reads the credentials of every profile that is not cached, in
    /// parallel. Each read starts a `security` process (about 25 ms), so
    /// reading five accounts one by one is slow.
    pub fn prefetch(&self) {
        let missing: Vec<String> = {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            self.profiles
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|p| {
                    cache
                        .get(&p.id)
                        .is_none_or(|(at, _)| at.elapsed() >= CACHE_TTL)
                })
                .map(|p| p.id.clone())
                .collect()
        };
        if missing.len() < 2 {
            return;
        }
        std::thread::scope(|s| {
            for id in &missing {
                // Errors are not cached; the caller reads again and sees them.
                s.spawn(move || self.credentials(id));
            }
        });
    }

    /// The credentials of `profile_id`, cached for 60 s.
    pub fn credentials(&self, profile_id: &str) -> Result<Option<Credentials>> {
        if let Some((at, c)) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(profile_id)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(c.clone());
        }
        let profile = self
            .profiles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|p| p.id == profile_id)
            .cloned();
        let Some(profile) = profile else {
            return Ok(None);
        };
        let creds = keychain::read_credentials(self.store.as_ref(), &profile)?;
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(profile_id.to_string(), (Instant::now(), creds.clone()));
        Ok(creds)
    }
}

/// The poller's token for `c`: a Codex token carries its account id.
pub fn access_token(c: &Credentials) -> AccessToken {
    let secret = c.access_token.expose().to_string();
    match &c.account_id {
        Some(a) => AccessToken::codex(secret, c.expires_at, Some(a.clone())),
        None => AccessToken::new(secret, c.expires_at),
    }
}

impl TokenSource for KeychainTokens {
    fn token(&self, profile_id: &str) -> Result<Option<AccessToken>> {
        Ok(self.credentials(profile_id)?.as_ref().map(access_token))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Counts reads; every item is missing.
    #[derive(Default)]
    struct Counting(AtomicUsize);

    impl SecretStore for Counting {
        fn read(&self, _service: &str) -> Result<Option<String>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
    }

    fn prof(id: &str) -> Profile {
        Profile::bare(
            id,
            prism_core::model::Tool::Claude,
            format!("/h/.claude-{id}").into(),
            "/h/.claude".into(),
        )
    }

    #[test]
    fn prefetch_fills_the_cache() {
        let store = Arc::new(Counting::default());
        let ids = ["a", "b", "c"];
        let t = KeychainTokens::new(store.clone(), ids.iter().map(|i| prof(i)).collect());
        t.prefetch();
        let after_prefetch = store.0.load(Ordering::SeqCst);
        assert!(after_prefetch >= ids.len());
        for id in ids {
            assert!(t.credentials(id).unwrap().is_none());
        }
        // Everything came from the cache; a second prefetch reads nothing.
        t.prefetch();
        assert_eq!(store.0.load(Ordering::SeqCst), after_prefetch);
    }
}
