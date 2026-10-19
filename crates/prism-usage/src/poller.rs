//! Background usage polling (DESIGN §6.4).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prism_core::model::UsageSnapshot;

use crate::api::{self, DEFAULT_BACKOFF_SECS, FetchError};
use crate::cache::{CachedUsage, Sample, UsageCache};
use crate::tokens::{AccessToken, TokenSource};

/// How often the loop wakes without a command.
pub const DEFAULT_TICK: Duration = Duration::from_secs(5);

/// A profile to poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollTarget {
    pub profile_id: String,
    /// Polled at `poll_active_secs` instead of `poll_idle_secs`.
    pub active: bool,
}

/// Poller settings.
#[derive(Clone, Debug)]
pub struct PollerConfig {
    pub cache_dir: PathBuf,
    pub poll_active_secs: u64,
    pub poll_idle_secs: u64,
}

/// A new cached value for one profile.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageUpdate {
    pub profile_id: String,
    pub cached: CachedUsage,
}

/// Fetches a usage snapshot; injectable for tests.
pub trait UsageFetcher: Send + Sync {
    /// Fetches usage with `token` at time `now`.
    fn fetch(&self, token: &AccessToken, now: i64) -> Result<UsageSnapshot, FetchError>;
}

/// The real fetcher, using `ureq`.
pub struct UreqFetcher {
    agent: ureq::Agent,
}

impl Default for UreqFetcher {
    fn default() -> Self {
        Self {
            agent: ureq::Agent::new_with_defaults(),
        }
    }
}

impl UsageFetcher for UreqFetcher {
    fn fetch(&self, token: &AccessToken, now: i64) -> Result<UsageSnapshot, FetchError> {
        match token.tool {
            prism_core::model::Tool::Claude => api::fetch_usage(&self.agent, token.expose(), now),
            prism_core::model::Tool::Codex => api::fetch_codex_usage(
                &self.agent,
                token.expose(),
                token.account_id.as_deref(),
                now,
            ),
        }
    }
}

enum Command {
    Refresh(Option<String>),
    SetTargets(Vec<PollTarget>),
    Stop,
}

/// Spawns the polling thread.
pub struct Poller;

impl Poller {
    /// Starts polling with the real HTTP fetcher.
    pub fn spawn(
        cfg: PollerConfig,
        targets: Vec<PollTarget>,
        tokens: Arc<dyn TokenSource>,
        on_update: Box<dyn Fn(UsageUpdate) + Send>,
    ) -> PollerHandle {
        Self::spawn_with(
            cfg,
            targets,
            tokens,
            on_update,
            Arc::new(UreqFetcher::default()),
            DEFAULT_TICK,
        )
    }

    /// Starts polling with a custom fetcher and wake interval.
    pub fn spawn_with(
        cfg: PollerConfig,
        targets: Vec<PollTarget>,
        tokens: Arc<dyn TokenSource>,
        on_update: Box<dyn Fn(UsageUpdate) + Send>,
        fetcher: Arc<dyn UsageFetcher>,
        tick: Duration,
    ) -> PollerHandle {
        let (tx, rx) = mpsc::channel();
        let mut state = Loop {
            cache: UsageCache::new(cfg.cache_dir.clone()),
            cfg,
            targets,
            tokens,
            fetcher,
            on_update,
            seen: HashMap::new(),
            forced: HashSet::new(),
        };
        let thread = std::thread::Builder::new()
            .name("prism-usage-poller".into())
            .spawn(move || state.run(rx, tick))
            .expect("spawn usage poller");
        PollerHandle {
            tx,
            thread: Some(thread),
        }
    }
}

