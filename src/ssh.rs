//! SSH ControlMaster lifecycle and dynamic port forwarding.
//!
//! Forwards are added to and removed from a running master with `ssh -O
//! forward` / `-O cancel`, so no connection is torn down when the set of
//! forwards changes.
//!
//! The master is ours, not Herdr's. Under `herdr --remote` Herdr keeps its own
//! master at `$TMPDIR/herdr-ssh-<pid>-<n>/ctl`, but that path is an internal
//! detail, it is `-O exit`ed when the attach ends, and
//! `[remote].manage_ssh_config = false` removes it entirely. See
//! `docs/ARCHITECTURE.md`.

use std::path::{Path, PathBuf};

use crate::types::ForwardSpec;

/// `sun_path` is 104 bytes on macOS and 108 on Linux. Use the smaller limit
/// everywhere and leave a byte for the NUL: exceeding it fails at connect time
/// with an error that does not mention path length.
pub const MAX_CONTROL_PATH_LEN: usize = 103;

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("control socket path is {len} bytes, over the {max}-byte limit: {path}")]
    ControlPathTooLong {
        path: String,
        len: usize,
        max: usize,
    },
    #[error("no directory to keep control sockets in: set XDG_STATE_HOME or HOME")]
    NoControlDir,
    #[error("ssh {operation} failed for {target} (exit {code}): {stderr}")]
    Command {
        operation: &'static str,
        target: String,
        code: i32,
        stderr: String,
    },
    #[error("could not run ssh: {0}")]
    Spawn(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SshError>;

/// State of a control master, as reported by `ssh -O check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterState {
    Running,
    Absent,
}

/// Everything this crate does with `ssh`, behind one seam so tests never spawn
/// a real one. Implementations must pass argv arrays to the OS, never a shell
/// command line.
pub trait SshRunner {
    /// Start a master for `target` at `control_path` if one is not already
    /// running. Idempotent.
    fn ensure_master(&self, target: &str, control_path: &Path) -> Result<()>;

    /// `ssh -O check`.
    fn check_master(&self, target: &str, control_path: &Path) -> Result<MasterState>;

    /// `ssh -O forward -L <spec>`. Errors if the local port is unavailable.
    fn add_forward(&self, target: &str, control_path: &Path, spec: &ForwardSpec) -> Result<()>;

    /// `ssh -O cancel -L <spec>`. Cancelling a forward that does not exist is a
    /// no-op, not an error: reconciliation calls this speculatively.
    fn cancel_forward(&self, target: &str, control_path: &Path, spec: &ForwardSpec) -> Result<()>;

    /// `ssh -O exit`: stop the master and every forward on it.
    fn exit_master(&self, target: &str, control_path: &Path) -> Result<()>;

    /// Run a command on the remote over the existing master and return stdout.
    /// `command` is passed as a single remote command string, as ssh requires;
    /// callers must not interpolate untrusted input into it.
    fn exec(&self, target: &str, control_path: &Path, command: &str) -> Result<String>;
}

/// Directory holding one control socket per host.
///
/// Not the plugin state dir: that path can be arbitrarily long, and a control
/// socket that overflows `sun_path` fails confusingly.
pub fn control_socket_dir() -> Result<PathBuf> {
    control_socket_dir_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
        .ok_or(SshError::NoControlDir)
}

/// Resolution rule behind [`control_socket_dir`], pure so it is testable
/// without the process environment.
///
/// `None` rather than `/tmp/herdr-portfwd` when neither is set: a fixed path in
/// a world-writable directory is one another local user can create first, and a
/// control socket they own is a connection of theirs that every forward this
/// tool creates would land on.
pub fn control_socket_dir_from(
    xdg_state_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(state) = xdg_state_home.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(state).join("herdr-portfwd"));
    }
    let home = home.filter(|v| !v.is_empty())?;
    Some(PathBuf::from(home).join(".local/state/herdr-portfwd"))
}

/// Control socket path for `host`, rejected up front when it would overflow
/// `sun_path`.
pub fn control_socket_path(dir: &Path, host: &str) -> Result<PathBuf> {
    let path = dir.join(format!("{host}.ctl"));
    let len = path.as_os_str().len();
    if len > MAX_CONTROL_PATH_LEN {
        return Err(SshError::ControlPathTooLong {
            path: path.to_string_lossy().into_owned(),
            len,
            max: MAX_CONTROL_PATH_LEN,
        });
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_path_within_the_limit_is_accepted() {
        let path = control_socket_path(Path::new("/tmp/hpf"), "buildbox")
            .expect("short path should be accepted");
        assert!(path.ends_with("buildbox.ctl"));
    }

    #[test]
    fn the_socket_directory_follows_xdg_then_home() {
        assert_eq!(
            control_socket_dir_from(Some("/xdg".into()), Some("/home/u".into())),
            Some(PathBuf::from("/xdg/herdr-portfwd"))
        );
        assert_eq!(
            control_socket_dir_from(None, Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.local/state/herdr-portfwd"))
        );
        assert_eq!(
            control_socket_dir_from(Some("".into()), Some("".into())),
            None,
            "an exported-but-empty variable names nowhere"
        );
    }

    #[test]
    fn with_nowhere_to_put_a_socket_nothing_is_put_in_tmp() {
        // A control socket at a fixed path in a world-writable directory can be
        // created first by another local user, and every forward this tool
        // makes would then land on their connection.
        assert_eq!(control_socket_dir_from(None, None), None);
    }

    #[test]
    fn control_path_over_the_limit_is_rejected_before_use() {
        let deep = PathBuf::from("/tmp").join("x".repeat(MAX_CONTROL_PATH_LEN));
        let err = control_socket_path(&deep, "buildbox")
            .expect_err("an over-long path must be rejected, not passed to ssh");
        assert!(matches!(err, SshError::ControlPathTooLong { .. }));
    }
}
