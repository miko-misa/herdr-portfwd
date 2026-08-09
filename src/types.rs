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
    ///
    /// Brackets are stripped first because `lsof` and `ss` disagree about them,
    /// and the IPv6 wildcard reaches here as bare `::` from both parsers.
    pub fn is_forwardable(&self) -> bool {
        let bind = self.bind.trim_start_matches('[').trim_end_matches(']');
        matches!(bind, "127.0.0.1" | "::1" | "*" | "0.0.0.0" | "::")
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

/// Bind address for the listening end of every forward.
///
/// Named explicitly rather than left out: a three-field `-L` spec binds "in
/// accordance with the GatewayPorts setting", so a user whose `ssh_config` says
/// `GatewayPorts yes` would have every forwarded dev server bound to the
/// wildcard address and reachable from their network. `man ssh` documents
/// `localhost` as meaning local use only, and unlike a literal `127.0.0.1` it
/// still covers both loopback families, which matters to a client that resolves
/// `localhost` to `::1`. This is the unconditional half of SECURITY.md's
/// loopback promise.
pub const LOCAL_BIND: &str = "localhost";

/// Bracket a bare IPv6 destination: `ssh -L` splits its argument on colons, and
/// `5173:::1:5173` means nothing in particular.
fn bracketed(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

impl ForwardSpec {
    /// `-L` argument for `ssh -O forward`/`-O cancel`. The two must be
    /// byte-identical or the cancel silently matches nothing.
    pub fn forward_arg(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            LOCAL_BIND,
            self.local_port,
            bracketed(&self.remote_bind),
            self.remote_port
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
        assert_eq!(
            spec(5173, 5173).forward_arg(),
            "localhost:5173:127.0.0.1:5173"
        );
    }

    #[test]
    fn the_listening_end_is_bound_to_loopback_explicitly() {
        // Without a bind address ssh honours GatewayPorts, and a user with
        // `GatewayPorts yes` in ssh_config would have the forward bound to the
        // wildcard address — the one thing SECURITY.md promises cannot happen.
        let arg = spec(5173, 5173).forward_arg();
        assert!(
            arg.starts_with("localhost:"),
            "the spec must name its bind address: {arg}"
        );
        assert_eq!(arg.split(':').count(), 4, "a four-field -L spec: {arg}");
    }

    #[test]
    fn an_ipv6_destination_is_bracketed_so_the_spec_stays_unambiguous() {
        let mut v6 = spec(8501, 8501);
        v6.remote_bind = "::1".into();
        assert_eq!(v6.forward_arg(), "localhost:8501:[::1]:8501");

        // Already bracketed by whoever built the spec: not bracketed twice.
        let mut bracketed = spec(8501, 8501);
        bracketed.remote_bind = "[::1]".into();
        assert_eq!(bracketed.forward_arg(), "localhost:8501:[::1]:8501");
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
        // `--host 0.0.0.0` is the default for `vite --host`, `next dev -H` and
        // most containerized servers, and it is what `ss -ltnH` reports for
        // them. Dropped from the matcher, the commonest dev server there is
        // vanishes from `scan` and from the picker with nothing said.
        assert!(listener("0.0.0.0").is_forwardable());
        assert!(!listener("192.168.1.10").is_forwardable());
        assert!(!listener("10.0.0.9").is_forwardable());
    }

    #[test]
    fn ipv6_binds_are_forwardable_bracketed_or_not() {
        let listener = |bind: &str| Listener {
            port: 5173,
            bind: bind.into(),
            pid: 1,
            process: "node".into(),
        };
        // `ss` reports `[::]`, the parsers hand this type a bare `::`, and a
        // wildcard v6 listener is exactly the case a dev server produces.
        for bind in ["::", "[::]", "::1", "[::1]"] {
            assert!(
                listener(bind).is_forwardable(),
                "{bind} should be forwardable"
            );
        }
        // A routable v6 address is not reachable through a loopback forward,
        // bracketed or not: stripping the brackets must not lose the check.
        assert!(!listener("2001:db8::1").is_forwardable());
        assert!(!listener("[2001:db8::1]").is_forwardable());
    }
}
