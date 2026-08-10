//! Forward dev-server ports from remote Herdr hosts to the local machine.
//!
//! See `docs/ARCHITECTURE.md` for the design contract. The short version:
//! plugin commands run wherever the Herdr *server* runs, which under
//! `herdr --remote` is the remote host. Opening a local port therefore has to
//! be separable from deciding that a port should be opened, so the crate splits
//! into a request side and an apply side that meet at a serialized record.
//!
//! Everything that shells out does so behind a trait ([`ssh::SshRunner`],
//! [`herdr::HerdrClient`]) so that no test needs a real `ssh`, a real `herdr`,
//! or a network.

#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(clippy::todo, clippy::dbg_macro)]

/// The plugin id this crate is registered under.
///
/// It is `id` in `herdr-plugin.toml`, repeated here because talking back to
/// Herdr means naming the plugin: `plugin pane open --plugin <id>`. Herdr
/// injects the same value as `HERDR_PLUGIN_ID`, which is preferred when it is
/// there; this is the fallback, and a test in `tests/cli.rs` reads the manifest
/// to keep the two from drifting apart.
pub const PLUGIN_ID: &str = "portfwd";

pub mod actions;
pub mod cli;
pub mod config;
pub mod confirm;
pub mod context;
pub mod discover;
pub mod herdr;
pub mod herdr_cli;
pub mod local_agent;
pub mod request;
pub mod ssh;
pub mod ssh_process;
pub mod state;
pub mod types;
pub mod ui;
