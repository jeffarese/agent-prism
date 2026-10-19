//! Live data that panels read while they are open. The session host (or the
//! full-screen settings app) owns the writer side and updates it whenever
//! usage, config or session state changes.

use std::sync::{Arc, Mutex, MutexGuard};

use prism_core::config::Config;
use prism_core::model::{AccountView, Tool};
use prism_usage::balance::PickResult;

/// Facts about the running session, if any (none in `agp settings`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionInfo {
    /// The tool this session runs (Claude or Codex).
    pub tool: Tool,
    pub profile_id: String,
    pub session_id: Option<String>,
    /// Claude is working on a turn (between `UserPromptSubmit` and `Stop`).
    /// Always false for Codex (no hooks).
    pub busy: bool,
    /// Past sessions (`projects/`, Codex `sessions/`) are shared for the
    /// current profile, so a switch can resume.
    pub can_switch: bool,
    /// A limit switch waiting for the user (or a countdown).
    pub pending: Option<PendingSwitch>,
}

/// A pending limit switch, for the menu to confirm or cancel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PendingSwitch {
    pub target_id: String,
    pub target_label: String,
    /// Why, e.g. "hit the 5h limit".
    pub reason: String,
}

#[derive(Clone, Debug, Default)]
pub struct LiveData {
    pub config: Config,
    /// One view per configured profile, in config order.
    pub views: Vec<AccountView>,
    /// The balancer's pick for a new session (or the switch target).
    pub pick: Option<PickResult>,
    pub session: Option<SessionInfo>,
    /// Recent log lines, newest last (for the Log view).
    pub log: Vec<String>,
}

/// Shared handle. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct Live(Arc<Mutex<LiveData>>);

impl Live {
    pub fn new(data: LiveData) -> Self {
        Self(Arc::new(Mutex::new(data)))
    }

    pub fn lock(&self) -> MutexGuard<'_, LiveData> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}
