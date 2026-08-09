//! The real [`crate::ssh::SshRunner`], driving the `ssh` binary.
//!
//! Master options mirror what Herdr itself uses for remote attach:
//! `ControlMaster=auto`, `ControlPersist=yes`, plus `BatchMode=yes` so a host
//! that would prompt fails fast instead of hanging a plugin action.
//!
//! Two behaviors are load-bearing:
//! - `-O cancel` for a forward that does not exist must succeed. Reconciliation
//!   calls it speculatively (invariant 4).
//! - the `-L` string given to `cancel` must be byte-identical to the one given
//!   to `forward`, or OpenSSH matches nothing and reports success anyway. Both
//!   come from [`crate::types::ForwardSpec::forward_arg`].
//!
//! Every invocation is an argv array. Nothing user-controlled is ever assembled
//! into a shell command line.
//!
//! The module is split so that the parts worth testing need no `ssh`: argv
//! construction and "what did ssh mean by this exit code and stderr" are pure
//! functions, and [`SshSpawner`] is the thin layer they drive.

use std::fs::{self, DirBuilder, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::ssh::{MasterState, Result, SshError, SshRunner, MAX_CONTROL_PATH_LEN};
use crate::types::ForwardSpec;

/// Directory mode for the control socket's parent. Anyone who can write there
/// can plant a socket that later `-O` commands would talk to.
const CONTROL_DIR_MODE: u32 = 0o700;

/// One finished `ssh` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// The only place this module spawns a process, so every decision above it can
/// be tested from a scripted fake.
pub trait SshSpawner {
    /// Run to completion and capture both streams.
    fn run(&self, argv: &[String]) -> io::Result<SshOutput>;

    /// Run a command that backgrounds itself (`ssh -f`).
    ///
    /// Separate from [`SshSpawner::run`] because `ssh -f` calls `daemon(1, 1)`:
    /// the surviving master inherits our stdout and stderr and holds them for
    /// its whole life. Reading those from a pipe never reaches EOF, so the
    /// foreground call would hang forever. Both streams go to `log_path`
    /// instead, which is read back once the foreground process exits.
    fn run_forking(&self, argv: &[String], log_path: &Path) -> io::Result<SshOutput>;
}

/// Spawns the real `ssh`.
#[derive(Debug, Clone)]
pub struct SystemSsh {
    bin: String,
}

impl SystemSsh {
    pub fn new() -> Self {
        Self {
            bin: "ssh".to_string(),
        }
    }

    /// Point at a different `ssh`, for hosts where it is not on `PATH`.
    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }
}

impl Default for SystemSsh {
    fn default() -> Self {
        Self::new()
    }
}

