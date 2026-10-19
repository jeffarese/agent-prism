//! Data helpers shared by the screens: building [`LiveData`] from the usage
//! cache, the balancer preview, account facts and a one-line text field.

use std::collections::HashMap;

use prism_core::config::{Config, ProfileConfig};
use prism_core::keychain::{read_credentials, token_state};
use prism_core::model::{AccountView, DockModel, TokenState, Tool, UsageStatus};
use prism_core::profile::Profile;
use prism_usage::balance::{BalanceSettings, PickResult, pick};
use prism_usage::cache::CachedUsage;
use prism_usage::views::{AccountInput, ViewCtx, build_views};

use crate::app::AppCtx;
use crate::live::LiveData;

/// Current unix time in seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Balancer settings from the config.
pub fn balance_settings(config: &Config) -> BalanceSettings {
    BalanceSettings {
        policy: config.balance.policy,
        soft_5h: config.balance.soft_5h,
        hard: config.balance.hard,
    }
}

fn view_ctx(config: &Config) -> ViewCtx {
    ViewCtx {
        now: now(),
        model: None,
        settings: balance_settings(config),
        exclude: None,
        stale_after_secs: (config.usage.poll_idle_secs as i64) * 2,
    }
}

/// Account inputs for every configured profile, reading identity and
/// credentials (plan, token expiry) through the context's secret store.
/// One thread per profile: each Keychain read starts a `security` process.
pub fn account_inputs(ctx: &AppCtx, config: &Config) -> Vec<AccountInput> {
    let t = now();
    let input = |p: &Profile| {
        let mut identity = prism_core::identity::read_identity(p, &ctx.paths);
        let token = match read_credentials(ctx.store.as_ref(), p) {
            Ok(c) => {
                if let Some(c) = &c {
                    identity.subscription = c.subscription_type.clone();
                    identity.rate_limit_tier = c.rate_limit_tier.clone();
                }
                token_state(c.as_ref(), t)
            }
            Err(e) => TokenState::Unreadable(format!("{e:#}")),
        };
        input_for(p, identity, token)
    };
    let profiles = ctx.profiles(config);
    std::thread::scope(|s| {
        let jobs: Vec<_> = profiles.iter().map(|p| s.spawn(move || input(p))).collect();
        jobs.into_iter()
            .map(|j| j.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    })
}

fn input_for(
    p: &Profile,
    identity: prism_core::model::AccountIdentity,
    token: TokenState,
) -> AccountInput {
    AccountInput {
        profile_id: p.id.clone(),
        label: p.label.clone(),
        color: p.color.clone(),
        in_pool: p.pool,
        weight: p.weight,
        reserve_7d: p.reserve_7d,
        identity,
        token,
        sessions_elsewhere: 0,
        active_here: false,
        last_used: None,
    }
}

/// Live data for a screen without a session (full-screen settings, the
/// wizard): views and pick from the usage cache.
pub fn load_live(ctx: &AppCtx, config: &Config, log: Vec<String>) -> LiveData {
    let inputs = account_inputs(ctx, config);
    let usage: HashMap<String, CachedUsage> = inputs
        .iter()
        .filter_map(|i| {
            ctx.cache
                .load(&i.profile_id)
                .map(|c| (i.profile_id.clone(), c))
        })
        .collect();
    let vctx = view_ctx(config);
    let views = build_views(&inputs, &usage, &HashMap::new(), &vctx);
    // The pick of a plain `agp` (Claude); `agp codex` picks on its own.
    let claude: Vec<AccountInput> = inputs
        .iter()
        .filter(|i| i.identity.tool == Tool::Claude)
        .cloned()
        .collect();
    let pick = Some(pick(&claude, &usage, &vctx));
    LiveData {
        config: config.clone(),
        views,
        pick,
        session: None,
        log,
    }
}

/// The balancer's pick for `config` (possibly edited) using the usage and
/// identities already in `views`. Profiles without a view count as unknown.
pub fn preview_pick(config: &Config, views: &[AccountView]) -> PickResult {
    let mut usage = HashMap::new();
    let inputs: Vec<AccountInput> = config
        .profiles
        .iter()
        .filter(|p| p.tool == Tool::Claude)
        .map(|p| {
            let v = views.iter().find(|v| v.profile_id == p.id);
            if let Some(s) = v.and_then(|v| v.usage.clone()) {
                usage.insert(
                    p.id.clone(),
                    CachedUsage {
                        snapshot: Some(s.clone()),
                        last_attempt: s.fetched_at,
                        ..Default::default()
                    },
                );
            }
            AccountInput {
                profile_id: p.id.clone(),
                label: label_of(p),
                color: p.color.clone(),
                in_pool: p.pool,
                weight: p.weight,
                reserve_7d: p.reserve_7d,
                identity: v.map(|v| v.identity.clone()).unwrap_or_default(),
                token: v.map(|v| v.token.clone()).unwrap_or(TokenState::Missing),
                sessions_elsewhere: v.map_or(0, |v| v.active_elsewhere),
                active_here: v.is_some_and(|v| v.active_here),
                last_used: None,
            }
        })
        .collect();
    pick(&inputs, &usage, &view_ctx(config))
}

/// A one-line description of a pick: `Acme Al — most 5h headroom`.
pub fn pick_line(config: &Config, pick: &PickResult) -> (Option<String>, String) {
    let label = pick.chosen.as_ref().map(|id| {
        config
            .profiles
            .iter()
            .find(|p| &p.id == id)
            .map(label_of)
            .unwrap_or_else(|| id.clone())
    });
    let reason = match (&pick.chosen, &pick.earliest_reset) {
        (None, Some((l, t))) => format!(
            "{} · {l} resets in {}",
            pick.reason,
            prism_term::dock::fmt_duration(t - now())
        ),
        _ => pick.reason.clone(),
    };
    (label, reason)
}

/// The label of a profile entry (the id when empty).
pub fn label_of(p: &ProfileConfig) -> String {
    if p.label.is_empty() {
        p.id.clone()
    } else {
        p.label.clone()
    }
}

/// 5h and 7d utilization of a view, if known.
pub fn usage_pcts(v: &AccountView) -> (Option<f64>, Option<f64>) {
    let u = v.usage.as_ref();
    (
        u.and_then(|u| u.five_hour.as_ref()).map(|w| w.utilization),
        u.and_then(|u| u.seven_day.as_ref()).map(|w| w.utilization),
    )
}

/// Short login state: `logged in`, `idle token`, `not logged in`, ...
pub fn login_state(token: &TokenState) -> (&'static str, bool) {
    match token {
        TokenState::Valid { .. } => ("logged in", true),
        TokenState::Expired { .. } => ("logged in · idle", true),
        TokenState::Missing => ("not logged in", false),
        TokenState::Unreadable(_) => ("keychain error", false),
    }
}

/// Short usage freshness, e.g. `stale 19h`.
pub fn usage_note(v: &AccountView) -> Option<String> {
    match &v.usage_status {
        UsageStatus::Fresh => None,
        UsageStatus::Stale { age_secs } => Some(format!(
            "stale {}",
            prism_term::dock::fmt_duration(*age_secs)
        )),
        UsageStatus::Unavailable(_) => Some("no usage yet".into()),
    }
}

/// A dock model for previews.
pub fn dock_model(
    config: &Config,
    views: &[AccountView],
    mode: prism_core::model::DockMode,
) -> DockModel {
    use prism_core::config::DockShow;
    let accounts = views
        .iter()
        .filter(|v| config.dock.show == DockShow::All || v.in_pool)
        .cloned()
        .collect();
    DockModel {
        accounts,
        mode,
        policy: config.balance.policy,
        session_short: None,
        on_account: None,
        usage_age_secs: None,
        message: None,
        countdown: None,
        menu_key_label: "⌃\\".into(),
        show_projections: config.dock.projections,
        icons: config.dock.icons,
        now: now(),
    }
}

/// Is `id` a valid profile id: lowercase letters, digits and `-`, not
/// starting or ending with `-`, at most 32 characters.
pub fn valid_slug(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A minimal one-line text input.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextField {
    /// The text.
    pub text: String,
    /// Cursor position in characters.
    pub cursor: usize,
}

impl TextField {
    /// A field holding `text` with the cursor at the end.
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            cursor: text.chars().count(),
        }
    }

    fn byte_at(&self, ci: usize) -> usize {
        self.text
            .char_indices()
            .nth(ci)
            .map_or(self.text.len(), |(b, _)| b)
    }

    /// Insert text at the cursor (newlines dropped).
    pub fn insert(&mut self, s: &str) {
        for ch in s.chars().filter(|c| !c.is_control()) {
            let b = self.byte_at(self.cursor);
            self.text.insert(b, ch);
            self.cursor += 1;
        }
    }

    /// Edit keys: chars, backspace, delete, ←/→, home/end, ctrl+u.
    /// Returns false for keys it does not handle.
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                self.text.clear();
                self.cursor = 0;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.text.chars().count(),
            KeyCode::Char(c) if !ctrl => self.insert(&c.to_string()),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let b = self.byte_at(self.cursor);
                self.text.remove(b);
            }
            KeyCode::Delete if self.cursor < self.text.chars().count() => {
                let b = self.byte_at(self.cursor);
                self.text.remove(b);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.text.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.chars().count(),
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    /// Spans for the field with a visible cursor (reversed cell).
    pub fn spans(&self, focused: bool) -> Vec<ratatui::text::Span<'static>> {
        use ratatui::style::{Modifier, Style};
        use ratatui::text::Span;
        if !focused {
            return vec![Span::raw(self.text.clone())];
        }
        let b = self.byte_at(self.cursor);
        let (pre, rest) = self.text.split_at(b);
        let mut it = rest.chars();
        let cur = it.next().map_or(" ".to_string(), |c| c.to_string());
        let post: String = it.collect();
        vec![
            Span::raw(pre.to_string()),
            Span::styled(cur, Style::default().add_modifier(Modifier::REVERSED)),
            Span::raw(post),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn slugs() {
        for ok in ["a", "acme-al", "x2", "a-b-c"] {
            assert!(valid_slug(ok), "{ok}");
        }
        for bad in ["", "A", "a b", "-a", "a-", "a_b", "ä", &"a".repeat(33)] {
            assert!(!valid_slug(bad), "{bad}");
        }
    }

    #[test]
    fn text_field_edits() {
        let mut f = TextField::new("ab");
        f.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        f.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(f.text, "axb");
        f.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(f.text, "ab");
        f.insert("c\nd");
        assert_eq!(f.text, "acdb");
        assert!(!f.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    }
}
