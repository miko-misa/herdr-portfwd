//! Durable record of which forwards should exist.
//!
//! Lives under `HERDR_PLUGIN_STATE_DIR` when Herdr provides one, and under
//! `$XDG_STATE_HOME/herdr-portfwd` otherwise, so the CLI and the plugin agree
//! on one file. Control sockets deliberately do *not* live here: see
//! [`crate::ssh::control_socket_path`].
//!
//! Concurrency is real. A `[[startup]]` hook, an event hook, an action, and the
//! local agent can all write at once. Every mutation is lock, read, mutate,
//! atomically replace — advisory `flock` on a lock file, then write a sibling
//! temp file and `rename` it into place. A crash must never leave a truncated
//! state file. This is invariant 2 in `ARCHITECTURE.md`.