impl SshSpawner for SystemSsh {
    fn run(&self, argv: &[String]) -> io::Result<SshOutput> {
        // stdin is closed, not inherited: BatchMode suppresses ssh's own
        // prompts, but nothing else should be able to consume our stdin.
        let out = Command::new(&self.bin)
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;
        Ok(SshOutput {
            code: out.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn run_forking(&self, argv: &[String], log_path: &Path) -> io::Result<SshOutput> {
        let log = File::create(log_path)?;
        let dup = log.try_clone()?;
        let status = Command::new(&self.bin)
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(dup))
            .status()?;
        let text = fs::read(log_path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        // Unlink immediately: the persistent master keeps writing to this fd
        // for as long as it lives, and nobody reads it after this point.
        let _ = fs::remove_file(log_path);
        Ok(SshOutput {
            code: status.code().unwrap_or(1),
            stdout: String::new(),
            stderr: text,
        })
    }
}

/// What `ssh -O cancel` meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The master had the forward and dropped it.
    Cancelled,
    /// There was nothing to cancel. Invariant 4: that is success.
    NothingToCancel,
    /// A real failure, worth surfacing.
    Failed,
}

fn socket_args(control_path: &Path) -> Vec<String> {
    vec![
        "-S".to_string(),
        control_path.to_string_lossy().into_owned(),
    ]
}

/// `ssh -M -N -f ...`: start a master and background it.
pub fn master_argv(target: &str, control_path: &Path) -> Vec<String> {
    vec![
        "-M".to_string(),
        "-N".to_string(),
        "-f".to_string(),
        "-o".to_string(),
        format!("ControlPath={}", control_path.to_string_lossy()),
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        "ControlPersist=yes".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        // The master performs the bind for every `-O forward`, using its own
        // options. A user's `GatewayPorts yes` would otherwise put a forwarded
        // dev server on a routable address. The `-L` spec names `localhost`
        // too, which already overrides this; both are here so a master started
        // by an older build still cannot be talked into a wildcard bind.
        "-o".to_string(),
        "GatewayPorts=no".to_string(),
        target.to_string(),
    ]
}

/// `ssh -O check`: ask the socket whether a master is behind it.
pub fn check_argv(target: &str, control_path: &Path) -> Vec<String> {
    let mut argv = socket_args(control_path);
    argv.extend(["-O".to_string(), "check".to_string(), target.to_string()]);
    argv
}

/// `ssh -O forward -L <spec>`.
pub fn forward_argv(target: &str, control_path: &Path, spec: &str) -> Vec<String> {
    mux_forward_argv("forward", target, control_path, spec)
}

/// `ssh -O cancel -L <spec>`. The spec must be byte-identical to the one that
/// created the forward.
pub fn cancel_argv(target: &str, control_path: &Path, spec: &str) -> Vec<String> {
    mux_forward_argv("cancel", target, control_path, spec)
}

fn mux_forward_argv(op: &str, target: &str, control_path: &Path, spec: &str) -> Vec<String> {
    let mut argv = socket_args(control_path);
    argv.extend([
        "-O".to_string(),
        op.to_string(),
        "-L".to_string(),
        spec.to_string(),
        target.to_string(),
    ]);
    argv
}

/// `ssh -O exit`: stop the master and every forward on it.
pub fn exit_argv(target: &str, control_path: &Path) -> Vec<String> {
    let mut argv = socket_args(control_path);
    argv.extend(["-O".to_string(), "exit".to_string(), target.to_string()]);
    argv
}

/// Run `command` on the remote over the existing master.
///
/// `BatchMode` matters even here: if the master died, ssh opens a fresh
/// connection instead, and that one can prompt.
pub fn exec_argv(target: &str, control_path: &Path, command: &str) -> Vec<String> {
    let mut argv = socket_args(control_path);
    argv.extend([
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        target.to_string(),
        command.to_string(),
    ]);
    argv
}

/// Whether ssh is telling us the control socket itself is not usable.
///
/// OpenSSH prints `Control socket connect(<path>): <errno>` and exits non-zero
/// for every `-O` command when the socket is missing, dead, or refusing.
pub fn master_socket_missing(stderr: &str) -> bool {
    stderr
        .to_ascii_lowercase()
        .contains("control socket connect(")
}

/// `ssh -O check` says `Master running (pid=N)` on stderr and exits 0. Any
/// non-zero exit means "no master", not "something went wrong".
pub fn interpret_check(code: i32, stderr: &str) -> MasterState {
    if code == 0 || stderr.contains("Master running") {
        MasterState::Running
    } else {
        MasterState::Absent
    }
}

/// `ssh -O forward` logs `Forwarding port.` on stderr when it *succeeds*, so
/// only the exit status decides. The error text is the reason, not the wrapper.
pub fn interpret_forward(code: i32, stderr: &str) -> std::result::Result<(), String> {
    if code == 0 {
        Ok(())
    } else {
        Err(failure_reason(stderr))
    }
}

/// Invariant 4 lives here.
///
/// Reconciliation cancels speculatively, so the two ways there can be nothing
/// to cancel are both success: the master does not have this forward
/// (`port not found`), or the master is gone entirely, in which case neither
/// is any of its forwards.
pub fn interpret_cancel(code: i32, stderr: &str) -> CancelOutcome {
    if code == 0 {
        return CancelOutcome::Cancelled;
    }
    if master_socket_missing(stderr) {
        return CancelOutcome::NothingToCancel;
    }
    let lowered = stderr.to_ascii_lowercase();
    if lowered.contains("port not found") || lowered.contains("requested forward not found") {
        return CancelOutcome::NothingToCancel;
    }
    CancelOutcome::Failed
}

/// The one line of an ssh failure worth showing a user.
///
/// OpenSSH reports a mux failure twice: `mux_client_forward: forwarding request
/// failed: port not found` carries the reason, and the `fatal` after it
/// (`master cancel forward request failed`) only names the wrapper. Keep the
/// reason and drop the `func_name: ` prefix OpenSSH puts in front of it.
pub fn failure_reason(stderr: &str) -> String {
    const WRAPPERS: [&str; 4] = [
        "master forward request failed",
        "master cancel forward request failed",
        "master alive check failed",
        "master hello exchange failed",
    ];
    let mut wrapper: Option<&str> = None;
    for line in stderr.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if WRAPPERS.contains(&line) {
            wrapper.get_or_insert(line);
            continue;
        }
        // A truncated line can leave nothing after the colon; then the whole
        // line, prefix and all, still beats an empty error message.
        return match line.split_once("request failed: ").map(|(_, r)| r.trim()) {
            Some(reason) if !reason.is_empty() => reason.to_string(),
            _ => line.to_string(),
        };
    }
    wrapper.unwrap_or("no diagnostic on stderr").to_string()
}

/// Where the master's own output goes while it is still attached to us. Not a
/// socket, so no length limit applies.
fn master_log_path(control_path: &Path) -> PathBuf {
    let mut name = control_path.as_os_str().to_os_string();
    name.push(".start.log");
    PathBuf::from(name)
}

/// Length-check before anything is spawned (invariant 8).
///
/// Routed through [`crate::ssh::control_socket_path`] so the limit keeps living
/// in one place, then re-checked directly because a caller may hand us a name
/// longer than the `<host>.ctl` that function would have built.
fn verify_control_path(path: &Path) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new(""));
    let host = path.file_stem().unwrap_or_default().to_string_lossy();
    crate::ssh::control_socket_path(dir, &host)?;
    let len = path.as_os_str().len();
    if len > MAX_CONTROL_PATH_LEN {
        return Err(SshError::ControlPathTooLong {
            path: path.to_string_lossy().into_owned(),
            len,
            max: MAX_CONTROL_PATH_LEN,
        });
    }
    Ok(())
}

