//! Types shared by the balancer, the dock and the settings UI.
//!
//! Times are unix seconds. Percentages are `f64` in 0..=100.

use serde::{Deserialize, Serialize};

pub type ProfileId = String;

/// The coding agent an account belongs to. A session runs one tool, and it
/// moves only between accounts of that tool.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Tool {
    /// Claude Code with a Claude subscription.
    #[default]
    Claude,
    /// The Codex CLI with a ChatGPT subscription.
    Codex,
}

impl Tool {
    pub const ALL: [Tool; 2] = [Tool::Claude, Tool::Codex];

    /// The product name, e.g. "Claude".
    pub fn name(self) -> &'static str {
        match self {
            Tool::Claude => "Claude",
            Tool::Codex => "Codex",
        }
    }

    /// The subscription that pays for it, e.g. "ChatGPT".
    pub fn plan_name(self) -> &'static str {
        match self {
            Tool::Claude => "Claude",
            Tool::Codex => "ChatGPT",
        }
    }

    /// The executable name.
    pub fn bin(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
        }
    }

    /// The env var that selects the config dir.
    pub fn home_env(self) -> &'static str {
        match self {
            Tool::Claude => "CLAUDE_CONFIG_DIR",
            Tool::Codex => "CODEX_HOME",
        }
    }

    /// The name of the default dir under the home dir: `.claude` or `.codex`.
    pub fn dir_name(self) -> &'static str {
        match self {
            Tool::Claude => ".claude",
            Tool::Codex => ".codex",
        }
    }

    pub fn is_claude(&self) -> bool {
        *self == Tool::Claude
    }
}

/// One usage window, e.g. the 5-hour or the 7-day limit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub utilization: f64,
    pub resets_at: Option<i64>,
    /// True when agent-prism derived the value (a past reset means 0%), not the API.
    #[serde(default)]
    pub inferred: bool,
}

/// A model-scoped weekly window (`limits[].kind == "weekly_scoped"`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScopedWindow {
    /// Model display name from the API, e.g. "Fable".
    pub label: String,
    pub kind: String,
    pub utilization: f64,
    pub resets_at: Option<i64>,
    #[serde(default)]
    pub inferred: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub fetched_at: i64,
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    #[serde(default)]
    pub scoped: Vec<ScopedWindow>,
    #[serde(default)]
    pub extra_usage_enabled: bool,
    /// The service says the account cannot be used now (Codex
    /// `rate_limit.limit_reached`), whatever the percentages show.
    #[serde(default)]
    pub limit_reached: bool,
}

/// Projected utilization at the next reset, from recent samples.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Projection {
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TokenState {
    Valid { expires_at: i64 },
    Expired { since: i64 },
    Missing,
    Unreadable(String),
}

/// Who a profile is logged in as. Never holds a secret.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountIdentity {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub org: Option<String>,
    /// e.g. "max", "team", "pro".
    pub subscription: Option<String>,
    /// e.g. "default_claude_max_5x".
    pub rate_limit_tier: Option<String>,
    #[serde(default)]
    pub tool: Tool,
}

impl AccountIdentity {
    /// Plan multiplier from the rate-limit tier or subscription (5x = 1.0).
    /// Only accounts of the same tool are compared.
    pub fn capacity(&self) -> f64 {
        let tier = self.rate_limit_tier.as_deref().unwrap_or("").to_lowercase();
        let sub = self.subscription.as_deref().unwrap_or("").to_lowercase();
        if self.tool == Tool::Codex {
            // ChatGPT plans (`chatgpt_plan_type`).
            return match sub.as_str() {
                "pro" => 2.0,
                "plus" => 0.4,
                "free" | "go" => 0.1,
                _ => 1.0,
            };
        }
        if tier.contains("20x") {
            2.0
        } else if tier.contains("5x") || sub == "team" {
            1.0
        } else if sub == "pro" {
            0.4
        } else {
            1.0
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UsageStatus {
    Fresh,
    Stale { age_secs: i64 },
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LimitKind {
    FiveHour,
    SevenDay,
    Scoped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Limited {
    pub kind: LimitKind,
    /// When the limiting window resets, if known.
    pub until: Option<i64>,
}

/// Everything the dock and the balancer need to know about one account.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountView {
    pub profile_id: ProfileId,
    pub label: String,
    /// A color name from the config ("cyan", "magenta", ...), if set.
    pub color: Option<String>,
    pub identity: AccountIdentity,
    pub token: TokenState,
    pub usage: Option<UsageSnapshot>,
    pub usage_status: UsageStatus,
    pub projection: Projection,
    pub in_pool: bool,
    /// This agent-prism session runs on this account now.
    pub active_here: bool,
    /// Number of other live agent-prism sessions on this account.
    pub active_elsewhere: u32,
    pub limited: Option<Limited>,
    /// The balancer would pick this account for the next new session.
    pub next_pick: bool,
    pub score: Option<f64>,
    /// Short reason for the score or the pick, for the UI.
    pub reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DockMode {
    Auto,
    Full,
    #[default]
    Compact,
    Hidden,
}

/// Which icons the dock draws. `Nerd` needs a Nerd Font in the terminal;
/// `Unicode` works with any font.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DockIcons {
    Unicode,
    #[default]
    Nerd,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    #[default]
    Smart,
    Priority,
    RoundRobin,
    Headroom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnLimit {
    #[default]
    Auto,
    Ask,
    Stay,
    /// Keep the account, wait for its reset, then go on there.
    Wait,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageLevel {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StatusMessage {
    pub level: MessageLevel,
    pub text: String,
}

/// A pending automatic switch, shown in the status row.
#[derive(Clone, Debug, PartialEq)]
pub struct Countdown {
    pub from_label: String,
    pub to_label: String,
    /// Why, e.g. "hit the 5h limit".
    pub reason: String,
    pub deadline: i64,
}

/// Everything the dock renders. Built by the `agp` binary, drawn by
/// `prism-term`.
#[derive(Clone, Debug, PartialEq)]
pub struct DockModel {
    pub accounts: Vec<AccountView>,
    pub mode: DockMode,
    pub policy: Policy,
    /// Short session id, e.g. "3f2a91c0".
    pub session_short: Option<String>,
    /// Label of the account in use and since when.
    pub on_account: Option<(String, i64)>,
    /// Seconds since the newest usage fetch, for "usage 40s ago".
    pub usage_age_secs: Option<i64>,
    pub message: Option<StatusMessage>,
    pub countdown: Option<Countdown>,
    /// Label of the menu key, e.g. "⌃\\".
    pub menu_key_label: String,
    pub show_projections: bool,
    pub icons: DockIcons,
    /// Current unix time, so rendering is deterministic in tests.
    pub now: i64,
}
