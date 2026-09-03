//! `hyperpanes-core` — headless Rust core for the native rewrite (Phase 1).
//!
//! The module map below is **frozen by the fan-out scaffold**. Each leaf module is a
//! 1:1 port of a TypeScript source under `src/main`, owned by exactly one parallel
//! track (see that module's header and your `FANOUT-HANDOFF.md`). Fill in ONLY your
//! owned files — never `lib.rs`, the `mod.rs` files, or `Cargo.toml` (shared → collide).

pub mod ai;
pub mod ansi_strip;
pub mod app;
pub mod claude_accounts;
pub mod claude_history;
pub mod claude_hook;
pub mod claude_panes;
pub mod claude_recovery;
pub mod cli;
pub mod control;
/// Reading one commit out of a repository — what a clicked hash needs to become a view.
pub mod git;
pub mod hyperpane;
pub mod layout;
pub mod logging;
pub mod open;
pub mod paths;
pub mod permissions;
pub mod persistence;
pub mod resume_queue;
pub mod session;
pub mod session_manager;
pub mod shell_integration;
pub mod single_instance;
pub mod speech;
pub mod stt;
pub mod tools;
pub mod workspace;