fn io_error(message: String) -> SshError {
    SshError::Spawn(io::Error::other(message))
}

/// Create the control-socket directory at 0700 and refuse one that is not ours.
///
/// Runs before the socket is *used*, not only before it is created: a socket
/// somebody else planted in a directory they own is a live master of theirs,
/// and `ssh -O forward` does not check that the socket's peer is the host named
/// on the command line. Every forward would then be opened on their connection.
fn ensure_control_dir(control_path: &Path) -> Result<()> {
    let Some(dir) = control_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
    else {
        return Ok(());
    };
    DirBuilder::new()
        .recursive(true)
        .mode(CONTROL_DIR_MODE)
        .create(dir)
        .map_err(|e| io_error(format!("cannot create {}: {e}", dir.display())))?;
    let metadata = fs::metadata(dir)
        .map_err(|e| io_error(format!("cannot inspect {}: {e}", dir.display())))?;
    // SAFETY: geteuid reads a process property and cannot fail.
    let me = unsafe { libc::geteuid() };
    if metadata.uid() != me {
        return Err(io_error(format!(
            "refusing to use {}: it belongs to uid {}, not to uid {me}",
            dir.display(),
            metadata.uid()
        )));
    }
    // A recursive create leaves an existing directory's mode alone, and this
    // directory may predate the mode being enforced.
    let perms = metadata.permissions();
    if perms.mode() & 0o777 != CONTROL_DIR_MODE {
        let mut tightened = perms;
        tightened.set_mode(CONTROL_DIR_MODE);
        fs::set_permissions(dir, tightened)
            .map_err(|e| io_error(format!("cannot restrict {} to 0700: {e}", dir.display())))?;
    }
    Ok(())
}

/// Remove a control socket with nothing behind it.
///
/// OpenSSH does not fail on a dead ControlPath: it silently falls back to a
/// standalone connection, which with `-f -N` leaks one process per attempt
/// while every later `-O` command keeps talking to the dead socket. Forwards
/// then appear to work and vanish on the next reconnect.
///
/// Anything that is not a socket is left alone. Deleting whatever happens to
/// sit at that path is how a config typo turns into data loss.
fn remove_stale_control_socket(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(io_error(format!(
                "cannot inspect control socket {}: {e}",
                path.display()
            )))
        }
    };
    if !metadata.file_type().is_socket() {
        return Err(io_error(format!(
            "refusing to replace control path {}: it is not a socket",
            path.display()
        )));
    }
    fs::remove_file(path).map_err(|e| {
        io_error(format!(
            "cannot remove stale control socket {}: {e}",
            path.display()
        ))
    })
}

/// [`SshRunner`] backed by a real `ssh` process.
#[derive(Debug, Clone, Default)]
pub struct SshProcess<S = SystemSsh> {
    spawner: S,
}

impl SshProcess<SystemSsh> {
    pub fn new() -> Self {
        Self {
            spawner: SystemSsh::new(),
        }
    }
}

impl<S: SshSpawner> SshProcess<S> {
    pub fn with_spawner(spawner: S) -> Self {
        Self { spawner }
    }

    fn failed(operation: &'static str, target: &str, code: i32, stderr: String) -> SshError {
        SshError::Command {
            operation,
            target: target.to_string(),
            code,
            stderr,
        }
    }
}

impl<S: SshSpawner> SshRunner for SshProcess<S> {
    fn ensure_master(&self, target: &str, control_path: &Path) -> Result<()> {
        verify_control_path(control_path)?;
        // Before the check, not after: a master answering `-O check` from a
        // directory somebody else owns is somebody else's connection, and
        // returning early on it would skip this exact test.
        ensure_control_dir(control_path)?;
        if self.check_master(target, control_path)? == MasterState::Running {
            return Ok(());
        }
        remove_stale_control_socket(control_path)?;

        let log = master_log_path(control_path);
        let out = self
            .spawner
            .run_forking(&master_argv(target, control_path), &log)?;
        if out.code != 0 {
            return Err(Self::failed(
                "master",
                target,
                out.code,
                failure_reason(&out.stderr),
            ));
        }
        // Exiting 0 is not proof of a usable socket: ssh reports success for
        // the standalone fallback too, and then no `-O forward` ever lands.
        if self.check_master(target, control_path)? == MasterState::Absent {
            return Err(Self::failed(
                "master",
                target,
                out.code,
                format!(
                    "started but left no control socket at {}",
                    control_path.display()
                ),
            ));
        }
        Ok(())
    }

