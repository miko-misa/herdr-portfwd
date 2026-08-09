//! Domain types shared across modules.
//!
//! These are deliberately plain data. Anything that needs I/O to produce one of
//! these lives behind a trait in [`crate::ssh`] or [`crate::herdr`].

use serde::{Deserialize, Serialize};

/// A configured remote host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// Key in `hosts.toml`, and the name used in state files and log lines.
    pub name: String,
    /// Anything ssh accepts: an alias, `user@host`, or `ssh://host:2222`.
    pub target: String,
    /// Workspace label prefix used by herdr-mirror (`"mini: scholion"`).
    /// Resolving a mirrored workspace back to its host goes through this.
    pub prefix: Option<String>,
}

/// A pane on some Herdr server, as far as this crate cares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRef {
    pub pane_id: String,
    pub workspace_id: String,
    pub workspace_label: Option<String>,
    pub agent: Option<String>,
    /// From `pane.process_info`. The root of the process subtree a listener
    /// must descend from to be attributed to this pane.
    pub shell_pid: Option<u32>,
}

/// A TCP socket listening on the remote host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listener {
    pub port: u16,
    /// Bind address as reported by `lsof`/`ss`: `127.0.0.1`, `::1`, `*`, ...
    pub bind: String,
    pub pid: u32,
    /// Process name as reported. Truncated by `lsof`; display only.
    pub process: String,
}

impl Listener {
    /// Whether the listener is reachable through a loopback-targeted forward.
    /// A wildcard bind is, a bind to a specific non-loopback address is not.
    pub fn is_forwardable(&self) -> bool {
        matches!(
            self.bind.as_str(),
            "127.0.0.1" | "::1" | "*" | "0.0.0.0" | "[::]"
        )
    }
}

/// A listener resolved to the pane that owns it, when one does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributedListener {
    pub listener: Listener,
    /// `None` when the process ancestry reaches no Herdr pane. Those are not
    /// offered by default: see the attribution section of `ARCHITECTURE.md`.
    pub pane: Option<PaneRef>,
}

/// Why a forward exists. Recorded so `reconcile` can tell a user-pinned
/// forward from one that should disappear with its pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Ctrl+click on a URL, or an explicit `add`.
    Explicit,
    /// A detected listener the user approved.
    Offered,
    /// Matched an opt-in allowlist.
    Auto,
}

/// A forward that should exist: remote `port` reachable at `local_port` here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardSpec {
    pub host: String,
    pub remote_port: u16,
    /// Defaults to `remote_port`. Differing means the user accepted a remap
    /// and knows it can break URLs that embed the port.
    pub local_port: u16,
    /// Remote-side destination, resolved on the remote. Loopback only in v1.
    pub remote_bind: String,
    pub origin: Origin,
    /// Pane the port was attributed to, when known. `reconcile` drops
    /// non-explicit forwards whose pane is gone.
    pub pane_id: Option<String>,
}

impl ForwardSpec {
    /// `-L` argument for `ssh -O forward`/`-O cancel`. The two must be
    /// byte-identical or the cancel silently matches nothing.
    pub fn forward_arg(&self) -> String {
        format!(
            "{}:{}:{}",
            self.local_port, self.remote_bind, self.remote_port
        )
    }

    /// Whether the local port was remapped away from the remote one.
    pub fn is_remapped(&self) -> bool {
        self.local_port != self.remote_port
    }
}

/// A forward believed to be live, as recorded in the state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    #[serde(flatten)]
    pub spec: ForwardSpec,
    /// Milliseconds since the Unix epoch. Stamped by the caller so the
    /// clock stays out of the pure layers.
    pub created_unix_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(local: u16, remote: u16) -> ForwardSpec {
        ForwardSpec {
            host: "buildbox".into(),
            remote_port: remote,
            local_port: local,
            remote_bind: "127.0.0.1".into(),
            origin: Origin::Explicit,
            pane_id: None,
        }
    }

    #[test]
    fn forward_arg_is_stable_between_create_and_cancel() {
        assert_eq!(spec(5173, 5173).forward_arg(), "5173:127.0.0.1:5173");
    }

    #[test]
    fn remap_is_visible_on_the_spec() {
        assert!(!spec(5173, 5173).is_remapped());
        assert!(spec(15173, 5173).is_remapped());
    }

    #[test]
    fn only_loopback_and_wildcard_binds_are_forwardable() {
        let listener = |bind: &str| Listener {
            port: 5173,
            bind: bind.into(),
            pid: 1,
            process: "node".into(),
        };
        assert!(listener("127.0.0.1").is_forwardable());
        assert!(listener("*").is_forwardable());
        assert!(!listener("192.168.1.10").is_forwardable());
    }
}
