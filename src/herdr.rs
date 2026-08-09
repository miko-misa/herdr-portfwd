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
    /// Every pane on the server this client talks to. `workspace_label` is
    /// always `None`: labels live on the workspace listing, not on panes.
    fn pane_list(&self) -> Result<Vec<PaneRef>>;

    /// The same panes with `workspace_label` filled in, at the cost of a second
    /// round trip.
    ///
    /// On the trait rather than on the concrete client because deciding which
    /// side of the connection we are on depends on it: a mirrored workspace is
    /// recognised by its label prefix, so a caller holding a `&dyn HerdrClient`
    /// that could only reach [`Self::pane_list`] would see every label as
    /// `None`, conclude the server mirrors nothing, and queue a request for a
    /// local agent that is not running.
    fn pane_list_labelled(&self) -> Result<Vec<PaneRef>>;

    /// `pane.process_info` for one pane. Returns the pane's `shell_pid`, which
    /// is the root that port attribution walks up to.
    fn shell_pid(&self, pane_id: &str) -> Result<Option<u32>>;

    /// Show a toast. Used to offer a detected port rather than forwarding it
    /// silently.
    fn notify(&self, notification: &Notification) -> Result<()>;
}

/// Environment variable Herdr injects for every plugin command, naming the
/// binary the plugin should talk back through.
pub const BIN_PATH_ENV: &str = "HERDR_BIN_PATH";

/// Path to the Herdr binary. Prefers what Herdr injected, falls back to `PATH`
/// so the CLI works from a plain shell too.
pub fn herdr_bin() -> String {
    herdr_bin_from(std::env::var_os(BIN_PATH_ENV))
}

/// Resolution rule behind [`herdr_bin`], separated so it is testable without
/// mutating the process-global environment.
///
/// The injected path is the primary source, not a nicety: `herdr` need not be
/// on the `PATH` a plugin command inherits, and without it every `pane list`
/// and every notification fails to spawn — which costs attribution and every
/// L2 offer.
pub fn herdr_bin_from(injected: Option<std::ffi::OsString>) -> String {
    injected
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "herdr".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_herdr_injected_is_what_gets_run() {
        assert_eq!(
            herdr_bin_from(Some("/opt/herdr/bin/herdr".into())),
            "/opt/herdr/bin/herdr"
        );
    }

    #[test]
    fn with_nothing_injected_the_bare_name_is_left_to_path() {
        assert_eq!(herdr_bin_from(None), "herdr");
        // Herdr exports the variable even where it has nothing to put in it.
        assert_eq!(herdr_bin_from(Some("".into())), "herdr");
    }

    #[test]
    fn herdr_bin_falls_back_to_path_lookup() {
        // Not asserting on a set value: the env is process-global and other
        // tests run in parallel. The fallback is the branch worth pinning.
        let bin = herdr_bin();
        assert!(!bin.is_empty());
    }
}
