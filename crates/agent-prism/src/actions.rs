//! Actions that panels (menu, settings, countdown, ...) ask the session host
//! to perform. This is the `A` type of `prism_term::panel::Panel<A>`.

use prism_core::model::DockMode;

#[derive(Clone, Debug, PartialEq)]
pub enum UiAction {
    /// Move this session to another account now. `None` = the balancer's pick.
    SwitchTo(Option<String>),
    /// Run the pending countdown switch at once.
    ConfirmSwitch,
    /// Cancel the pending countdown switch and stay.
    CancelSwitch,
    SetDockMode(DockMode),
    /// Refresh usage now (`None` = all accounts).
    RefreshUsage(Option<String>),
    /// The config file was saved; reload it and apply it live.
    ConfigChanged,
}
