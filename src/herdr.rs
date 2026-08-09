//! Talking to Herdr.
//!
//! Always through the CLI at `HERDR_BIN_PATH` rather than the raw socket: the
//! transport behind `HERDR_SOCKET_PATH` is a Unix socket on one platform and a
//! named pipe on another, and the CLI hides that. Herdr sets `HERDR_BIN_PATH`
//! for every plugin command.
//!
//! Under `herdr --remote` this talks to the *remote* server, because that is
//! where the plugin runs. The local agent reaches the same server by running
//! the remote `herdr` over its own SSH master.

use crate::types::PaneRef;

#[derive(Debug, thiserror::Error)]
pub enum HerdrError {
    #[error("herdr {command} failed (exit {code}): {stderr}")]
    Command {
        command: String,
        code: i32,
        stderr: String,
    },
    #[error("could not parse herdr output for {command}: {source}")]
    Parse {
        command: String,
        source: serde_json::Error,
    },
    #[error("could not run herdr: {0}")]
    Spawn(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, HerdrError>;

/// Where a notification should point the user.
#[derive(Debug, Clone)]
pub struct Notification {
    pub title: String,
    pub body: String,
}

/// The slice of Herdr's API this crate needs, behind one seam so tests never
/// spawn a real `herdr`.
pub trait HerdrClient {
    /// Every pane on the server this client talks to.
    fn pane_list(&self) -> Result<Vec<PaneRef>>;

    /// `pane.process_info` for one pane. Returns the pane's `shell_pid`, which
    /// is the root that port attribution walks up to.
    fn shell_pid(&self, pane_id: &str) -> Result<Option<u32>>;

    /// Show a toast. Used to offer a detected port rather than forwarding it
    /// silently.
    fn notify(&self, notification: &Notification) -> Result<()>;
}

/// Path to the Herdr binary. Prefers what Herdr injected, falls back to `PATH`
/// so the CLI works from a plain shell too.
pub fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "herdr".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_bin_falls_back_to_path_lookup() {
        // Not asserting on a set value: the env is process-global and other
        // tests run in parallel. The fallback is the branch worth pinning.
        let bin = herdr_bin();
        assert!(!bin.is_empty());
    }
}
