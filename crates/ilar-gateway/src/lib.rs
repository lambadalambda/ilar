//! ilar as an always-on assistant.
//!
//! The core already has the hard parts — the loop, sessions that
//! outlive their window, subagents, tools, a headless turn. This crate
//! is the shell picoclaw wraps around a much thinner loop: channels a
//! person talks through, a bus between them and the agent, a route
//! from each chat to its session, and a driver that runs turns on the
//! library runtime and hands the answer back.
//!
//! It stays thin on purpose. `ilar serve` was stood down for growing a
//! transcript view of its own; this driver needs a turn's final text
//! and the loop's events, nothing more.

pub mod bus;
pub mod channel;
pub mod config;
pub mod deltachat;
pub mod driver;
pub mod gateway;
pub mod inbox;
pub mod message;
pub mod policy;
pub mod routes;