    fn check_master(&self, target: &str, control_path: &Path) -> Result<MasterState> {
        verify_control_path(control_path)?;
        let out = self.spawner.run(&check_argv(target, control_path))?;
        Ok(interpret_check(out.code, &out.stderr))
    }

    fn add_forward(&self, target: &str, control_path: &Path, spec: &ForwardSpec) -> Result<()> {
        verify_control_path(control_path)?;
        let argv = forward_argv(target, control_path, &spec.forward_arg());
        let out = self.spawner.run(&argv)?;
        interpret_forward(out.code, &out.stderr)
            .map_err(|reason| Self::failed("forward", target, out.code, reason))
    }

    fn cancel_forward(&self, target: &str, control_path: &Path, spec: &ForwardSpec) -> Result<()> {
        verify_control_path(control_path)?;
        let argv = cancel_argv(target, control_path, &spec.forward_arg());
        let out = self.spawner.run(&argv)?;
        match interpret_cancel(out.code, &out.stderr) {
            CancelOutcome::Cancelled | CancelOutcome::NothingToCancel => Ok(()),
            CancelOutcome::Failed => Err(Self::failed(
                "cancel",
                target,
                out.code,
                failure_reason(&out.stderr),
            )),
        }
    }

    fn exit_master(&self, target: &str, control_path: &Path) -> Result<()> {
        verify_control_path(control_path)?;
        let out = self.spawner.run(&exit_argv(target, control_path))?;
        // Asking a master that is already gone to exit is the outcome we
        // wanted, not a failure.
        if out.code == 0 || master_socket_missing(&out.stderr) {
            return Ok(());
        }
        Err(Self::failed(
            "exit",
            target,
            out.code,
            failure_reason(&out.stderr),
        ))
    }

