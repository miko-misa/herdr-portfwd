//! The real [`crate::herdr::HerdrClient`], driving the `herdr` binary.
//!
//! Two flavors of the same client:
//! - local, running `herdr` directly, used when the plugin and the server are
//!   on the same machine;
//! - remote, running `herdr` on the far side over an existing control master,
//!   used by the local agent to inspect the server it is not running on.
//!
//! Herdr's CLI answers with JSON on stdout and JSON errors on stderr with exit
//! status 1; syntax errors exit 2. Identifiers are read out of the responses,
//! never predicted.

use std::collections::HashMap;
use std::io::{Error as IoError, ErrorKind};
use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;

use crate::herdr::{herdr_bin, HerdrClient, HerdrError, Notification, Result};
use crate::ssh::{SshError, SshRunner};
use crate::types::PaneRef;

/// Labels for the `command` field of [`HerdrError`], and the argv prefix each
/// call uses. Keeping them together makes the error name the thing that ran.
const PANE_LIST: &str = "pane list";
const PANE_PROCESS_INFO: &str = "pane process-info";
const WORKSPACE_LIST: &str = "workspace list";
const NOTIFICATION_SHOW: &str = "notification show";
const PLUGIN_PANE_OPEN: &str = "plugin pane open";

/// The server refused the request; stderr carries a JSON error object.
const EXIT_REFUSED: i32 = 1;
/// The CLI rejected the argv itself. That is this crate's bug, or a `herdr`
/// too old to know the subcommand — never something the user typed.
const EXIT_USAGE: i32 = 2;
/// ssh's own failure code, reachable only through the remote flavor: the
/// remote command never ran, so its stderr is ssh's, not herdr's.
const EXIT_SSH_FAILED: i32 = 255;
/// Stand-in status when a child died from a signal and has no exit code.
const EXIT_SIGNALLED: i32 = -1;

/// One finished `herdr` invocation.
#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// How an argv reaches a `herdr` binary. The two flavors differ only here, so
/// argv construction, status classification and response parsing are shared,
/// and tests drive both with canned output instead of a process.
pub trait HerdrRunner {
    /// Run `herdr <argv>`. A non-zero exit is reported through
    /// [`CommandOutput::status`], not as an error: the status is data that
    /// [`command_failure`] classifies.
    fn run(&self, argv: &[String]) -> Result<CommandOutput>;
}

/// A workspace as far as host resolution cares. herdr-mirror labels a mirrored
/// workspace `"<prefix>: <name>"`, which is how a pane is traced back to the
/// host it actually lives on.
///
/// Not in `types.rs` because nothing outside host resolution needs it; if a
/// second module grows a use for it, it should move.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WorkspaceLabel {
    pub workspace_id: String,
    pub label: String,
}

/// A Herdr client over an injectable runner.
#[derive(Debug)]
pub struct HerdrCli<R> {
    runner: R,
}

impl HerdrCli<LocalHerdr> {
    /// Talk to the server on this machine.
    pub fn local() -> Self {
        Self::new(LocalHerdr::new())
    }
}

impl<S: SshRunner> HerdrCli<RemoteHerdr<S>> {
    /// Talk to the server on `target` over an already-running control master.
    pub fn remote(ssh: S, target: impl Into<String>, control_path: impl Into<PathBuf>) -> Self {
        Self::new(RemoteHerdr::new(ssh, target, control_path))
    }
}

impl<R: HerdrRunner> HerdrCli<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// Every workspace and its label. Not on [`HerdrClient`]: only host
    /// resolution needs labels, and the trait is the slice of Herdr the rest of
    /// the crate depends on.
    pub fn workspace_list(&self) -> Result<Vec<WorkspaceLabel>> {
        let stdout = self.stdout(WORKSPACE_LIST, &workspace_list_argv())?;
        parse_workspace_list(&stdout)
    }

    /// Ask Herdr to open one of this plugin's declared pane entrypoints.
    ///
    /// Not on [`HerdrClient`]: the trait is the slice of Herdr the rest of the
    /// crate composes over, and only the process entry point — the one place
    /// that knows it was started headless with nothing to draw on — needs this.
    ///
    /// A pane opens with no payload worth reading, so an empty echo is success,
    /// exactly as for [`Self::notify`]. A refusal (`ui_busy` while another
    /// modal is up, an entrypoint this Herdr does not know) arrives either as a
    /// non-zero exit or as an error object, and both become an error here
    /// rather than a silently unopened pane.
    pub fn open_pane(&self, plugin_id: &str, entrypoint: &str) -> Result<()> {
        let argv = plugin_pane_open_argv(plugin_id, entrypoint);
        let stdout = self.stdout(PLUGIN_PANE_OPEN, &argv)?;
        if stdout.trim().is_empty() {
            return Ok(());
        }
        result_value(PLUGIN_PANE_OPEN, &stdout).map(|_| ())
    }

    /// Run one subcommand, turning a non-zero status into an error before
    /// anything tries to parse the output.
    fn stdout(&self, command: &str, argv: &[String]) -> Result<String> {
        let output = self.runner.run(argv)?;
        if output.status != 0 {
            return Err(command_failure(command, output.status, &output.stderr));
        }
        Ok(output.stdout)
    }
}

impl<R: HerdrRunner> HerdrClient for HerdrCli<R> {
    fn pane_list(&self) -> Result<Vec<PaneRef>> {
        let stdout = self.stdout(PANE_LIST, &pane_list_argv())?;
        parse_pane_list(&stdout)
    }

    fn pane_list_labelled(&self) -> Result<Vec<PaneRef>> {
        let panes = self.pane_list()?;
        let workspaces = self.workspace_list()?;
        Ok(with_workspace_labels(panes, &workspaces))
    }

    fn shell_pid(&self, pane_id: &str) -> Result<Option<u32>> {
        let stdout = self.stdout(PANE_PROCESS_INFO, &process_info_argv(pane_id))?;
        parse_process_info(&stdout)
    }

