//! ilar core: providers, tools, agent loop, sessions, config.
//!
//! Pure logic — no TUI dependencies. Everything here is unit-testable
//! with mock providers.

pub mod agent;
pub mod aside;
mod atomic_file;
pub mod auth;
pub mod checkpoint;
pub mod command;
pub mod compaction;
pub mod config;
pub mod image;
pub mod model;
pub mod outbox;
pub mod provider;
pub mod question;
pub mod recall;
pub mod rewind;
pub mod runtime;
pub mod session;
pub mod skill;
pub mod subagent;
pub mod text;
pub mod todo;
pub mod tools;
pub mod topic;