/// Controls a running poller. Dropping it stops the thread too.
pub struct PollerHandle {
    tx: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl PollerHandle {
    /// Fetches one profile (or all with `None`) now, ignoring the cache age.
    pub fn refresh_now(&self, profile_id: Option<String>) {
        let _ = self.tx.send(Command::Refresh(profile_id));
    }

    /// Replaces the polled profiles.
    pub fn set_targets(&self, t: Vec<PollTarget>) {
        let _ = self.tx.send(Command::SetTargets(t));
    }

    /// Stops the thread and waits for it.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PollerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

struct Loop {
    cfg: PollerConfig,
    cache: UsageCache,
    targets: Vec<PollTarget>,
    tokens: Arc<dyn TokenSource>,
    fetcher: Arc<dyn UsageFetcher>,
    on_update: Box<dyn Fn(UsageUpdate) + Send>,
    /// Cache mtime last emitted per profile.
    seen: HashMap<String, Option<SystemTime>>,
    forced: HashSet<String>,
}

impl Loop {
    fn run(&mut self, rx: Receiver<Command>, tick: Duration) {
        for t in self.targets.clone() {
            self.emit_cached(&t.profile_id);
        }
        loop {
            self.poll_all();
            match rx.recv_timeout(tick) {
                Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(Command::Refresh(Some(id))) => {
                    self.forced.insert(id);
                }
                Ok(Command::Refresh(None)) => {
                    self.forced
                        .extend(self.targets.iter().map(|t| t.profile_id.clone()));
                }
                Ok(Command::SetTargets(t)) => {
                    for n in &t {
                        if !self.seen.contains_key(&n.profile_id) {
                            self.emit_cached(&n.profile_id);
                        }
                    }
                    self.targets = t;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn emit_cached(&mut self, id: &str) {
        self.seen.insert(id.to_string(), self.cache.modified(id));
        let cached = self.cache.load(id).unwrap_or_default();
        (self.on_update)(UsageUpdate {
            profile_id: id.to_string(),
            cached,
        });
    }

    fn poll_all(&mut self) {
        for t in self.targets.clone() {
            if let Err(e) = self.poll_one(&t) {
                log::warn!("usage poll {}: {e:#}", t.profile_id);
            }
        }
    }

    fn poll_one(&mut self, t: &PollTarget) -> anyhow::Result<()> {
        let id = t.profile_id.as_str();
        let now = unix_now();
        let interval = if t.active {
            self.cfg.poll_active_secs
        } else {
            self.cfg.poll_idle_secs
        } as i64;
        let forced = self.forced.contains(id);
        let old = self.cache.load(id);
        let backoff = old
            .as_ref()
            .and_then(|c| c.backoff_until)
            .is_some_and(|b| b > now);
        let young = old
            .as_ref()
            .is_some_and(|c| now - c.last_attempt < interval);
        if backoff || (young && !forced) {
            if self.seen.get(id) != Some(&self.cache.modified(id)) {
                self.emit_cached(id);
            }
            return Ok(());
        }
        let Some(_lock) = self.cache.try_lock(id)? else {
            return Ok(());
        };
        self.forced.remove(id);
        let mut next = old.unwrap_or_default();
        next.last_attempt = now;
        match self.tokens.token(id) {
            Err(_) => next.last_error = Some("token unreadable".into()),
            Ok(None) => next.last_error = Some("not logged in".into()),
            Ok(Some(tok)) => {
                next.token_expires_at = tok.expires_at;
                if tok.is_expired(now) {
                    next.last_error = Some("token expired".into());
                } else {
                    self.fetch_into(id, &tok, now, &mut next)?;
                }
            }
        }
        self.cache.store(id, &next)?;
        self.seen.insert(id.to_string(), self.cache.modified(id));
        (self.on_update)(UsageUpdate {
            profile_id: id.to_string(),
            cached: next,
        });
        Ok(())
    }

    fn fetch_into(
        &self,
        id: &str,
        tok: &AccessToken,
        now: i64,
        next: &mut CachedUsage,
    ) -> anyhow::Result<()> {
        match self.fetcher.fetch(tok, now) {
            Ok(snap) => {
                self.cache.append_sample(
                    id,
                    &Sample {
                        ts: now,
                        u5h: snap.five_hour.as_ref().map(|w| w.utilization),
                        u7d: snap.seven_day.as_ref().map(|w| w.utilization),
                    },
                )?;
                next.snapshot = Some(snap);
                next.last_error = None;
                next.backoff_until = None;
            }
            Err(FetchError::Unauthorized) => next.last_error = Some("token rejected".into()),
            Err(FetchError::RateLimited { retry_after_secs }) => {
                let secs = retry_after_secs.unwrap_or(DEFAULT_BACKOFF_SECS) as i64;
                next.backoff_until = Some(now + secs);
                next.last_error = Some("rate limited".into());
            }
            Err(e) => next.last_error = Some(e.to_string()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_core::model::Window;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeTokens(Option<i64>);
    impl TokenSource for FakeTokens {
        fn token(&self, _: &str) -> anyhow::Result<Option<AccessToken>> {
            Ok(Some(AccessToken::new("t".into(), self.0)))
        }
    }

    struct FakeFetcher {
        calls: AtomicUsize,
        result: Mutex<Result<f64, FetchError>>,
    }
    impl UsageFetcher for FakeFetcher {
        fn fetch(&self, _: &AccessToken, now: i64) -> Result<UsageSnapshot, FetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let u = self.result.lock().unwrap().clone()?;
            Ok(UsageSnapshot {
                fetched_at: now,
                five_hour: Some(Window {
                    utilization: u,
                    resets_at: None,
                    inferred: false,
                }),
                seven_day: None,
                scoped: vec![],
                extra_usage_enabled: false,
                limit_reached: false,
            })
        }
    }

    struct Rig {
        handle: PollerHandle,
        rx: Receiver<UsageUpdate>,
        fetcher: Arc<FakeFetcher>,
        _dir: tempfile::TempDir,
    }

    fn rig(dir: tempfile::TempDir, expires: Option<i64>, result: Result<f64, FetchError>) -> Rig {
        let fetcher = Arc::new(FakeFetcher {
            calls: AtomicUsize::new(0),
            result: Mutex::new(result),
        });
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let handle = Poller::spawn_with(
            PollerConfig {
                cache_dir: dir.path().to_path_buf(),
                poll_active_secs: 3600,
                poll_idle_secs: 3600,
            },
            vec![PollTarget {
                profile_id: "a".into(),
                active: true,
            }],
            Arc::new(FakeTokens(expires)),
            Box::new(move |u| {
                let _ = tx.lock().unwrap().send(u);
            }),
            fetcher.clone(),
            Duration::from_millis(10),
        );
        Rig {
            handle,
            rx,
            fetcher,
            _dir: dir,
        }
    }

    fn next(rx: &Receiver<UsageUpdate>) -> UsageUpdate {
        rx.recv_timeout(Duration::from_secs(5)).expect("update")
    }

    fn quiet(rx: &Receiver<UsageUpdate>) -> bool {
        rx.recv_timeout(Duration::from_millis(100)).is_err()
    }

    #[test]
    fn fetches_then_reuses_cache() {
        let r = rig(tempfile::tempdir().unwrap(), None, Ok(42.0));
        assert_eq!(next(&r.rx).cached, CachedUsage::default());
        let u = next(&r.rx);
        assert_eq!(u.profile_id, "a");
        assert_eq!(
            u.cached.snapshot.unwrap().five_hour.unwrap().utilization,
            42.0
        );
        assert!(quiet(&r.rx));
        assert_eq!(r.fetcher.calls.load(Ordering::SeqCst), 1);
        // Another agent-prism writes the file: we emit without fetching.
        let cache = UsageCache::new(r._dir.path().to_path_buf());
        let mut c = cache.load("a").unwrap();
        c.last_error = Some("other".into());
        std::thread::sleep(Duration::from_millis(20));
        cache.store("a", &c).unwrap();
        assert_eq!(next(&r.rx).cached.last_error.as_deref(), Some("other"));
        // refresh_now ignores the age.
        r.handle.refresh_now(None);
        assert!(next(&r.rx).cached.last_error.is_none());
        assert_eq!(r.fetcher.calls.load(Ordering::SeqCst), 2);
        assert_eq!(cache.samples("a", 0).len(), 2);
        r.handle.stop();
    }

    #[test]
    fn expired_token_keeps_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let cache = UsageCache::new(dir.path().to_path_buf());
        let snap = UsageSnapshot {
            fetched_at: 1,
            five_hour: None,
            seven_day: None,
            scoped: vec![],
            extra_usage_enabled: false,
            limit_reached: false,
        };
        cache
            .store(
                "a",
                &CachedUsage {
                    snapshot: Some(snap.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        let r = rig(dir, Some(1), Ok(1.0));
        assert_eq!(next(&r.rx).cached.snapshot, Some(snap.clone()));
        let u = next(&r.rx).cached;
        assert_eq!(u.last_error.as_deref(), Some("token expired"));
        assert_eq!(u.snapshot, Some(snap));
        assert_eq!(u.token_expires_at, Some(1));
        assert_eq!(r.fetcher.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn rate_limit_backs_off() {
        let r = rig(
            tempfile::tempdir().unwrap(),
            None,
            Err(FetchError::RateLimited {
                retry_after_secs: Some(60),
            }),
        );
        next(&r.rx);
        let u = next(&r.rx).cached;
        let b = u.backoff_until.unwrap();
        assert!((b - u.last_attempt - 60).abs() <= 1);
        r.handle.refresh_now(Some("a".into()));
        assert!(quiet(&r.rx));
        assert_eq!(r.fetcher.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unauthorized_sets_error() {
        let r = rig(
            tempfile::tempdir().unwrap(),
            None,
            Err(FetchError::Unauthorized),
        );
        next(&r.rx);
        assert_eq!(
            next(&r.rx).cached.last_error.as_deref(),
            Some("token rejected")
        );
    }
}
