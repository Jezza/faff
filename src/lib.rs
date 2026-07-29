//! faf — a jj-native TUI for managing parallel Claude Code agents.
//!
//! See `docs/superpowers/specs/2026-07-21-faf-design.md` for the full design.

pub mod cli;
pub mod config;
pub mod domain;
pub mod events;
pub mod graph;
pub mod jj;
pub mod scheduler;
pub mod store;
pub mod tui;
pub mod wezterm;
pub mod workspace;