    fn notify(&self, notification: &Notification) -> Result<()> {
        let stdout = self.stdout(NOTIFICATION_SHOW, &notification_argv(notification))?;
        // A toast has no payload worth reading, but a refusal can still ride
        // along on stdout. Silence is success: not every herdr version echoes
        // the response, and a missing echo must not fail an offer.
        if stdout.trim().is_empty() {
            return Ok(());
        }
        result_value(NOTIFICATION_SHOW, &stdout).map(|_| ())
    }
}

/// Runs `herdr` on this machine.
#[derive(Debug, Clone)]
pub struct LocalHerdr {
    bin: String,
}

impl LocalHerdr {
    pub fn new() -> Self {
        Self { bin: herdr_bin() }
    }

    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }
}

impl Default for LocalHerdr {
    fn default() -> Self {
        Self::new()
    }
}

impl HerdrRunner for LocalHerdr {
    fn run(&self, argv: &[String]) -> Result<CommandOutput> {
        let output = Command::new(&self.bin).args(argv).output()?;
        Ok(CommandOutput {
            status: output.status.code().unwrap_or(EXIT_SIGNALLED),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Runs `herdr` on the far side of an existing control master.
///
/// The binary path is deliberately *not* [`herdr_bin`]: `HERDR_BIN_PATH`
/// describes the machine the plugin runs on, and the remote's `herdr` can live
/// somewhere else entirely.
#[derive(Debug, Clone)]
pub struct RemoteHerdr<S> {
    ssh: S,
    target: String,
    control_path: PathBuf,
    bin: String,
}

impl<S: SshRunner> RemoteHerdr<S> {
    pub fn new(ssh: S, target: impl Into<String>, control_path: impl Into<PathBuf>) -> Self {
        Self {
            ssh,
            target: target.into(),
            control_path: control_path.into(),
            bin: "herdr".to_string(),
        }
    }

    /// Override the remote binary, for a host where `herdr` is not on `PATH`
    /// for a non-interactive shell.
    pub fn with_bin(mut self, bin: impl Into<String>) -> Self {
        self.bin = bin.into();
        self
    }
}

impl<S: SshRunner> HerdrRunner for RemoteHerdr<S> {
    fn run(&self, argv: &[String]) -> Result<CommandOutput> {
        let command = remote_command_line(&self.bin, argv)?;
        match self.ssh.exec(&self.target, &self.control_path, &command) {
            Ok(stdout) => Ok(CommandOutput {
                status: 0,
                stdout,
                stderr: String::new(),
            }),
            // ssh reports the remote command's own exit status, so herdr's 1
            // and 2 survive the hop and classify exactly as they do locally.
            Err(SshError::Command { code, stderr, .. }) => Ok(CommandOutput {
                status: code,
                stdout: String::new(),
                stderr,
            }),
            Err(SshError::Spawn(err)) => Err(HerdrError::Spawn(err)),
            Err(err @ (SshError::ControlPathTooLong { .. } | SshError::NoControlDir)) => Err(
                HerdrError::Spawn(IoError::new(ErrorKind::InvalidInput, err.to_string())),
            ),
        }
    }
}

/// `herdr pane list`.
pub fn pane_list_argv() -> Vec<String> {
    vec!["pane".to_string(), "list".to_string()]
}

/// `herdr pane process-info --pane <id>`. The pane id always comes from a
/// previous response, never from a guess about how ids are shaped.
pub fn process_info_argv(pane_id: &str) -> Vec<String> {
    vec![
        "pane".to_string(),
        "process-info".to_string(),
        "--pane".to_string(),
        pane_id.to_string(),
    ]
}

/// `herdr workspace list`.
pub fn workspace_list_argv() -> Vec<String> {
    vec!["workspace".to_string(), "list".to_string()]
}

/// `herdr notification show <title> --body <body>`. The title is positional and
/// stays first; the body is passed as one argument however it is spelled.
pub fn notification_argv(notification: &Notification) -> Vec<String> {
    vec![
        "notification".to_string(),
        "show".to_string(),
        notification.title.clone(),
        "--body".to_string(),
        notification.body.clone(),
    ]
}

/// `herdr plugin pane open --plugin <id> --entrypoint <id>`.
///
/// No `--placement`: the manifest already declares one per entrypoint, and
/// overriding it here would give the same pane two shapes depending on which
/// command opened it. Both ids are separate argv words, so neither can be read
/// as an option however it is spelled.
pub fn plugin_pane_open_argv(plugin_id: &str, entrypoint: &str) -> Vec<String> {
    vec![
        "plugin".to_string(),
        "pane".to_string(),
        "open".to_string(),
        "--plugin".to_string(),
        plugin_id.to_string(),
        "--entrypoint".to_string(),
        entrypoint.to_string(),
    ]
}

/// Panes on the server, in the order the response listed them.
pub fn parse_pane_list(stdout: &str) -> Result<Vec<PaneRef>> {
    let result = result_value(PANE_LIST, stdout)?;
    let listing: PaneListResult =
        serde_json::from_value(result).map_err(|source| parse_failure(PANE_LIST, source))?;
    Ok(listing
        .panes
        .into_iter()
        .map(|pane| PaneRef {
            pane_id: pane.pane_id,
            workspace_id: pane.workspace_id,
            // Labels live on the workspace listing, and shell pids on
            // process-info. Both are separate calls; inventing either here
            // would be predicting.
            workspace_label: None,
            agent: pane.agent,
            shell_pid: None,
        })
        .collect())
}

/// A pane's `shell_pid`, when it has one.
pub fn parse_process_info(stdout: &str) -> Result<Option<u32>> {
    let result = result_value(PANE_PROCESS_INFO, stdout)?;
    let info: ProcessInfoResult = serde_json::from_value(result)
        .map_err(|source| parse_failure(PANE_PROCESS_INFO, source))?;
    // pid 0 belongs to the kernel and terminates every ancestry walk, so
    // accepting it as a pane root would attribute the whole host to one pane.
    Ok(info.process_info.shell_pid.filter(|pid| *pid != 0))
}

/// Workspaces and their labels, in response order.
pub fn parse_workspace_list(stdout: &str) -> Result<Vec<WorkspaceLabel>> {
    let result = result_value(WORKSPACE_LIST, stdout)?;
    let listing: WorkspaceListResult =
        serde_json::from_value(result).map_err(|source| parse_failure(WORKSPACE_LIST, source))?;
    Ok(listing.workspaces)
}

/// Fill in [`PaneRef::workspace_label`] from a workspace listing. A pane whose
/// workspace is absent keeps `None` rather than acquiring a made-up label.
pub fn with_workspace_labels(panes: Vec<PaneRef>, workspaces: &[WorkspaceLabel]) -> Vec<PaneRef> {
    let mut labels: HashMap<&str, &str> = HashMap::with_capacity(workspaces.len());
    for workspace in workspaces {
        // A duplicated id is a malformed response; keep the first sighting
        // rather than letting a later one rewrite it.
        labels
            .entry(workspace.workspace_id.as_str())
            .or_insert(workspace.label.as_str());
    }
    panes
        .into_iter()
        .map(|mut pane| {
            pane.workspace_label = labels
                .get(pane.workspace_id.as_str())
                .map(|label| (*label).to_string());
            pane
        })
        .collect()
}

/// Exit status and stderr to an error a reader can act on.
///
/// Herdr's statuses are the whole signal: 1 means the server got the request
/// and refused it, and stderr holds the JSON error; 2 means the CLI never
/// accepted the argv, which points at this crate or at a version mismatch, not
/// at the request.
pub fn command_failure(command: &str, code: i32, stderr: &str) -> HerdrError {
    let trimmed = stderr.trim();
    let detail = match code {
        EXIT_REFUSED => match serde_json::from_str::<Value>(trimmed)
            .ok()
            .as_ref()
            .and_then(|value| value.get("error").map(describe_error_body))
        {
            Some(body) => format!("herdr refused the request: {body}"),
            None => format!("herdr refused the request: {}", or_no_output(trimmed)),
        },
        EXIT_USAGE => format!(
            "herdr rejected this command line; the installed herdr may not have \
             this subcommand: {}",
            or_no_output(trimmed)
        ),
        EXIT_SSH_FAILED => format!(
            "ssh could not run herdr on the remote: {}",
            or_no_output(trimmed)
        ),
        _ => or_no_output(trimmed).to_string(),
    };
    HerdrError::Command {
        command: command.to_string(),
        code,
        stderr: detail,
    }
}

#[derive(Debug, Deserialize)]
struct Envelope {
    result: Value,
}

#[derive(Debug, Deserialize)]
struct PaneListResult {
    panes: Vec<PaneEntry>,
}

#[derive(Debug, Deserialize)]
struct PaneEntry {
    pane_id: String,
    workspace_id: String,
    #[serde(default)]
    agent: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProcessInfoResult {
    process_info: ProcessInfo,
}

#[derive(Debug, Deserialize)]
struct ProcessInfo {
    #[serde(default)]
    shell_pid: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceListResult {
    workspaces: Vec<WorkspaceLabel>,
}

/// The `result` payload of one response.
///
/// Herdr prints exactly one JSON object. A refusal has an `error` member and no
/// `result`, and can reach stdout even on a zero exit, so it is checked before
/// the shape is trusted. Unknown members anywhere are ignored: this crate reads
/// a handful of fields out of a growing API.
fn result_value(command: &str, stdout: &str) -> Result<Value> {
    let value: Value =
        serde_json::from_str(stdout).map_err(|source| parse_failure(command, source))?;
    if let Some(error) = value.get("error") {
        return Err(HerdrError::Command {
            command: command.to_string(),
            code: EXIT_REFUSED,
            stderr: format!("herdr refused the request: {}", describe_error_body(error)),
        });
    }
    serde_json::from_value::<Envelope>(value)
        .map(|envelope| envelope.result)
        .map_err(|source| parse_failure(command, source))
}

fn parse_failure(command: &str, source: serde_json::Error) -> HerdrError {
    HerdrError::Parse {
        command: command.to_string(),
        source,
    }
}

/// `pane_not_found: no such pane` reads better in a log line than the raw
/// object, but an unfamiliar shape is passed through rather than dropped.
fn describe_error_body(error: &Value) -> String {
    let code = error.get("code").and_then(Value::as_str);
    let message = error.get("message").and_then(Value::as_str);
    match (code, message) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code.to_string(),
        (None, Some(message)) => message.to_string(),
        (None, None) => error.to_string(),
    }
}

fn or_no_output(stderr: &str) -> &str {
    if stderr.is_empty() {
        "no output on stderr"
    } else {
        stderr
    }
}

/// ssh hands the remote command to a shell, so the argv has to survive exactly
/// one round of shell parsing. Quoting every word is what keeps rule "commands
/// are argv arrays" true across the hop.
fn remote_command_line(bin: &str, argv: &[String]) -> Result<String> {
    let mut line = String::new();
    for word in std::iter::once(bin).chain(argv.iter().map(String::as_str)) {
        if word.contains('\0') {
            return Err(HerdrError::Spawn(IoError::new(
                ErrorKind::InvalidInput,
                "herdr argument contains a NUL byte",
            )));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&shell_quote(word));
    }
    Ok(line)
}

/// Single-quote a word for a POSIX shell. Inside single quotes every byte is
/// literal except the quote itself, which is closed, escaped, and reopened.
fn shell_quote(word: &str) -> String {
    let mut quoted = String::with_capacity(word.len() + 2);
    quoted.push('\'');
    for ch in word.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::ssh::MasterState;
    use crate::types::ForwardSpec;

    const PANE_LIST_SAMPLE: &str = r#"{"id":"cli:pane:list","result":{"panes":[{"agent":"claude","agent_status":"idle","cwd":"/Users/hiroyuki/projects/scholion","focused":true,"pane_id":"w1:p1","tab_id":"w1:t1","terminal_title":"...","workspace_id":"w1"}],"type":"pane_list"}}"#;

    const PROCESS_INFO_SAMPLE: &str = r#"{"id":"cli:pane:process_info","result":{"process_info":{"foreground_process_group_id":61788,"foreground_processes":[{"argv":["claude"],"argv0":"claude","cmdline":"claude","cwd":"/Users/x/p","name":"2.1.226","pid":61788}],"pane_id":"wG:p1","shell_pid":60416},"type":"pane_process_info"}}"#;

    const WORKSPACE_LIST_SAMPLE: &str = r#"{"id":"cli:workspace:list","result":{"type":"workspace_list","workspaces":[{"workspace_id":"w1","number":1,"label":"mini: scholion","focused":true,"pane_count":2,"tab_count":1,"active_tab_id":"w1:t1","agent_status":"idle"},{"workspace_id":"w2","number":2,"label":"local","focused":false,"pane_count":1,"tab_count":1,"active_tab_id":"w2:t1","agent_status":"idle"}]}}"#;

    const ERROR_OBJECT: &str = r#"{"id":"cli:pane:process_info","error":{"code":"pane_not_found","message":"no pane w9:p9"}}"#;

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<Vec<String>>>,
        outputs: Mutex<VecDeque<CommandOutput>>,
    }

    impl FakeRunner {
        fn new(outputs: Vec<CommandOutput>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                outputs: Mutex::new(outputs.into()),
            }
        }

        fn ok(stdout: &str) -> Self {
            Self::new(vec![CommandOutput {
                status: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            }])
        }

        fn failing(status: i32, stderr: &str) -> Self {
            Self::new(vec![CommandOutput {
                status,
                stdout: String::new(),
                stderr: stderr.to_string(),
            }])
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().expect("fake runner lock").clone()
        }
    }

    impl HerdrRunner for FakeRunner {
        fn run(&self, argv: &[String]) -> Result<CommandOutput> {
            self.calls
                .lock()
                .expect("fake runner lock")
                .push(argv.to_vec());
            let mut outputs = self.outputs.lock().expect("fake runner lock");
            // The last canned output repeats, so a test that calls once and a
            // test that calls from many threads can share a fixture.
            if outputs.len() > 1 {
                Ok(outputs.pop_front().unwrap_or_default())
            } else {
                Ok(outputs.front().cloned().unwrap_or_default())
            }
        }
    }

    #[derive(Clone)]
    enum SshOutcome {
        Stdout(String),
        Failed { code: i32, stderr: String },
        PathTooLong,
        NoSsh,
    }

    struct FakeSsh {
        outcome: SshOutcome,
        execs: Mutex<Vec<(String, PathBuf, String)>>,
    }

    impl FakeSsh {
        fn new(outcome: SshOutcome) -> Self {
            Self {
                outcome,
                execs: Mutex::new(Vec::new()),
            }
        }

        fn execs(&self) -> Vec<(String, PathBuf, String)> {
            self.execs.lock().expect("fake ssh lock").clone()
        }
    }

    impl SshRunner for Arc<FakeSsh> {
        fn ensure_master(&self, _target: &str, _control_path: &Path) -> crate::ssh::Result<()> {
            Ok(())
        }

        fn check_master(
            &self,
            _target: &str,
            _control_path: &Path,
        ) -> crate::ssh::Result<MasterState> {
            Ok(MasterState::Running)
        }

        fn add_forward(
            &self,
            _target: &str,
            _control_path: &Path,
            _spec: &ForwardSpec,
        ) -> crate::ssh::Result<()> {
            Ok(())
        }

        fn cancel_forward(
            &self,
            _target: &str,
            _control_path: &Path,
            _spec: &ForwardSpec,
        ) -> crate::ssh::Result<()> {
            Ok(())
        }

        fn exit_master(&self, _target: &str, _control_path: &Path) -> crate::ssh::Result<()> {
            Ok(())
        }

        fn exec(
            &self,
            target: &str,
            control_path: &Path,
            command: &str,
        ) -> crate::ssh::Result<String> {
            self.execs.lock().expect("fake ssh lock").push((
                target.to_string(),
                control_path.to_path_buf(),
                command.to_string(),
            ));
            match &self.outcome {
                SshOutcome::Stdout(stdout) => Ok(stdout.clone()),
                SshOutcome::Failed { code, stderr } => Err(SshError::Command {
                    operation: "exec",
                    target: target.to_string(),
                    code: *code,
                    stderr: stderr.clone(),
                }),
                SshOutcome::PathTooLong => Err(SshError::ControlPathTooLong {
                    path: control_path.to_string_lossy().into_owned(),
                    len: 200,
                    max: crate::ssh::MAX_CONTROL_PATH_LEN,
                }),
                SshOutcome::NoSsh => Err(SshError::Spawn(IoError::new(
                    ErrorKind::NotFound,
                    "no ssh binary",
                ))),
            }
        }
    }

    fn pane_list_json(panes: &str) -> String {
        format!(r#"{{"id":"cli:pane:list","result":{{"panes":[{panes}],"type":"pane_list"}}}}"#)
    }

    fn process_info_json(body: &str) -> String {
        format!(r#"{{"id":"x","result":{{"process_info":{body},"type":"pane_process_info"}}}}"#)
    }

    // ---- pane list parsing ----

    #[test]
    fn pane_list_reads_ids_from_the_response() {
        let panes = parse_pane_list(PANE_LIST_SAMPLE).expect("the captured sample must parse");
        assert_eq!(
            panes,
            vec![PaneRef {
                pane_id: "w1:p1".into(),
                workspace_id: "w1".into(),
                workspace_label: None,
                agent: Some("claude".into()),
                shell_pid: None,
            }]
        );
    }

    #[test]
    fn pane_without_an_agent_is_listed_with_no_agent() {
        let absent = pane_list_json(r#"{"pane_id":"w1:p2","workspace_id":"w1"}"#);
        let explicit_null =
            pane_list_json(r#"{"pane_id":"w1:p2","workspace_id":"w1","agent":null}"#);
        for json in [absent, explicit_null] {
            let panes = parse_pane_list(&json).expect("an agentless pane is normal, not an error");
            assert_eq!(panes.len(), 1);
            assert_eq!(panes[0].agent, None);
        }
    }

    #[test]
    fn unknown_fields_never_fail_a_pane_list() {
        let json = r#"{"id":"x","unknown_top":1,"result":{"type":"pane_list","future":true,
            "panes":[{"pane_id":"w1:p1","workspace_id":"w1","invented_field":{"deep":[1,2]}}]}}"#;
        let panes = parse_pane_list(json).expect("unknown fields must be ignored, not fatal");
        assert_eq!(panes[0].pane_id, "w1:p1");
    }

    #[test]
    fn empty_pane_list_is_not_an_error() {
        let panes = parse_pane_list(&pane_list_json("")).expect("zero panes is a valid answer");
        assert!(panes.is_empty());
    }

    #[test]
    fn duplicate_panes_are_reported_as_sent() {
        let json = pane_list_json(
            r#"{"pane_id":"w1:p1","workspace_id":"w1"},{"pane_id":"w1:p1","workspace_id":"w1"}"#,
        );
        let panes = parse_pane_list(&json).expect("a duplicate is the server's problem to explain");
        assert_eq!(panes.len(), 2);
    }

    #[test]
    fn pane_list_error_object_becomes_a_command_error() {
        let err = parse_pane_list(ERROR_OBJECT).expect_err("an error object is not a pane list");
        match err {
            HerdrError::Command {
                command,
                code,
                stderr,
            } => {
                assert_eq!(command, PANE_LIST);
                assert_eq!(code, EXIT_REFUSED);
                assert!(stderr.contains("pane_not_found"), "{stderr}");
                assert!(stderr.contains("no pane w9:p9"), "{stderr}");
            }
            other => panic!("expected a command error, got {other:?}"),
        }
    }

    #[test]
    fn empty_output_is_a_parse_error() {
        for stdout in ["", "   \n"] {
            let err = parse_pane_list(stdout).expect_err("no output cannot be a pane list");
            assert!(matches!(err, HerdrError::Parse { .. }), "{err:?}");
        }
    }

    #[test]
    fn truncated_json_is_a_parse_error() {
        let truncated = &PANE_LIST_SAMPLE[..PANE_LIST_SAMPLE.len() / 2];
        let err = parse_pane_list(truncated).expect_err("a half-written response must not parse");
        assert!(matches!(err, HerdrError::Parse { .. }), "{err:?}");
    }

    #[test]
    fn two_concatenated_objects_are_a_parse_error() {
        let doubled = format!("{PANE_LIST_SAMPLE}\n{PANE_LIST_SAMPLE}");
        let err = parse_pane_list(&doubled).expect_err("herdr answers with exactly one object");
        assert!(matches!(err, HerdrError::Parse { .. }), "{err:?}");
    }

    #[test]
    fn unexpected_result_shapes_are_parse_errors() {
        for stdout in [
            r#"{"id":"x"}"#,
            r#"{"id":"x","result":[]}"#,
            r#"{"id":"x","result":{"type":"pane_list"}}"#,
            r#"{"id":"x","result":{"panes":{}}}"#,
            r#"{"id":"x","result":{"panes":[{"workspace_id":"w1"}]}}"#,
            r#"{"id":"x","result":{"panes":[{"pane_id":7,"workspace_id":"w1"}]}}"#,
            "[]",
            "null",
            "\"pane list\"",
        ] {
            let err = parse_pane_list(stdout)
                .expect_err("an unexpected result shape must not be silently accepted");
            assert!(matches!(err, HerdrError::Parse { .. }), "{stdout}: {err:?}");
        }
    }

    // ---- process info parsing ----

    #[test]
    fn process_info_reads_the_shell_pid() {
        let pid = parse_process_info(PROCESS_INFO_SAMPLE).expect("the captured sample must parse");
        assert_eq!(pid, Some(60416));
    }

    #[test]
    fn process_info_pid_comes_from_the_response_not_the_request() {
        // The pane_id echoed back need not match the one asked for; the pid is
        // read out of whatever came back rather than being cross-checked
        // against a predicted id.
        let json = process_info_json(r#"{"pane_id":"somewhere:else","shell_pid":42}"#);
        assert_eq!(parse_process_info(&json).expect("must parse"), Some(42));
    }

    #[test]
    fn absent_shell_pid_is_none_not_an_error() {
        for body in [
            r#"{"pane_id":"w1:p1"}"#,
            r#"{"pane_id":"w1:p1","shell_pid":null}"#,
        ] {
            let json = process_info_json(body);
            let pid = parse_process_info(&json).expect("a pane can legitimately have no shell");
            assert_eq!(pid, None);
        }
    }

    #[test]
    fn shell_pid_zero_is_rejected_as_a_pane_root() {
        // Every ancestry walk reaches pid 0, so accepting it would attribute
        // every listener on the host to this pane.
        let json = process_info_json(r#"{"pane_id":"w1:p1","shell_pid":0}"#);
        assert_eq!(parse_process_info(&json).expect("must parse"), None);
    }

    #[test]
    fn shell_pid_at_and_past_the_u32_boundary() {
        let at = process_info_json(&format!(r#"{{"shell_pid":{}}}"#, u32::MAX));
        assert_eq!(parse_process_info(&at).expect("must parse"), Some(u32::MAX));

        for past in [
            format!(r#"{{"shell_pid":{}}}"#, u64::from(u32::MAX) + 1),
            r#"{"shell_pid":-1}"#.to_string(),
            r#"{"shell_pid":"60416"}"#.to_string(),
        ] {
            let json = process_info_json(&past);
            let err = parse_process_info(&json).expect_err("a pid outside u32 is not a pid");
            assert!(matches!(err, HerdrError::Parse { .. }), "{err:?}");
        }
    }

    #[test]
    fn process_info_error_object_becomes_a_command_error() {
        let err = parse_process_info(ERROR_OBJECT).expect_err("an error object carries no pid");
        match err {
            HerdrError::Command { command, code, .. } => {
                assert_eq!(command, PANE_PROCESS_INFO);
                assert_eq!(code, EXIT_REFUSED);
            }
            other => panic!("expected a command error, got {other:?}"),
        }
    }

    #[test]
    fn process_info_without_process_info_is_a_parse_error() {
        for stdout in [
            "",
            "{",
            r#"{"id":"x","result":{}}"#,
            r#"{"result":{"process_info":3}}"#,
        ] {
            let err = parse_process_info(stdout).expect_err("no process_info means no answer");
            assert!(matches!(err, HerdrError::Parse { .. }), "{stdout}: {err:?}");
        }
    }

    #[test]
    fn unknown_process_info_fields_are_ignored() {
        let json = process_info_json(
            r#"{"pane_id":"w1:p1","shell_pid":9,"tty":"/dev/ttys004","invented":[{"a":1}]}"#,
        );
        assert_eq!(parse_process_info(&json).expect("must parse"), Some(9));
    }

    // ---- workspace list parsing ----

    #[test]
    fn workspace_list_reads_ids_and_labels() {
        let workspaces =
            parse_workspace_list(WORKSPACE_LIST_SAMPLE).expect("the captured shape must parse");
        assert_eq!(
            workspaces,
            vec![
                WorkspaceLabel {
                    workspace_id: "w1".into(),
                    label: "mini: scholion".into(),
                },
                WorkspaceLabel {
                    workspace_id: "w2".into(),
                    label: "local".into(),
                },
            ]
        );
    }

    #[test]
    fn workspace_list_rejects_entries_without_a_label() {
        let json =
            r#"{"id":"x","result":{"type":"workspace_list","workspaces":[{"workspace_id":"w1"}]}}"#;
        let err = parse_workspace_list(json).expect_err("a labelless workspace resolves no host");
        assert!(matches!(err, HerdrError::Parse { .. }), "{err:?}");
    }

    #[test]
    fn workspace_list_error_object_becomes_a_command_error() {
        let err = parse_workspace_list(ERROR_OBJECT).expect_err("an error object is not a listing");
        match err {
            HerdrError::Command { command, .. } => assert_eq!(command, WORKSPACE_LIST),
            other => panic!("expected a command error, got {other:?}"),
        }
    }

    // ---- joining labels onto panes ----

    fn pane(pane_id: &str, workspace_id: &str) -> PaneRef {
        PaneRef {
            pane_id: pane_id.into(),
            workspace_id: workspace_id.into(),
            workspace_label: None,
            agent: None,
            shell_pid: None,
        }
    }

    fn workspace(workspace_id: &str, label: &str) -> WorkspaceLabel {
        WorkspaceLabel {
            workspace_id: workspace_id.into(),
            label: label.into(),
        }
    }

    #[test]
    fn labels_are_joined_by_workspace_id() {
        let panes = with_workspace_labels(
            vec![pane("w1:p1", "w1"), pane("w2:p1", "w2")],
            &[workspace("w1", "mini: scholion"), workspace("w2", "local")],
        );
        assert_eq!(panes[0].workspace_label.as_deref(), Some("mini: scholion"));
        assert_eq!(panes[1].workspace_label.as_deref(), Some("local"));
    }

    #[test]
    fn a_pane_in_an_unlisted_workspace_gets_no_label() {
        let panes = with_workspace_labels(vec![pane("w9:p1", "w9")], &[workspace("w1", "mini: x")]);
        assert_eq!(panes[0].workspace_label, None);
    }

    #[test]
    fn a_duplicated_workspace_id_keeps_the_first_label() {
        let panes = with_workspace_labels(
            vec![pane("w1:p1", "w1")],
            &[workspace("w1", "first"), workspace("w1", "second")],
        );
        assert_eq!(panes[0].workspace_label.as_deref(), Some("first"));
    }

    // ---- argv ----

    #[test]
    fn pane_list_argv_is_exact() {
        let runner = FakeRunner::ok(PANE_LIST_SAMPLE);
        let client = HerdrCli::new(runner);
        client.pane_list().expect("the canned response must parse");
        assert_eq!(client.runner.calls(), vec![vec!["pane", "list"]]);
    }

    #[test]
    fn process_info_argv_passes_the_pane_id_as_one_argument() {
        let runner = FakeRunner::ok(PROCESS_INFO_SAMPLE);
        let client = HerdrCli::new(runner);
        client
            .shell_pid("w1:p1 with spaces")
            .expect("the canned response must parse");
        assert_eq!(
            client.runner.calls(),
            vec![vec!["pane", "process-info", "--pane", "w1:p1 with spaces"]]
        );
    }

    #[test]
    fn workspace_list_argv_is_exact() {
        let runner = FakeRunner::ok(WORKSPACE_LIST_SAMPLE);
        let client = HerdrCli::new(runner);
        client
            .workspace_list()
            .expect("the canned response must parse");
        assert_eq!(client.runner.calls(), vec![vec!["workspace", "list"]]);
    }

    #[test]
    fn notification_argv_keeps_the_title_positional_and_the_body_whole() {
        let runner = FakeRunner::ok("");
        let client = HerdrCli::new(runner);
        client
            .notify(&Notification {
                title: "-5173 detected".into(),
                body: "accept to forward\nvite dev server".into(),
            })
            .expect("an empty echo is still a delivered toast");
        assert_eq!(
            client.runner.calls(),
            vec![vec![
                "notification",
                "show",
                "-5173 detected",
                "--body",
                "accept to forward\nvite dev server",
            ]]
        );
    }

    #[test]
    fn opening_a_pane_names_the_plugin_and_the_entrypoint_as_separate_words() {
        let runner = FakeRunner::ok(r#"{"id":"cli:plugin","result":{"type":"ok"}}"#);
        let client = HerdrCli::new(runner);
        client
            .open_pane("portfwd", "picker")
            .expect("an ok result is an opened pane");
        assert_eq!(
            client.runner.calls(),
            vec![vec![
                "plugin",
                "pane",
                "open",
                "--plugin",
                "portfwd",
                "--entrypoint",
                "picker",
            ]]
        );
    }

    #[test]
    fn a_refused_pane_open_is_reported_rather_than_read_as_opened() {
        // `ui_busy` while Settings or Copy mode is up, or an entrypoint this
        // Herdr does not know. Either way no pane appeared, so the caller must
        // not report success.
        for output in [
            CommandOutput {
                status: EXIT_REFUSED,
                stdout: String::new(),
                stderr: r#"{"error":{"code":"ui_busy","message":"a modal is open"}}"#.to_string(),
            },
            CommandOutput {
                status: 0,
                stdout: r#"{"id":"x","error":{"code":"ui_busy","message":"a modal is open"}}"#
                    .to_string(),
                stderr: String::new(),
            },
        ] {
            let client = HerdrCli::new(FakeRunner::new(vec![output]));
            let err = client
                .open_pane("portfwd", "picker")
                .expect_err("a refusal is not an opened pane");
            assert!(err.to_string().contains("ui_busy"), "{err}");
        }
    }

    #[test]
    fn a_pane_open_that_echoes_nothing_still_counts_as_opened() {
        let client = HerdrCli::new(FakeRunner::ok(""));
        client
            .open_pane("portfwd", "list")
            .expect("not every herdr echoes the response");
    }

    #[test]
    fn labelled_pane_list_makes_both_calls_in_order() {
        let runner = FakeRunner::new(vec![
            CommandOutput {
                status: 0,
                stdout: PANE_LIST_SAMPLE.to_string(),
                stderr: String::new(),
            },
            CommandOutput {
                status: 0,
                stdout: WORKSPACE_LIST_SAMPLE.to_string(),
                stderr: String::new(),
            },
        ]);
        let client = HerdrCli::new(runner);
        let panes = client
            .pane_list_labelled()
            .expect("both canned responses parse");
        assert_eq!(panes[0].workspace_label.as_deref(), Some("mini: scholion"));
        assert_eq!(
            client.runner.calls(),
            vec![vec!["pane", "list"], vec!["workspace", "list"]]
        );
    }

    #[test]
    fn a_refused_notification_on_stdout_is_reported() {
        let runner = FakeRunner::ok(ERROR_OBJECT);
        let client = HerdrCli::new(runner);
        let err = client
            .notify(&Notification {
                title: "t".into(),
                body: "b".into(),
            })
            .expect_err("a refusal must not read as a delivered toast");
        assert!(matches!(err, HerdrError::Command { .. }), "{err:?}");
    }

    // ---- exit status mapping ----

    #[test]
    fn exit_one_is_a_refusal_carrying_the_json_error() {
        let err = command_failure(PANE_LIST, EXIT_REFUSED, ERROR_OBJECT);
        match err {
            HerdrError::Command { code, stderr, .. } => {
                assert_eq!(code, EXIT_REFUSED);
                assert!(stderr.contains("refused"), "{stderr}");
                assert!(stderr.contains("pane_not_found: no pane w9:p9"), "{stderr}");
            }
            other => panic!("expected a command error, got {other:?}"),
        }
    }

    #[test]
    fn exit_one_with_unparseable_stderr_still_reads_as_a_refusal() {
        let err = command_failure(PANE_LIST, EXIT_REFUSED, "herdr: server not running\n");
        let text = err.to_string();
        assert!(text.contains("refused"), "{text}");
        assert!(text.contains("server not running"), "{text}");
    }

    #[test]
    fn exit_two_is_reported_as_a_command_line_problem_not_a_refusal() {
        let err = command_failure(PANE_LIST, EXIT_USAGE, "unknown option: --pane");
        match err {
            HerdrError::Command { code, stderr, .. } => {
                assert_eq!(code, EXIT_USAGE);
                assert!(!stderr.contains("refused"), "{stderr}");
                assert!(stderr.contains("rejected this command line"), "{stderr}");
                assert!(stderr.contains("unknown option: --pane"), "{stderr}");
            }
            other => panic!("expected a command error, got {other:?}"),
        }
    }

    #[test]
    fn exit_two_five_five_blames_ssh_not_herdr() {
        let err = command_failure(PANE_LIST, EXIT_SSH_FAILED, "ssh: connect: host is down");
        let text = err.to_string();
        assert!(text.contains("ssh could not run herdr"), "{text}");
    }

    #[test]
    fn an_unclassified_status_still_names_the_command_and_code() {
        let err = command_failure(PANE_LIST, 7, "");
        let text = err.to_string();
        assert!(text.contains("pane list"), "{text}");
        assert!(text.contains("exit 7"), "{text}");
        assert!(text.contains("no output on stderr"), "{text}");
    }

    #[test]
    fn a_failed_status_is_reported_before_the_output_is_parsed() {
        // stdout holds a perfectly good pane list; the status still wins,
        // because a non-zero herdr answered something else entirely.
        let runner = FakeRunner::new(vec![CommandOutput {
            status: EXIT_REFUSED,
            stdout: PANE_LIST_SAMPLE.to_string(),
            stderr: ERROR_OBJECT.to_string(),
        }]);
        let client = HerdrCli::new(runner);
        let err = client
            .pane_list()
            .expect_err("a non-zero exit must not be parsed as a result");
        assert!(matches!(err, HerdrError::Command { code, .. } if code == EXIT_REFUSED));
    }

    #[test]
    fn a_usage_failure_from_process_info_names_that_command() {
        let runner = FakeRunner::failing(EXIT_USAGE, "unknown option: --pane");
        let client = HerdrCli::new(runner);
        let err = client
            .shell_pid("w1:p1")
            .expect_err("exit 2 must not be ignored");
        match err {
            HerdrError::Command { command, code, .. } => {
                assert_eq!(command, PANE_PROCESS_INFO);
                assert_eq!(code, EXIT_USAGE);
            }
            other => panic!("expected a command error, got {other:?}"),
        }
    }

    // ---- remote flavor ----

    #[test]
    fn remote_calls_run_herdr_over_the_given_master() {
        let ssh = Arc::new(FakeSsh::new(SshOutcome::Stdout(
            PANE_LIST_SAMPLE.to_string(),
        )));
        let client = HerdrCli::remote(Arc::clone(&ssh), "buildbox", "/tmp/hpf/buildbox.ctl");
        let panes = client.pane_list().expect("the canned response must parse");
        assert_eq!(panes[0].pane_id, "w1:p1");
        assert_eq!(
            ssh.execs(),
            vec![(
                "buildbox".to_string(),
                PathBuf::from("/tmp/hpf/buildbox.ctl"),
                "'herdr' 'pane' 'list'".to_string(),
            )]
        );
    }

    #[test]
    fn remote_argv_is_quoted_word_by_word() {
        let ssh = Arc::new(FakeSsh::new(SshOutcome::Stdout(
            PROCESS_INFO_SAMPLE.to_string(),
        )));
        let client = HerdrCli::remote(Arc::clone(&ssh), "buildbox", "/tmp/c");
        client
            .shell_pid("w1:p1; rm -rf $HOME")
            .expect("the canned response must parse");
        let (_, _, command) = ssh.execs().remove(0);
        assert_eq!(
            command,
            "'herdr' 'pane' 'process-info' '--pane' 'w1:p1; rm -rf $HOME'"
        );
    }

    #[test]
    fn remote_binary_path_is_overridable_and_quoted() {
        let ssh = Arc::new(FakeSsh::new(SshOutcome::Stdout(
            WORKSPACE_LIST_SAMPLE.to_string(),
        )));
        let runner =
            RemoteHerdr::new(Arc::clone(&ssh), "buildbox", "/tmp/c").with_bin("/opt/my herdr/bin");
        let client = HerdrCli::new(runner);
        client
            .workspace_list()
            .expect("the canned response must parse");
        let (_, _, command) = ssh.execs().remove(0);
        assert_eq!(command, "'/opt/my herdr/bin' 'workspace' 'list'");
    }

    #[test]
    fn a_remote_exit_status_classifies_exactly_like_a_local_one() {
        for (code, expected) in [
            (EXIT_REFUSED, "refused"),
            (EXIT_USAGE, "rejected this command line"),
            (EXIT_SSH_FAILED, "ssh could not run herdr"),
        ] {
            let ssh = Arc::new(FakeSsh::new(SshOutcome::Failed {
                code,
                stderr: ERROR_OBJECT.to_string(),
            }));
            let client = HerdrCli::remote(ssh, "buildbox", "/tmp/c");
            let err = client
                .pane_list()
                .expect_err("a remote failure must surface");
            let text = err.to_string();
            assert!(text.contains(expected), "exit {code}: {text}");
        }
    }

    #[test]
    fn an_unusable_control_path_is_reported_as_a_spawn_failure() {
        let ssh = Arc::new(FakeSsh::new(SshOutcome::PathTooLong));
        let client = HerdrCli::remote(ssh, "buildbox", "/tmp/way/too/long.ctl");
        let err = client
            .pane_list()
            .expect_err("an unusable master cannot be used");
        assert!(matches!(err, HerdrError::Spawn(_)), "{err:?}");
        assert!(err.to_string().contains("over the"), "{err}");
    }

    #[test]
    fn a_missing_ssh_binary_is_reported_as_a_spawn_failure() {
        let ssh = Arc::new(FakeSsh::new(SshOutcome::NoSsh));
        let client = HerdrCli::remote(ssh, "buildbox", "/tmp/c");
        let err = client.pane_list().expect_err("no ssh means no answer");
        assert!(matches!(err, HerdrError::Spawn(_)), "{err:?}");
    }

    #[test]
    fn a_nul_in_an_argument_is_refused_before_it_reaches_ssh() {
        let ssh = Arc::new(FakeSsh::new(SshOutcome::Stdout(String::new())));
        let client = HerdrCli::remote(Arc::clone(&ssh), "buildbox", "/tmp/c");
        let err = client
            .shell_pid("w1:\0p1")
            .expect_err("a NUL cannot survive an exec and must not be sent");
        assert!(matches!(err, HerdrError::Spawn(_)), "{err:?}");
        assert!(ssh.execs().is_empty(), "nothing may be sent");
    }

    // ---- quoting ----

    #[test]
    fn every_shell_metacharacter_survives_quoting_as_a_literal() {
        for word in [
            "",
            "plain",
            "two words",
            "semi;colon",
            "$(id)",
            "`id`",
            "a\nb",
            "*",
            "--body",
            "back\\slash",
            "tab\there",
        ] {
            let quoted = shell_quote(word);
            assert!(
                quoted.starts_with('\'') && quoted.ends_with('\''),
                "{quoted}"
            );
            assert_eq!(&quoted[1..quoted.len() - 1], word);
        }
    }

    #[test]
    fn a_single_quote_is_closed_escaped_and_reopened() {
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
        assert_eq!(shell_quote("'"), r#"''\'''"#);
        assert_eq!(shell_quote("'; rm -rf /; '"), r#"''\''; rm -rf /; '\'''"#);
    }

    #[test]
    fn a_command_line_is_one_quoted_word_per_argv_entry() {
        let line = remote_command_line(
            "herdr",
            &notification_argv(&Notification {
                title: "port 5173".into(),
                body: "it's up".into(),
            }),
        )
        .expect("no NUL, so this must build");
        assert_eq!(
            line,
            r#"'herdr' 'notification' 'show' 'port 5173' '--body' 'it'\''s up'"#
        );
    }

    // ---- concurrency ----

    #[test]
    fn one_client_serves_several_threads_at_once() {
        let client = Arc::new(HerdrCli::new(FakeRunner::ok(PANE_LIST_SAMPLE)));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let client = Arc::clone(&client);
                std::thread::spawn(move || {
                    client.pane_list().expect("the canned response must parse")
                })
            })
            .collect();
        for thread in threads {
            let panes = thread.join().expect("no worker may panic");
            assert_eq!(panes[0].pane_id, "w1:p1");
        }
        assert_eq!(client.runner.calls().len(), 8);
    }

    // ---- local flavor ----

    #[test]
    fn the_local_flavor_keeps_the_binary_it_was_given() {
        // Never run here: a test that spawns a real herdr is out of bounds.
        let runner = LocalHerdr::with_bin("/nonexistent/herdr");
        assert_eq!(runner.bin, "/nonexistent/herdr");
        assert_eq!(LocalHerdr::default().bin, herdr_bin());
    }
}
