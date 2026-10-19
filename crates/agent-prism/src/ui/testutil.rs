//! Fixtures for UI tests: a temp home with fake profiles, a fake secret
//! store, sample views and buffer helpers. Never touches the real home or
//! the Keychain.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use prism_core::config::{Config, ProfileConfig};
use prism_core::keychain::SecretStore;
use prism_core::model::{
    AccountIdentity, AccountView, Projection, TokenState, UsageSnapshot, UsageStatus, Window,
};
use prism_core::paths::Paths;
use prism_term::panel::Panel;
use prism_usage::cache::UsageCache;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::actions::UiAction;
use crate::app::AppCtx;
use crate::live::{Live, LiveData, SessionInfo};

/// A secret store backed by a map.
#[derive(Default)]
pub struct FakeStore(pub HashMap<String, String>);

impl SecretStore for FakeStore {
    fn read(&self, service: &str) -> Result<Option<String>> {
        Ok(self.0.get(service).cloned())
    }
}

/// Fake credentials blob with an expired token.
pub const CREDS: &str = r#"{"claudeAiOauth":{"accessToken":"sk-test-SECRET","expiresAt":1000,"subscriptionType":"team","rateLimitTier":"default_claude_max_5x"}}"#;

/// A temp root with `home/.claude`, `home/.claude-a` (logged in),
/// `home/.claude-b` (logged in) and `home/.claude-c` (no login).
pub struct Fixture {
    pub _tmp: tempfile::TempDir,
    pub ctx: Arc<AppCtx>,
}

impl Fixture {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let h = &paths.home;
        for d in [".claude/projects", ".claude-a", ".claude-b", ".claude-c"] {
            std::fs::create_dir_all(h.join(d)).unwrap();
        }
        std::fs::write(h.join(".claude/CLAUDE.md"), "# hi").unwrap();
        for (d, name, email) in [
            (".claude-a", "Ann", "ann@example.com"),
            (".claude-b", "Bob", "bob@example.com"),
        ] {
            std::fs::write(
                h.join(d).join(".claude.json"),
                format!(
                    r#"{{"oauthAccount":{{"displayName":"{name}","emailAddress":"{email}","organizationName":"Acme Team"}}}}"#
                ),
            )
            .unwrap();
            std::fs::write(h.join(d).join(".credentials.json"), CREDS).unwrap();
        }
        let ctx = AppCtx {
            cache: UsageCache::new(paths.usage_dir()),
            paths,
            store: Arc::new(FakeStore::default()),
        };
        Fixture {
            _tmp: tmp,
            ctx: Arc::new(ctx),
        }
    }

    pub fn paths(&self) -> &Paths {
        &self.ctx.paths
    }
}

/// A config with profiles `a` and `b` in the pool.
pub fn config() -> Config {
    Config {
        profiles: vec![
            ProfileConfig {
                id: "a".into(),
                label: "Ann".into(),
                dir: "~/.claude-a".into(),
                ..Default::default()
            },
            ProfileConfig {
                id: "b".into(),
                label: "Bob".into(),
                dir: "~/.claude-b".into(),
                color: Some("magenta".into()),
                ..Default::default()
            },
        ],
        ..Config::default()
    }
}

fn window(u: f64) -> Option<Window> {
    Some(Window {
        utilization: u,
        resets_at: Some(crate::ui::data::now() + 7200),
        inferred: false,
    })
}

/// A view with usage.
pub fn view(id: &str, label: &str, u5: f64, u7: f64) -> AccountView {
    AccountView {
        profile_id: id.into(),
        label: label.into(),
        color: None,
        identity: AccountIdentity {
            display_name: Some(label.into()),
            email: Some(format!("{id}@example.com")),
            org: Some("Acme Team".into()),
            subscription: Some("team".into()),
            rate_limit_tier: None,
            tool: Default::default(),
        },
        token: TokenState::Valid {
            expires_at: i64::MAX,
        },
        usage: Some(UsageSnapshot {
            fetched_at: crate::ui::data::now(),
            five_hour: window(u5),
            seven_day: window(u7),
            scoped: vec![],
            extra_usage_enabled: false,
            limit_reached: false,
        }),
        usage_status: UsageStatus::Fresh,
        projection: Projection::default(),
        in_pool: true,
        active_here: false,
        active_elsewhere: 0,
        limited: None,
        next_pick: false,
        score: None,
        reason: None,
    }
}

/// Live data with two accounts, a session on `a` and a pick of `b`.
pub fn live() -> Live {
    let mut a = view("a", "Ann", 48.0, 31.0);
    a.active_here = true;
    let b = view("b", "Bob", 1.0, 72.0);
    Live::new(LiveData {
        config: config(),
        views: vec![a, b],
        pick: Some(prism_usage::balance::PickResult {
            chosen: Some("b".into()),
            reason: "most 5h headroom".into(),
            earliest_reset: None,
        }),
        session: Some(SessionInfo {
            tool: Default::default(),
            profile_id: "a".into(),
            session_id: Some("3f2a91c0-1".into()),
            busy: false,
            can_switch: true,
            pending: None,
        }),
        log: (0..30).map(|i| format!("log line {i}")).collect(),
    })
}

/// A key press without modifiers.
pub fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// A character key press.
pub fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

/// Ctrl + character.
pub fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// All text in a buffer, one line per row.
pub fn text(buf: &Buffer) -> String {
    let a = buf.area;
    let mut s = String::new();
    for y in a.y..a.y + a.height {
        for x in a.x..a.x + a.width {
            s.push_str(buf[(x, y)].symbol());
        }
        s.push('\n');
    }
    s
}

/// Render a panel at `w × h` and return its text.
pub fn render(p: &mut dyn Panel<UiAction>, w: u16, h: u16) -> String {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    p.render(area, &mut buf);
    text(&buf)
}

/// The three sizes every screen must survive.
pub const SIZES: [(u16, u16); 3] = [(120, 40), (80, 24), (60, 14)];
