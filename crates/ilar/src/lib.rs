//! ilar core: providers, tools, agent loop, sessions, config.
//!
//! Pure logic — no TUI dependencies. Everything here is unit-testable
//! with mock providers.

pub mod agent;
mod atomic_file;
pub mod auth;
pub mod compaction;
pub mod config;
pub mod provider;
pub mod session;
pub mod skill;
pub mod subagent;
pub mod todo;
pub mod tools;
