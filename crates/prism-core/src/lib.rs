//! Config, profiles, identity, Claude (Keychain or file) and Codex logins,
//! the share map, adopt, and what differs between macOS and Linux.
//! See docs/DESIGN.md §2–§5.

pub mod model;

pub mod backup;
pub mod codex;
pub mod config;
pub mod discover;
pub mod env;
pub mod fsutil;
pub mod identity;
pub mod journal;
pub mod keychain;
pub mod paths;
pub mod platform;
pub mod procs;
pub mod profile;
pub mod registry;
pub mod settings_sync;
pub mod share;
pub mod trust;