    fn exec(&self, target: &str, control_path: &Path, command: &str) -> Result<String> {
        verify_control_path(control_path)?;
        let out = self
            .spawner
            .run(&exec_argv(target, control_path, command))?;
        if out.code != 0 {
            return Err(Self::failed(
                "exec",
                target,
                out.code,
                failure_reason(&out.stderr),
            ));
        }
        Ok(out.stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Origin;
    use std::collections::VecDeque;
    use std::os::unix::net::UnixListener;
    use std::sync::Mutex;

    /// Scripted `ssh`. Records every argv and answers from a queue, so a test
    /// pins both what was run and what was made of the answer.
    #[derive(Default)]
    struct FakeSsh {
        replies: Mutex<VecDeque<SshOutput>>,
        calls: Mutex<Vec<Vec<String>>>,
        logs: Mutex<Vec<PathBuf>>,
    }

    impl FakeSsh {
        fn with(replies: Vec<SshOutput>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                calls: Mutex::new(Vec::new()),
                logs: Mutex::new(Vec::new()),
            }
        }

        fn next(&self, argv: &[String]) -> io::Result<SshOutput> {
            self.calls.lock().expect("fake lock").push(argv.to_vec());
            Ok(self
                .replies
                .lock()
                .expect("fake lock")
                .pop_front()
                .unwrap_or(SshOutput {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }))
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().expect("fake lock").clone()
        }

        fn logs(&self) -> Vec<PathBuf> {
            self.logs.lock().expect("fake lock").clone()
        }
    }

    impl SshSpawner for FakeSsh {
        fn run(&self, argv: &[String]) -> io::Result<SshOutput> {
            self.next(argv)
        }

        fn run_forking(&self, argv: &[String], log_path: &Path) -> io::Result<SshOutput> {
            self.logs
                .lock()
                .expect("fake lock")
                .push(log_path.to_path_buf());
            self.next(argv)
        }
    }

    fn ok(stderr: &str) -> SshOutput {
        SshOutput {
            code: 0,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    fn fail(stderr: &str) -> SshOutput {
        SshOutput {
            code: 255,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    /// Real OpenSSH strings, from `strings $(command -v ssh)` on OpenSSH 10.0.
    const MASTER_RUNNING: &str = "Master running (pid=64160)";
    const NO_SOCKET: &str =
        "Control socket connect(/tmp/hpf/buildbox.ctl): No such file or directory";
    const CANCEL_UNKNOWN: &str = "mux_client_forward: forwarding request failed: port not found\nmaster cancel forward request failed";
    const PORT_BUSY: &str = "cannot listen to port: 19999";

    fn spec() -> ForwardSpec {
        ForwardSpec {
            host: "buildbox".into(),
            remote_port: 5173,
            local_port: 5173,
            remote_bind: "127.0.0.1".into(),
            origin: Origin::Explicit,
            pane_id: None,
        }
    }

    fn ctl() -> PathBuf {
        PathBuf::from("/tmp/hpf/buildbox.ctl")
    }

    #[test]
    fn master_starts_detached_with_prompts_disabled() {
        assert_eq!(
            master_argv("buildbox", &ctl()),
            [
                "-M",
                "-N",
                "-f",
                "-o",
                "ControlPath=/tmp/hpf/buildbox.ctl",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPersist=yes",
                "-o",
                "BatchMode=yes",
                "-o",
                "GatewayPorts=no",
                "buildbox",
            ]
        );
    }

    #[test]
    fn the_master_never_inherits_a_wildcard_bind_from_the_users_config() {
        // `GatewayPorts yes` in ~/.ssh/config would otherwise put every
        // forwarded dev server on a routable address, which SECURITY.md
        // promises without a caveat cannot happen.
        assert!(
            master_argv("buildbox", &ctl()).contains(&"GatewayPorts=no".to_string()),
            "the master must overrule the user's GatewayPorts setting"
        );
    }

    #[test]
    fn a_forward_names_the_loopback_bind_address_it_wants() {
        let argv = forward_argv("buildbox", &ctl(), &spec().forward_arg());
        assert_eq!(argv[4], "-L");
        assert!(
            argv[5].starts_with("localhost:"),
            "the -L spec must not leave the bind address to ssh: {}",
            argv[5]
        );
    }

    #[test]
    fn mux_commands_address_the_existing_socket() {
        assert_eq!(
            check_argv("buildbox", &ctl()),
            ["-S", "/tmp/hpf/buildbox.ctl", "-O", "check", "buildbox"]
        );
        assert_eq!(
            exit_argv("buildbox", &ctl()),
            ["-S", "/tmp/hpf/buildbox.ctl", "-O", "exit", "buildbox"]
        );
    }

    #[test]
    fn cancel_repeats_the_forward_argv_verbatim() {
        // A one-byte difference here makes cancel match nothing and still
        // report success, leaving a forward nobody can find again.
        let arg = spec().forward_arg();
        let mut forward = forward_argv("buildbox", &ctl(), &arg);
        let cancel = cancel_argv("buildbox", &ctl(), &arg);
        assert_eq!(forward[3], "forward");
        assert_eq!(cancel[3], "cancel");
        forward[3] = "cancel".to_string();
        assert_eq!(forward, cancel);
        assert_eq!(cancel[5], "localhost:5173:127.0.0.1:5173");
    }

    #[test]
    fn exec_passes_the_remote_command_as_a_single_argument() {
        let argv = exec_argv("buildbox", &ctl(), "ss -ltnpH | head -n 5");
        assert_eq!(
            argv,
            [
                "-S",
                "/tmp/hpf/buildbox.ctl",
                "-o",
                "BatchMode=yes",
                "buildbox",
                "ss -ltnpH | head -n 5",
            ]
        );
        // The shell metacharacters stay inside one argv element; nothing here
        // ever reaches a local shell.
        assert_eq!(argv.len(), 6);
    }

    #[test]
    fn a_target_that_looks_like_an_option_is_still_one_argument() {
        let argv = check_argv("-oProxyCommand=touch /tmp/pwned", &ctl());
        assert_eq!(argv.len(), 5);
        assert_eq!(argv[4], "-oProxyCommand=touch /tmp/pwned");
    }

    #[test]
    fn check_reads_a_live_master_from_exit_zero() {
        assert_eq!(interpret_check(0, MASTER_RUNNING), MasterState::Running);
        assert_eq!(interpret_check(0, ""), MasterState::Running);
    }

    #[test]
    fn check_reads_a_missing_socket_as_absent_not_as_an_error() {
        assert_eq!(interpret_check(255, NO_SOCKET), MasterState::Absent);
        assert_eq!(interpret_check(1, ""), MasterState::Absent);
    }

    #[test]
    fn cancelling_a_forward_the_master_never_had_is_a_no_op() {
        assert_eq!(
            interpret_cancel(255, CANCEL_UNKNOWN),
            CancelOutcome::NothingToCancel
        );
    }

    #[test]
    fn cancelling_on_a_dead_master_is_a_no_op() {
        assert_eq!(
            interpret_cancel(255, NO_SOCKET),
            CancelOutcome::NothingToCancel
        );
    }

    #[test]
    fn cancel_success_is_not_confused_by_its_own_chatter() {
        assert_eq!(
            interpret_cancel(0, "Canceled forwarding."),
            CancelOutcome::Cancelled
        );
    }

    #[test]
    fn a_refused_cancel_is_a_real_failure() {
        let stderr = "Master refused forwarding request: administratively prohibited";
        assert_eq!(interpret_cancel(255, stderr), CancelOutcome::Failed);
        assert_eq!(interpret_cancel(255, ""), CancelOutcome::Failed);
    }

    #[test]
    fn forward_success_is_not_confused_by_its_own_chatter() {
        assert_eq!(interpret_forward(0, "Forwarding port."), Ok(()));
    }

    #[test]
    fn a_busy_local_port_surfaces_the_port_number() {
        let stderr = format!("{PORT_BUSY}\nmux_client_forward: forwarding request failed: port forwarding failed\nmaster forward request failed");
        assert_eq!(interpret_forward(255, &stderr), Err(PORT_BUSY.to_string()));
    }

    #[test]
    fn failure_reason_keeps_the_cause_and_drops_the_wrapper() {
        assert_eq!(failure_reason(CANCEL_UNKNOWN), "port not found");
    }

    #[test]
    fn failure_reason_falls_back_to_the_wrapper_when_that_is_all_there_is() {
        assert_eq!(
            failure_reason("master forward request failed\n"),
            "master forward request failed"
        );
    }

    #[test]
    fn failure_reason_survives_empty_and_ragged_stderr() {
        assert_eq!(failure_reason(""), "no diagnostic on stderr");
        assert_eq!(failure_reason("\n  \n\t\n"), "no diagnostic on stderr");
        // Truncated mid-line: no trailing newline, no reason after the colon.
        assert_eq!(
            failure_reason("mux_client_forward: forwarding request fail"),
            "mux_client_forward: forwarding request fail"
        );
        // Nothing after the colon: keep the line rather than report nothing.
        assert_eq!(failure_reason("x: request failed: "), "x: request failed:");
        assert_eq!(failure_reason("x: request failed: y"), "y");
    }

    #[test]
    fn master_socket_missing_only_matches_the_socket_error() {
        assert!(master_socket_missing(NO_SOCKET));
        assert!(!master_socket_missing(MASTER_RUNNING));
        assert!(!master_socket_missing(""));
    }

    #[test]
    fn an_already_running_master_is_not_started_again() {
        let fake = FakeSsh::with(vec![ok(MASTER_RUNNING)]);
        let ssh = SshProcess::with_spawner(fake);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("buildbox.ctl");

        ssh.ensure_master("buildbox", &path)
            .expect("idempotent when already up");

        let calls = ssh.spawner.calls();
        assert_eq!(calls.len(), 1, "only the check should run: {calls:?}");
        assert!(calls[0].contains(&"check".to_string()));
        assert!(!path.exists(), "nothing should have created a socket");
    }

    #[test]
    fn a_new_master_is_verified_before_it_is_trusted() {
        let fake = FakeSsh::with(vec![fail(NO_SOCKET), ok(""), ok(MASTER_RUNNING)]);
        let ssh = SshProcess::with_spawner(fake);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("buildbox.ctl");

        ssh.ensure_master("buildbox", &path)
            .expect("master should come up");

        let calls = ssh.spawner.calls();
        assert_eq!(calls.len(), 3, "check, start, re-check: {calls:?}");
        assert_eq!(calls[1][0], "-M");
        assert!(calls[2].contains(&"check".to_string()));
        // `-f` must never be handed a pipe it will hold open for its whole life.
        assert_eq!(ssh.spawner.logs(), [master_log_path(&path)]);
    }

    #[test]
    fn a_master_that_leaves_no_socket_fails_instead_of_looking_healthy() {
        // ssh exits 0 for the standalone fallback, so only the re-check knows.
        let fake = FakeSsh::with(vec![fail(NO_SOCKET), ok(""), fail(NO_SOCKET)]);
        let ssh = SshProcess::with_spawner(fake);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("buildbox.ctl");

        let err = ssh
            .ensure_master("buildbox", &path)
            .expect_err("must not report success");
        assert!(err.to_string().contains("no control socket"), "{err}");
    }

    #[test]
    fn a_failed_master_start_reports_the_ssh_reason() {
        let fake = FakeSsh::with(vec![
            fail(NO_SOCKET),
            fail("buildbox: Permission denied (publickey)."),
        ]);
        let ssh = SshProcess::with_spawner(fake);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("buildbox.ctl");

        let err = ssh
            .ensure_master("buildbox", &path)
            .expect_err("must not report success");
        assert!(err.to_string().contains("Permission denied"), "{err}");
    }

    #[test]
    fn a_dead_control_socket_is_removed_before_a_master_is_started() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("buildbox.ctl");
        let listener = UnixListener::bind(&path).expect("bind stale socket");
        drop(listener);
        assert!(path.exists());

        let fake = FakeSsh::with(vec![fail(NO_SOCKET), ok(""), ok(MASTER_RUNNING)]);
        let ssh = SshProcess::with_spawner(fake);
        ssh.ensure_master("buildbox", &path)
            .expect("stale socket should not block startup");

        assert!(
            !path.exists(),
            "the dead socket must be gone before ssh -f runs"
        );
    }

    #[test]
    fn a_control_path_that_is_not_a_socket_is_never_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("buildbox.ctl");
        fs::write(&path, "someone else's file").expect("write");

        let fake = FakeSsh::with(vec![fail(NO_SOCKET)]);
        let ssh = SshProcess::with_spawner(fake);
        let err = ssh
            .ensure_master("buildbox", &path)
            .expect_err("must refuse");

        assert!(err.to_string().contains("not a socket"), "{err}");
        assert_eq!(
            fs::read_to_string(&path).expect("still there"),
            "someone else's file"
        );
    }

    #[test]
    fn the_control_directory_is_private() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("state/herdr-portfwd");
        let path = nested.join("buildbox.ctl");

        let fake = FakeSsh::with(vec![fail(NO_SOCKET), ok(""), ok(MASTER_RUNNING)]);
        let ssh = SshProcess::with_spawner(fake);
        ssh.ensure_master("buildbox", &path)
            .expect("master should come up");

        let mode = fs::metadata(&nested)
            .expect("dir exists")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "control dir must not be readable by others"
        );
    }

    #[test]
    fn an_existing_world_writable_control_directory_is_tightened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loose = dir.path().join("loose");
        fs::create_dir(&loose).expect("create");
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o777)).expect("chmod");

        ensure_control_dir(&loose.join("buildbox.ctl")).expect("should tighten");

        let mode = fs::metadata(&loose)
            .expect("dir exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn a_running_master_does_not_excuse_the_directory_it_lives_in() {
        // The dangerous shape: somebody else's live socket sitting in a
        // world-writable directory. `ssh -O forward` does not check that the
        // socket's peer is the host on the command line, so returning early on
        // "master running" would put every forward on their connection.
        let dir = tempfile::tempdir().expect("tempdir");
        let loose = dir.path().join("loose");
        fs::create_dir(&loose).expect("create");
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o777)).expect("chmod");

        let fake = FakeSsh::with(vec![ok(MASTER_RUNNING)]);
        let ssh = SshProcess::with_spawner(fake);
        ssh.ensure_master("buildbox", &loose.join("buildbox.ctl"))
            .expect("a live master is still a live master");

        let mode = fs::metadata(&loose)
            .expect("dir exists")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "the directory was never checked because a master answered first"
        );
    }

    #[test]
    fn concurrent_callers_all_get_the_control_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state/herdr-portfwd/buildbox.ctl");
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| ensure_control_dir(&path)))
                .collect();
            for handle in handles {
                handle
                    .join()
                    .expect("thread")
                    .expect("racing creates must all succeed");
            }
        });
        assert!(path.parent().is_some_and(Path::is_dir));
    }

    #[test]
    fn no_operation_spawns_ssh_with_an_overlong_control_path() {
        // Invariant 8: the length check happens before anything is spawned,
        // because connect(2) on an overlong sun_path fails with an error that
        // never mentions the length.
        let long = PathBuf::from("/tmp")
            .join("d".repeat(MAX_CONTROL_PATH_LEN))
            .join("h.ctl");
        let fake = FakeSsh::with(Vec::new());
        let ssh = SshProcess::with_spawner(fake);

        let errs: Vec<SshError> = vec![
            ssh.ensure_master("h", &long).expect_err("ensure_master"),
            ssh.check_master("h", &long)
                .map(|_| ())
                .expect_err("check_master"),
            ssh.add_forward("h", &long, &spec())
                .expect_err("add_forward"),
            ssh.cancel_forward("h", &long, &spec())
                .expect_err("cancel_forward"),
            ssh.exit_master("h", &long).expect_err("exit_master"),
            ssh.exec("h", &long, "true").map(|_| ()).expect_err("exec"),
        ];
        for err in errs {
            assert!(matches!(err, SshError::ControlPathTooLong { .. }), "{err}");
        }
        assert!(ssh.spawner.calls().is_empty(), "nothing may be spawned");
    }

    #[test]
    fn a_control_path_exactly_at_the_limit_is_allowed() {
        let name = "h.ctl";
        let dir = "/".to_string() + &"d".repeat(MAX_CONTROL_PATH_LEN - name.len() - 2);
        let path = PathBuf::from(dir).join(name);
        assert_eq!(path.as_os_str().len(), MAX_CONTROL_PATH_LEN);
        verify_control_path(&path).expect("at the limit is still under it");

        let over = PathBuf::from(path.to_string_lossy().into_owned() + "x");
        verify_control_path(&over).expect_err("one byte past the limit is rejected");
    }

    #[test]
    fn cancelling_a_forward_that_was_never_created_succeeds() {
        // Invariant 4, end to end through the runner.
        let fake = FakeSsh::with(vec![fail(CANCEL_UNKNOWN)]);
        let ssh = SshProcess::with_spawner(fake);
        ssh.cancel_forward("buildbox", &ctl(), &spec())
            .expect("a speculative cancel must not fail");
    }

    #[test]
    fn a_cancel_that_really_failed_is_reported() {
        let fake = FakeSsh::with(vec![fail("Master refused forwarding request: nope")]);
        let ssh = SshProcess::with_spawner(fake);
        let err = ssh
            .cancel_forward("buildbox", &ctl(), &spec())
            .expect_err("must surface");
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn add_forward_reports_the_busy_port() {
        let fake = FakeSsh::with(vec![fail(PORT_BUSY)]);
        let ssh = SshProcess::with_spawner(fake);
        let err = ssh
            .add_forward("buildbox", &ctl(), &spec())
            .expect_err("must surface");
        assert!(err.to_string().contains("19999"), "{err}");
        assert!(
            err.to_string().contains("buildbox"),
            "the target belongs in the error: {err}"
        );
    }

    #[test]
    fn exiting_a_master_that_is_already_gone_succeeds() {
        let fake = FakeSsh::with(vec![fail(NO_SOCKET)]);
        let ssh = SshProcess::with_spawner(fake);
        ssh.exit_master("buildbox", &ctl())
            .expect("already gone is the wanted outcome");
    }

    #[test]
    fn a_refused_exit_is_reported() {
        let fake = FakeSsh::with(vec![fail("Master refused termination request: busy")]);
        let ssh = SshProcess::with_spawner(fake);
        let err = ssh
            .exit_master("buildbox", &ctl())
            .expect_err("must surface");
        assert!(err.to_string().contains("busy"), "{err}");
    }

    #[test]
    fn exec_returns_stdout_and_leaves_stderr_out_of_it() {
        let fake = FakeSsh::with(vec![SshOutput {
            code: 0,
            stdout: "LISTEN 0 511 127.0.0.1:5173\n".to_string(),
            stderr: "Warning: Permanently added 'buildbox'\n".to_string(),
        }]);
        let ssh = SshProcess::with_spawner(fake);
        let out = ssh.exec("buildbox", &ctl(), "ss -ltnpH").expect("exec");
        assert_eq!(out, "LISTEN 0 511 127.0.0.1:5173\n");
    }

    #[test]
    fn a_failed_exec_does_not_pass_off_partial_output_as_a_result() {
        let fake = FakeSsh::with(vec![SshOutput {
            code: 127,
            stdout: "partial".to_string(),
            stderr: "bash: ss: command not found".to_string(),
        }]);
        let ssh = SshProcess::with_spawner(fake);
        let err = ssh
            .exec("buildbox", &ctl(), "ss -ltnpH")
            .expect_err("must surface");
        assert!(err.to_string().contains("command not found"), "{err}");
    }

    #[test]
    fn a_spawn_failure_is_not_mistaken_for_an_ssh_answer() {
        struct Broken;
        impl SshSpawner for Broken {
            fn run(&self, _argv: &[String]) -> io::Result<SshOutput> {
                Err(io::Error::new(io::ErrorKind::NotFound, "ssh not installed"))
            }
            fn run_forking(&self, _argv: &[String], _log: &Path) -> io::Result<SshOutput> {
                Err(io::Error::new(io::ErrorKind::NotFound, "ssh not installed"))
            }
        }
        let ssh = SshProcess::with_spawner(Broken);
        let err = ssh
            .check_master("buildbox", &ctl())
            .expect_err("must surface");
        assert!(matches!(err, SshError::Spawn(_)), "{err}");
    }

    #[test]
    fn the_master_log_sits_beside_the_socket_and_never_replaces_it() {
        let log = master_log_path(Path::new("/tmp/hpf/web.example.com.ctl"));
        assert_eq!(log, Path::new("/tmp/hpf/web.example.com.ctl.start.log"));
    }

    #[test]
    fn the_real_spawner_reports_exit_code_and_output() {
        // Not ssh: a trivial binary is enough to pin that the spawn layer
        // reads a status and both streams back.
        let echo = SystemSsh::with_bin("/bin/echo");
        let out = echo.run(&["hello".to_string()]).expect("echo should run");
        assert_eq!(out.code, 0);
        assert_eq!(out.stdout, "hello\n");
        assert_eq!(out.stderr, "");

        let missing = SystemSsh::with_bin("/nonexistent/ssh");
        assert!(
            missing.run(&[]).is_err(),
            "a missing binary is an io error, not exit 0"
        );
    }

    #[test]
    fn the_forking_spawner_reads_the_log_back_and_unlinks_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("buildbox.ctl.start.log");
        let echo = SystemSsh::with_bin("/bin/echo");

        let out = echo
            .run_forking(&["backgrounded".to_string()], &log)
            .expect("echo should run");

        assert_eq!(out.code, 0);
        assert_eq!(
            out.stderr, "backgrounded\n",
            "output must come back from the file"
        );
        assert!(!log.exists(), "the log must not outlive the call");
    }
}
