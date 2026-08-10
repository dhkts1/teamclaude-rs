//! teamclaude-rs — a lean single-user rotating Anthropic proxy.
//!
//! A drop-in replacement for the personal teamclaude proxy: it reads the same
//! `~/.config/teamclaude.json`, rotates across the configured OAuth accounts as
//! their quota fills, refreshes tokens on expiry, streams requests to Anthropic
//! unchanged, and surfaces a live TUI. See `DESIGN.md` for the full contract.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), warn(clippy::unwrap_used))]
#![warn(
    clippy::todo,
    clippy::dbg_macro,
    clippy::map_unwrap_or,
    clippy::manual_let_else
)]

pub mod account_uuid;
pub mod affinity;
pub mod build_info;
pub mod cli;
pub mod config;
pub mod demo;
pub mod identity;
pub mod manager;
pub mod mitm;
pub mod model;
pub mod oauth;
pub mod probe;
pub mod proxy;
pub mod quota;
pub mod schedule;
pub mod server;
pub mod singleton;
pub mod stats;
pub mod status;
pub mod tui;
pub mod update;
pub mod warmer;

use time::OffsetDateTime;

/// Current wall-clock time in Unix milliseconds.
///
/// Epoch-ms is the unit used by the config's `expiresAt` field and by every
/// token-expiry comparison, so keeping one canonical helper avoids drift.
pub fn now_ms() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
