//! The PTY wrapper: passthrough output with a reserved dock at the bottom,
//! a shadow emulator, input interception and the dock renderer.
//! See docs/DESIGN.md §7.
//!
//! The `agp` binary drives a session through [`runner::Runner`] and a
//! [`runner::Host`] implementation. Panels (menu, settings) implement
//! [`panel::Panel`] and are drawn inside the expanded dock.

pub mod dock;
pub mod filter;
pub mod input;
pub mod layout;
pub mod panel;
pub mod pty;
pub mod runner;
