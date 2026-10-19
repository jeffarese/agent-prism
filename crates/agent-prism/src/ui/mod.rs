//! All interactive views: the in-dock menu and settings panels, the
//! full-screen settings app, the onboarding wizard and the add-account flow.
//!
//! Contract with the session host (crates/agent-prism/src/host.rs): the entry
//! points below keep these signatures.

mod add;
mod adopt;
mod art;
mod data;
mod fullscreen;
mod login;
mod menu;
mod settings;
mod theme;
mod wizard;

#[cfg(test)]
mod testutil;

use std::sync::Arc;

use anyhow::Result;
use prism_core::config::Config;
use prism_term::panel::Panel;

use crate::actions::UiAction;
use crate::app::AppCtx;
use crate::live::Live;

/// Settings tabs (DESIGN §10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Accounts,
    Balancing,
    Dock,
    Sharing,
    Keys,
    About,
}

/// The menu that the menu key opens inside the dock.
pub fn menu_panel(ctx: Arc<AppCtx>, live: Live) -> Box<dyn Panel<UiAction>> {
    Box::new(menu::MenuPanel::new(ctx, live))
}

/// The settings panel, for the dock or for full screen.
pub fn settings_panel(ctx: Arc<AppCtx>, live: Live, tab: Tab) -> Box<dyn Panel<UiAction>> {
    Box::new(settings::SettingsPanel::new(ctx, live, tab))
}

/// `agp settings`: the settings panel full screen. Returns when closed.
pub fn run_settings(ctx: Arc<AppCtx>) -> Result<()> {
    let config = ctx.load_config()?;
    let live = Live::new(data::load_live(&ctx, &config, Vec::new()));
    let mut panel = settings::SettingsPanel::new(ctx.clone(), live.clone(), Tab::Accounts);
    panel.set_full_screen();
    fullscreen::run(&ctx, &live, &mut panel)
}

/// First run and `agp setup`: the onboarding wizard. Writes the config and
/// returns it, or `None` if the user quits.
pub fn run_onboarding(ctx: Arc<AppCtx>) -> Result<Option<Config>> {
    let mut w = wizard::Wizard::new(ctx.clone());
    fullscreen::run(&ctx, &Live::default(), &mut w)?;
    Ok(w.result.take())
}

/// `agp add [id]`: create a profile, log in, add it to the config.
pub fn run_add(
    ctx: Arc<AppCtx>,
    id: Option<String>,
    tool: Option<prism_core::model::Tool>,
) -> Result<()> {
    let config = ctx.load_config()?;
    let mut app = add::AddApp::new(ctx.clone(), config, id, tool);
    fullscreen::run(&ctx, &Live::default(), &mut app)?;
    if let Some(e) = app.error {
        anyhow::bail!(e);
    }
    Ok(())
}
