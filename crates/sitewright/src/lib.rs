//! Sitewright: turn a prompt into a live website on your own server.
//!
//! This crate wires the daemon and CLI together. The interesting parts live in the
//! `sw-*` crates; see `docs/architecture.md`.

pub mod cli;
pub mod client;
pub mod daemon;
pub mod prompt;
