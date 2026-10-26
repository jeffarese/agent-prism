//! Config, profiles, identity, Keychain and Codex logins, the share map and
//! adopt.
//! See docs/DESIGN.md §2–§5.

pub mod model;

pub mod backup;
pub mod codex;
pub mod config;
pub mod discover;
pub mod env;
mod fsutil;
pub mod identity;
pub mod journal;
pub mod keychain;
pub mod paths;
pub mod procs;
pub mod profile;
pub mod registry;
pub mod share;
pub mod trust;
