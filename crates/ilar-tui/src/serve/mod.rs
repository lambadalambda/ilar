//! `ilar serve` — the read-only HTTP view of the session store.
//!
//! Slice 2 lands the projection only; the watcher and the router are
//! siblings that arrive next and become this module's consumers.
#![allow(dead_code)]

pub(crate) mod view;
