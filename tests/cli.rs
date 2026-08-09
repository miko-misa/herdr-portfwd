//! End-to-end tests of the binary, with every external command faked.
//!
//! These drive the real `herdr-portfwd` process, so they cover the one thing
//! unit tests structurally cannot: that `main` wires the modules together, that
//! an argv the manifest declares actually reaches an implementation, and that a
//! failure leaves an explanation on stderr and a non-zero status instead of a
//! panic. Herdr records both streams in `herdr plugin log list`, so those are
//! the plugin's only diagnostics.
//!
//! Nothing here may reach a real `ssh`, a real `herdr`, or a network. The
//! sandbox enforces that structurally rather than by hoping: `PATH` contains
//! exactly one directory, holding stand-ins for every program the binary can
//! spawn, and the `ssh` stand-in fails loudly if anything ever calls it. A test
//! that starts touching the network would have to get past an empty `PATH`
//! first.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// Ports and ids the fake host reports. Shared so an assertion and the fixture
/// that produces it cannot drift apart.
const FAKE_PORT: u16 = 5173;
const FAKE_PANE: &str = "pane-1";
const FAKE_SHELL_PID: u32 = 999;
const FAKE_LISTENER_PID: u32 = 4242;

// ---------------------------------------------------------------------------
// sandbox
// ---------------------------------------------------------------------------

struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = TempDir::new().expect("temp dir");
        let sandbox = Sandbox { root };
        for dir in [
            "bin",
            // Deliberately not on PATH: `herdr` is reachable only through the
            // HERDR_BIN_PATH Herdr injects, which is the whole point of that
            // variable — on a real host `herdr` need not be on a plugin
            // command's PATH. With one copy on PATH the two sources would be
            // indistinguishable and dropping the lookup would pass unnoticed.
            "herdr-bin",
            "home",
            "config/herdr-portfwd",
            // Where the records live: `$XDG_STATE_HOME/herdr-portfwd`, the same
            // directory the control socket uses. `state/plugin` is what Herdr
            // injects as HERDR_PLUGIN_STATE_DIR below and is deliberately *not*
            // where anything is written — see `Sandbox::state_file`.
            "state/herdr-portfwd",
            "state/plugin",
            "log",
        ] {
            fs::create_dir_all(sandbox.path(dir)).expect("create sandbox dir");
        }
        sandbox.install_fakes();
        sandbox
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn install_fakes(&self) {
        // Records every call and then fails: reaching a real host from a test is
        // the failure mode this whole file exists to make impossible, so an
        // invocation has to be both visible and useless.
        self.script(
            "ssh",
            "printf '%s\\n' \"$*\" >> \"$FAKE_SSH_LOG\"\n\
             echo \"fake ssh: a test must never reach a host ($*)\" >&2\n\
             exit 255\n",
        );
        self.script_in(
            "herdr-bin",
            "herdr",
            &format!(
                r#"printf '%s\n' "$*" >> "$FAKE_HERDR_LOG"
case "$1.$2" in
  pane.list)
    echo '{{"result":{{"panes":[{{"pane_id":"{FAKE_PANE}","workspace_id":"ws-1","agent":"claude"}}]}}}}'
    ;;
  pane.process-info)
    echo '{{"result":{{"process_info":{{"shell_pid":{FAKE_SHELL_PID}}}}}}}'
    ;;
  workspace.list)
    echo '{{"result":{{"workspaces":[{{"workspace_id":"ws-1","label":"mini: scholion"}}]}}}}'
    ;;
  notification.show)
    ;;
  plugin.pane)
    echo '{{"result":{{"type":"ok"}}}}'
    ;;
  *)
    echo "fake herdr: unsupported command $*" >&2
    exit 2
    ;;
esac
"#
            ),
        );
        // `-F pcn` field output, which is what the crate asks for.
        self.script(
            "lsof",
            &format!("printf 'p{FAKE_LISTENER_PID}\\ncnode\\nn127.0.0.1:{FAKE_PORT}\\n'\n"),
        );
        self.script("ss", "exit 1\n");
        self.script(
            "ps",
            &format!(
                "printf '{FAKE_LISTENER_PID} {FAKE_SHELL_PID}\\n{FAKE_SHELL_PID} 1\\n1 0\\n'\n"
            ),
        );
        for opener in ["open", "xdg-open"] {
            self.script(opener, "printf '%s\\n' \"$1\" >> \"$FAKE_OPEN_LOG\"\n");
        }
    }

    fn script(&self, name: &str, body: &str) {
        self.script_in("bin", name, body);
    }

    fn script_in(&self, dir: &str, name: &str, body: &str) {
        let path = self.path(dir).join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).expect("write fake");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake");
    }

    fn with_host(self) -> Self {
        fs::write(
            self.path("config/herdr-portfwd/hosts.toml"),
            "default_host = \"buildbox\"\n\n[hosts.buildbox]\ntarget = \"buildbox\"\nprefix = \"mini\"\n",
        )
        .expect("write hosts.toml");
        self
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-portfwd"));
        // Cleared, not extended: an inherited HERDR_* or a real PATH would make
        // these tests depend on the machine running them.
        command
            .env_clear()
            .env("PATH", self.path("bin"))
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("HERDR_PLUGIN_STATE_DIR", self.path("state/plugin"))
            .env("HERDR_BIN_PATH", self.path("herdr-bin").join("herdr"))
            .env("FAKE_HERDR_LOG", self.path("log/herdr.log"))
            .env("FAKE_OPEN_LOG", self.path("log/open.log"))
            .env("FAKE_SSH_LOG", self.path("log/ssh.log"))
            // Never a terminal, so no test can block on an interactive pane.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> Run {
        let output = self
            .command()
            .args(args)
            .output()
            .expect("spawn the binary");
        Run::new(args, output)
    }

    fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut command = self.command();
        command.args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        Run::new(args, command.output().expect("spawn the binary"))
    }

    /// Contents of a log one of the fakes appends to, or empty when nothing
    /// ever called it.
    fn log(&self, name: &str) -> String {
        fs::read_to_string(self.path("log").join(name)).unwrap_or_default()
    }

    /// A record file, at the one location every invocation of this tool uses.
    ///
    /// Not `HERDR_PLUGIN_STATE_DIR`, which is set for these runs and must be
    /// ignored: Herdr injects it for plugin commands only, so obeying it put a
    /// keybinding's forwards somewhere a shell's `status` never looked.
    fn state_file(&self, name: &str) -> String {
        fs::read_to_string(self.state_path(name)).unwrap_or_default()
    }

    fn state_path(&self, name: &str) -> PathBuf {
        self.path("state/herdr-portfwd").join(name)
    }

    /// A record file somewhere other than where it belongs, for asserting that
    /// nothing was written there.
    fn state_file_in(&self, dir: &str, name: &str) -> String {
        fs::read_to_string(self.path(dir).join(name)).unwrap_or_default()
    }
}

struct Run {
    argv: String,
    output: Output,
}

impl Run {
    fn new(args: &[&str], output: Output) -> Self {
        let run = Run {
            argv: args.join(" "),
            output,
        };
        // Checked for every invocation, not only where a test looks: a release
        // build aborts on panic, and an aborted plugin command leaves Herdr's
        // log with nothing to read.
        assert!(
            !run.stderr().contains("panicked at"),
            "`{}` panicked:\n{}",
            run.argv,
            run.stderr()
        );
        assert!(
            run.output.status.code().is_some(),
            "`{}` was killed by a signal instead of exiting",
            run.argv
        );
        run
    }

    fn code(&self) -> i32 {
        self.output.status.code().unwrap_or(-1)
    }

    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    fn succeeded(&self) -> &Self {
        assert_eq!(
            self.code(),
            0,
            "`{}` failed:\nstdout: {}\nstderr: {}",
            self.argv,
            self.stdout(),
            self.stderr()
        );
        self
    }

    fn failed(&self) -> &Self {
        assert_ne!(
            self.code(),
            0,
            "`{}` should have failed:\nstdout: {}",
            self.argv,
            self.stdout()
        );
        self
    }

    fn stderr_mentions(&self, needle: &str) -> &Self {
        assert!(
            self.stderr().contains(needle),
            "`{}` should have explained {needle:?} on stderr, got:\n{}",
            self.argv,
            self.stderr()
        );
        self
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout()).unwrap_or_else(|err| {
            panic!(
                "`{}` did not print JSON: {err}\n{}",
                self.argv,
                self.stdout()
            )
        })
    }
}

// ---------------------------------------------------------------------------
// the manifest is the source of truth for what can be invoked
// ---------------------------------------------------------------------------

/// Every argv `herdr-plugin.toml` can hand this binary, read from the manifest
/// itself rather than copied. A subcommand renamed on one side and not the
/// other is a runtime failure with no compile-time signal, so the two are not
/// allowed to be two lists.
fn manifest() -> toml::Table {
    toml::from_str(include_str!("../herdr-plugin.toml")).expect("herdr-plugin.toml parses")
}

/// The `id` of one section's entries, as Herdr will know them.
fn manifest_ids(section: &str) -> BTreeSet<String> {
    manifest()
        .get(section)
        .and_then(|value| value.as_array())
        .unwrap_or_else(|| panic!("the manifest has no [[{section}]]"))
        .iter()
        .map(|entry| {
            entry
                .get("id")
                .and_then(|value| value.as_str())
                .unwrap_or_else(|| panic!("a [[{section}]] entry has no id"))
                .to_string()
        })
        .collect()
}

fn manifest_argv() -> Vec<Vec<String>> {
    let manifest: toml::Table = manifest();
    let mut seen = BTreeSet::new();
    let mut argvs = Vec::new();
    // `build` is deliberately absent: it runs an installer script, not us.
    for section in ["startup", "actions", "panes", "events"] {
        let Some(entries) = manifest.get(section).and_then(|value| value.as_array()) else {
            continue;
        };
        for entry in entries {
            let command = entry
                .get("command")
                .and_then(|value| value.as_array())
                .unwrap_or_else(|| panic!("[[{section}]] entry has no command array"));
            let command: Vec<String> = command
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .unwrap_or_else(|| panic!("[[{section}]] argv entry is not a string"))
                        .to_string()
                })
                .collect();
            let Some((program, args)) = command.split_first() else {
                panic!("[[{section}]] declares an empty command");
            };
            let ours = Path::new(program).file_name() == Some(OsStr::new("herdr-portfwd"));
            if !ours {
                continue;
            }
            if seen.insert(args.to_vec()) {
                argvs.push(args.to_vec());
            }
        }
    }
    argvs
}

#[test]
fn the_manifest_declares_the_commands_this_test_file_thinks_it_does() {
    let argvs = manifest_argv();
    assert!(
        argvs.len() >= 8,
        "the manifest should invoke this binary several ways, found {argvs:?}"
    );
    let flattened: BTreeSet<String> = argvs.iter().map(|argv| argv.join(" ")).collect();
    for expected in [
        "reconcile --startup",
        "reconcile --event",
        "scan",
        "add",
        "stop",
        "stop-all",
        "open-url",
        "pane-ui list",
        "pane-ui picker",
    ] {
        assert!(
            flattened.contains(expected),
            "the manifest no longer invokes `{expected}`; if that is deliberate, \
             update this list and src/cli.rs together"
        );
    }
}

#[test]
fn every_manifest_argv_runs_without_panicking() {
    let sandbox = Sandbox::new().with_host();
    for argv in manifest_argv() {
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        let run = sandbox.run(&args);
        // Clap exits 2 when it could not parse the argv. That is the drift this
        // test is here to catch, and it is a different failure from a command
        // that ran and could not do its job.
        assert_ne!(
            run.code(),
            2,
            "the manifest argv `{}` no longer parses:\n{}",
            run.argv,
            run.stderr()
        );
        // Whatever happened, the log has to explain it.
        assert!(
            run.code() == 0 || !run.stderr().trim().is_empty(),
            "`{}` failed silently; a plugin log entry with no message is useless",
            run.argv
        );
    }
}

#[test]
fn every_manifest_argv_also_runs_with_no_configuration_at_all() {
    // The first run after `herdr plugin install` has no hosts.toml. None of
    // these may panic or hang; each should say what is missing.
    let sandbox = Sandbox::new();
    for argv in manifest_argv() {
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        let run = sandbox.run(&args);
        assert_ne!(run.code(), 2, "`{}` no longer parses", run.argv);
    }
}

#[test]
fn the_plugin_and_pane_ids_the_binary_names_are_the_ones_the_manifest_declares() {
    // Asking Herdr for a pane means naming both: `plugin pane open --plugin
    // <id> --entrypoint <id>`. A rename on one side alone is a request Herdr
    // answers with `plugin_not_found` at runtime and nothing at build time.
    assert_eq!(
        manifest()
            .get("id")
            .and_then(|value| value.as_str())
            .expect("the manifest has an id"),
        herdr_portfwd::PLUGIN_ID,
    );
    assert_eq!(
        manifest_ids("panes"),
        ["list", "picker"]
            .iter()
            .map(|id| (*id).to_string())
            .collect::<BTreeSet<String>>(),
    );
}

// ---------------------------------------------------------------------------
// a plugin action that was not told enough
// ---------------------------------------------------------------------------
//
// Herdr runs an action headless: no terminal, and stdin that may never reach
// EOF. `add` and `stop` are declared with no port because a person picks one,
// so with nowhere to ask they used to fail from every keybinding and every
// command-palette entry. They ask Herdr for the pane that can ask instead.

/// What Herdr injects for an action command, on top of what every plugin
/// command already gets from `Sandbox::command`.
fn action_env(action_id: &str) -> Vec<(&str, &str)> {
    vec![
        ("HERDR_PLUGIN_ACTION_ID", action_id),
        ("HERDR_PLUGIN_ID", "portfwd"),
    ]
}

#[test]
fn an_action_with_no_port_asks_herdr_to_open_the_pane_that_can_ask() {
    for (argv, action_id, entrypoint) in [
        // The picker offers listeners to forward; the manager overlay lists
        // live forwards and has the keys that stop them.
        (["add"], "portfwd.add", "picker"),
        (["stop"], "portfwd.stop", "list"),
        // `scan` too, and it is the flagship keybinding: the README and the
        // manifest both promise it "shows attributed listeners and forwards the
        // ones you pick". It used to print its report instead, into the plugin
        // log, where the person who pressed the key never saw it.
        (["scan"], "portfwd.scan", "picker"),
    ] {
        // A sandbox each, so one command's herdr log cannot satisfy the other's
        // assertion.
        let sandbox = Sandbox::new().with_host();
        let run = sandbox.run_with(&argv, &action_env(action_id));
        run.succeeded();
        let expected = format!("plugin pane open --plugin portfwd --entrypoint {entrypoint}");
        assert!(
            sandbox.log("herdr.log").contains(&expected),
            "`{}` never asked Herdr for `{expected}`; herdr calls were:\n{}",
            run.argv,
            sandbox.log("herdr.log")
        );
    }
}

#[test]
fn a_scan_that_someone_can_actually_read_still_prints_its_report() {
    // The other half of the flagship keybinding's fix. `scan` differs from
    // `add` and `stop` in having a complete answer without asking anybody, so a
    // caller parsing JSON gets that answer rather than a pane opened in front
    // of nobody — even when Herdr ran it as an action.
    let sandbox = Sandbox::new().with_host();
    let mut env = action_env("portfwd.scan");
    env.push(("HERDR_PORTFWD_MODE", "remote"));
    let run = sandbox.run_with(&["--json", "scan"], &env);
    run.succeeded();
    assert_eq!(run.json()["host"], serde_json::json!("buildbox"));
    assert!(
        !sandbox.log("herdr.log").contains("pane open"),
        "--json asked for the report, not for a pane:\n{}",
        sandbox.log("herdr.log")
    );

    // And a shell with no Herdr on the other end: nothing to ask, nowhere to
    // draw, but the report is still worth printing.
    let shell = Sandbox::new().with_host();
    let run = shell.run_with(&["scan"], &[("HERDR_PORTFWD_MODE", "remote")]);
    run.succeeded();
    assert!(run.stdout().contains("candidate(s)"), "{}", run.stdout());
    assert!(!shell.log("herdr.log").contains("pane open"));
}

#[test]
fn an_action_with_a_port_acts_instead_of_opening_a_pane() {
    // The other half: an action that was told what to do must still do it.
    let sandbox = Sandbox::new().with_host();
    let mut env = action_env("portfwd.add");
    env.push(("HERDR_PORTFWD_MODE", "remote"));
    let run = sandbox.run_with(&["--json", "add", "5173"], &env);
    run.succeeded();
    assert_eq!(run.json()["applied"], serde_json::json!("requested"));
    assert!(
        !sandbox.log("herdr.log").contains("pane open"),
        "nothing was missing, so nothing should have been asked for"
    );
}

#[test]
fn a_pane_command_never_asks_herdr_to_open_another_pane() {
    // A pane's own command carries HERDR_PLUGIN_ENTRYPOINT_ID. If that were not
    // enough to stop it, the pane Herdr opened would ask for a pane, whose
    // command would ask again.
    let sandbox = Sandbox::new().with_host();
    let run = sandbox.run_with(
        &["pane-ui", "picker"],
        &[
            ("HERDR_PLUGIN_ENTRYPOINT_ID", "picker"),
            ("HERDR_PLUGIN_ID", "portfwd"),
            ("HERDR_PORTFWD_MODE", "remote"),
        ],
    );
    run.succeeded();

    // And the same guard when a pane command runs an action's argv: the
    // entrypoint id wins over the action id, so this refuses rather than
    // recursing.
    let refused = sandbox.run_with(
        &["add"],
        &[
            ("HERDR_PLUGIN_ACTION_ID", "portfwd.add"),
            ("HERDR_PLUGIN_ENTRYPOINT_ID", "picker"),
            ("HERDR_PLUGIN_ID", "portfwd"),
        ],
    );
    refused.failed().stderr_mentions("no port given");

    assert!(
        !sandbox.log("herdr.log").contains("pane open"),
        "a pane command asked for another pane:\n{}",
        sandbox.log("herdr.log")
    );
}

#[test]
fn a_hook_or_a_shell_still_gets_the_refusal_rather_than_a_pane() {
    // Nothing to draw for and nobody to ask. A startup or event hook has no
    // user waiting on it, and a person in a shell gets a sentence to act on.
    let sandbox = Sandbox::new().with_host();
    for env in [
        vec![("HERDR_PLUGIN_EVENT", "startup")],
        vec![],
        // An action Herdr somehow injected no binary path into: there is no
        // Herdr to ask, so the refusal is all there is.
        vec![
            ("HERDR_PLUGIN_ACTION_ID", "portfwd.add"),
            ("HERDR_BIN_PATH", ""),
        ],
    ] {
        sandbox
            .run_with(&["add"], &env)
            .failed()
            .stderr_mentions("no port given");
    }
    assert!(
        !sandbox.log("herdr.log").contains("pane open"),
        "{}",
        sandbox.log("herdr.log")
    );
}

#[test]
fn each_command_with_no_port_names_itself_in_its_hint() {
    // A `stop` that suggests `add` sends the user to the opposite of what they
    // asked for.
    let sandbox = Sandbox::new().with_host();
    for (argv, hint) in [
        (["add"], "herdr-portfwd add 5173"),
        (["stop"], "herdr-portfwd stop 5173"),
    ] {
        sandbox.run(&argv).failed().stderr_mentions(hint);
    }
}

// ---------------------------------------------------------------------------
// open-url: the one place untrusted terminal output reaches the binary
// ---------------------------------------------------------------------------

#[test]
fn open_url_rejects_a_hostile_url_and_forwards_nothing() {
    let sandbox = Sandbox::new().with_host();
    let hostile = [
        // Reads as loopback, resolves anywhere.
        "http://localhost.evil.example.com:5173/",
        // Userinfo hides the real host behind an @.
        "http://localhost:5173@evil.example.com/",
        // Not a scheme we forward.
        "file:///etc/passwd",
        "ftp://localhost:5173/",
        // A non-loopback host, spelled to look local.
        "http://127.0.0.2:5173/",
        "http://0.0.0.0:5173/",
        // No port: there is nothing to forward.
        "http://localhost/",
        // Ports outside the range, or not plain decimal.
        "http://localhost:0/",
        "http://localhost:99999/",
        "http://localhost:0x1415/",
        // A control character that would drive the terminal reading the log.
        "http://localhost:5173/\u{1b}]0;pwned\u{7}",
        // A homoglyph host that renders as `localhost`.
        "http://lοcalhost:5173/",
        "",
    ];
    for url in hostile {
        let run = sandbox.run(&["open-url", url]);
        run.failed().stderr_mentions("herdr-portfwd:");
        assert!(
            !run.stderr().contains('\u{1b}'),
            "an escape sequence from the URL reached the log verbatim: {:?}",
            run.stderr()
        );
    }
    assert_eq!(
        sandbox.log("open.log"),
        "",
        "a rejected URL must never reach the desktop opener"
    );
    assert!(
        !sandbox.state_file("forwards.json").contains("5173"),
        "a rejected URL must not record a forward"
    );
    assert!(
        !sandbox.state_file("requests.json").contains("5173"),
        "a rejected URL must not queue a request"
    );
    assert_eq!(
        sandbox.log("ssh.log"),
        "",
        "a URL is validated before anything opens a connection"
    );
}

#[test]
fn open_url_rejects_a_hostile_url_that_arrives_through_the_environment() {
    // The link handler sets HERDR_PLUGIN_CLICKED_URL; the manifest pattern has
    // already run by then, and this is the re-validation that does not trust it.
    let sandbox = Sandbox::new().with_host();
    let run = sandbox.run_with(
        &["open-url"],
        &[("HERDR_PLUGIN_CLICKED_URL", "http://evil.example.com:5173/")],
    );
    run.failed().stderr_mentions("loopback");
    assert_eq!(sandbox.log("open.log"), "");
}

#[test]
fn open_url_with_no_url_at_all_says_so() {
    let sandbox = Sandbox::new().with_host();
    sandbox
        .run(&["open-url"])
        .failed()
        .stderr_mentions("no URL");
}

#[test]
fn open_url_on_the_far_side_queues_a_request_and_notifies_instead_of_opening() {
    // `herdr --remote`: the plugin is on the host, the browser is not.
    let sandbox = Sandbox::new().with_host();
    let run = sandbox.run_with(
        &["--json", "open-url", "http://localhost:5173/app?x=1"],
        &[("HERDR_PORTFWD_MODE", "remote")],
    );
    run.succeeded();
    let json = run.json();
    assert_eq!(json["opened"], serde_json::json!(false));
    assert_eq!(
        json["url"],
        serde_json::json!("http://localhost:5173/app?x=1")
    );
    assert_eq!(json["add"]["applied"], serde_json::json!("requested"));
    assert_eq!(json["add"]["spec"]["remote_port"], serde_json::json!(5173));
    assert_eq!(
        json["add"]["spec"]["local_port"],
        serde_json::json!(5173),
        "a clicked URL must keep its port number"
    );

    assert!(
        sandbox.state_file("requests.json").contains("5173"),
        "the request the local agent will apply was not recorded"
    );
    // The notification promises the URL opens once the agent applies the
    // forward. The agent is the half of this tool on the machine with the
    // browser, so the URL has to travel to it or that promise is false.
    assert!(
        sandbox
            .state_file("requests.json")
            .contains("http://localhost:5173/app?x=1"),
        "the local agent was given nothing to open:\n{}",
        sandbox.state_file("requests.json")
    );
    assert!(
        sandbox.log("herdr.log").contains("notification show"),
        "the user was never told where to point a browser: {:?}",
        sandbox.log("herdr.log")
    );
    assert_eq!(
        sandbox.log("open.log"),
        "",
        "nothing may be opened on the remote host's desktop"
    );
    assert_eq!(
        sandbox.log("ssh.log"),
        "",
        "the far side has no master to talk to; the local agent owns that"
    );
}

#[test]
fn a_local_scan_goes_through_this_machines_control_master() {
    // Also the control test for every `ssh.log` assertion elsewhere: it proves
    // the stand-in really does record the calls those tests claim never happen.
    let sandbox = Sandbox::new().with_host();
    let run = sandbox.run_with(&["scan"], &[("HERDR_PORTFWD_MODE", "local")]);
    run.failed().stderr_mentions("herdr-portfwd:");
    let calls = sandbox.log("ssh.log");
    assert!(
        calls.contains("-M"),
        "a local scan has to bring up the master first, got: {calls:?}"
    );
    assert!(
        calls.contains("buildbox"),
        "the master must be for the resolved host, got: {calls:?}"
    );
}

// ---------------------------------------------------------------------------
// the rest of the surface
// ---------------------------------------------------------------------------

#[test]
fn add_on_the_far_side_records_a_request_rather_than_pretending_to_forward() {
    let sandbox = Sandbox::new().with_host();
    let run = sandbox.run_with(
        &["--json", "add", "5173"],
        &[("HERDR_PORTFWD_MODE", "remote")],
    );
    run.succeeded();
    let json = run.json();
    assert_eq!(json["applied"], serde_json::json!("requested"));
    assert_eq!(json["placement"]["placement"], serde_json::json!("remote"));
    assert!(json["request_id"].is_number());
}

#[test]
fn a_local_port_remap_is_never_implied() {
    let sandbox = Sandbox::new().with_host();
    let run = sandbox.run_with(
        &["--json", "add", "5173", "--local-port", "15173"],
        &[("HERDR_PORTFWD_MODE", "remote")],
    );
    run.succeeded();
    assert_eq!(run.json()["spec"]["local_port"], serde_json::json!(15173));

    let plain = sandbox.run_with(
        &["--json", "add", "5173"],
        &[("HERDR_PORTFWD_MODE", "remote")],
    );
    plain.succeeded();
    assert_eq!(
        plain.json()["spec"]["local_port"],
        serde_json::json!(5173),
        "without --local-port the local port must equal the remote one"
    );
}

#[test]
fn add_without_a_port_refuses_instead_of_guessing_one() {
    let sandbox = Sandbox::new().with_host();
    sandbox
        .run_with(&["add"], &[("HERDR_PORTFWD_MODE", "remote")])
        .failed()
        .stderr_mentions("no port given");
}

#[test]
fn stopping_a_forward_this_machine_does_not_hold_says_where_to_run_it() {
    let sandbox = Sandbox::new().with_host();
    for argv in [vec!["stop", "5173"], vec!["stop-all"]] {
        sandbox
            .run_with(&argv, &[("HERDR_PORTFWD_MODE", "remote")])
            .failed()
            .stderr_mentions("--host buildbox");
    }
}

#[test]
fn stopping_a_forward_that_was_never_created_is_a_no_op() {
    // Invariant 4. Local placement with an empty store: nothing matches, and
    // that has to be success, not an error.
    let sandbox = Sandbox::new().with_host();
    let run = sandbox.run_with(
        &["--json", "stop", "5173"],
        &[("HERDR_PORTFWD_MODE", "local")],
    );
    run.succeeded();
    assert_eq!(run.json()["stopped"], serde_json::json!([]));
}

#[test]
fn scan_attributes_a_listener_to_the_pane_that_owns_it() {
    let sandbox = Sandbox::new().with_host();
    let run = sandbox.run_with(&["--json", "scan"], &[("HERDR_PORTFWD_MODE", "remote")]);
    run.succeeded();
    let json = run.json();
    assert_eq!(json["host"], serde_json::json!("buildbox"));
    let candidates = json["candidates"]
        .as_array()
        .expect("candidates is an array");
    assert_eq!(candidates.len(), 1, "{json}");
    assert_eq!(candidates[0]["listener"]["port"], serde_json::json!(5173));
    assert_eq!(
        candidates[0]["pane"]["pane_id"],
        serde_json::json!(FAKE_PANE),
        "the listener's ancestry reaches the pane's shell_pid, so it belongs to it"
    );
    assert_eq!(
        candidates[0]["already_forwarded"],
        serde_json::json!(false),
        "listing is not forwarding"
    );
    assert!(
        sandbox.state_file("forwards.json").is_empty()
            || !sandbox.state_file("forwards.json").contains("5173"),
        "a scan must not forward anything on its own"
    );
}

#[test]
fn reconcile_with_nothing_recorded_succeeds_for_every_trigger() {
    let sandbox = Sandbox::new().with_host();
    for argv in [
        vec!["--json", "reconcile", "--startup"],
        vec!["--json", "reconcile", "--event"],
        vec!["--json", "reconcile"],
    ] {
        let run = sandbox.run(&argv);
        run.succeeded();
        assert_eq!(run.json()["dropped"], serde_json::json!([]));
        assert_eq!(run.json()["kept"], serde_json::json!([]));
    }
    assert_eq!(
        sandbox.log("herdr.log").matches("pane list").count(),
        0,
        "reconcile with no records must not go asking anyone anything"
    );
}

#[test]
fn a_startup_reconcile_does_not_dial_when_there_is_nothing_to_restore() {
    // Startup is the one trigger allowed to open a connection. With no records
    // there is nothing to restore, so it must not open one anyway — a plugin
    // that dialled every configured host on every server start would be a
    // surprise the user never asked for.
    let sandbox = Sandbox::new().with_host();
    sandbox.run(&["reconcile", "--startup"]).succeeded();
    assert_eq!(sandbox.log("ssh.log"), "", "no records means no ssh at all");
    // The records and the control sockets share a directory, so its existence
    // proves nothing; a socket in it would.
    let sockets: Vec<String> = fs::read_dir(sandbox.path("state/herdr-portfwd"))
        .expect("the state directory exists")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".ctl"))
        .collect();
    assert!(
        sockets.is_empty(),
        "no control socket should have been created: {sockets:?}"
    );
}

#[test]
fn status_reports_rather_than_failing_when_the_world_is_broken() {
    // `status` is what someone runs when something is already wrong, so an
    // unresolvable host has to be data in the report, not an error exit.
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["--json", "status"]);
    run.succeeded();
    let json = run.json();
    assert_eq!(json["selected"], serde_json::Value::Null);
    assert!(
        json["selection_error"]
            .as_str()
            .is_some_and(|text| text.contains("hosts")),
        "{json}"
    );
    assert_eq!(json["hosts"], serde_json::json!([]));
    assert_eq!(json["pending_requests"], serde_json::json!(0));
}

#[test]
fn a_keybinding_and_a_shell_read_and_write_the_same_records() {
    // The bug: `state_dir` preferred HERDR_PLUGIN_STATE_DIR, which Herdr
    // injects for plugin commands and nothing else. A forward created by a
    // keybinding therefore landed in Herdr's per-plugin directory and
    // `herdr-portfwd status` in a shell read `$XDG_STATE_HOME/herdr-portfwd`
    // and reported nothing — while the port was open, because the control
    // socket already ignored that variable and the master *was* shared. `stop`
    // could not stop what it could not see.
    let sandbox = Sandbox::new().with_host();

    // A plugin action, with everything Herdr injects, queues a request.
    let mut env = action_env("portfwd.add");
    env.push(("HERDR_PORTFWD_MODE", "remote"));
    let queued = sandbox.run_with(&["--json", "add", "5173"], &env);
    queued.succeeded();
    assert_eq!(queued.json()["applied"], serde_json::json!("requested"));

    // A plain shell invocation, with none of them, has to see it.
    let status = sandbox.run_with(&["--json", "status"], &[("HERDR_PORTFWD_MODE", "remote")]);
    status.succeeded();
    assert_eq!(
        status.json()["pending_requests"],
        serde_json::json!(1),
        "a shell must see what a keybinding recorded: {}",
        status.stdout()
    );
    // And it has to name the file it really read, so the Troubleshooting
    // section's `cat` of it finds something.
    let reported = status.json()["request_file"]
        .as_str()
        .expect("status names the request file")
        .to_string();
    assert_eq!(
        PathBuf::from(&reported),
        sandbox.state_path("requests.json"),
        "status printed a path other than the one it used"
    );
    assert!(
        fs::read_to_string(&reported)
            .expect("the file status named exists")
            .contains("5173"),
        "the request is not in the file status pointed at"
    );
    assert_eq!(
        sandbox.state_file_in("state/plugin", "requests.json"),
        "",
        "nothing may be written to the per-plugin directory any more"
    );
}

#[test]
fn status_names_the_selected_host_and_where_the_files_are() {
    let sandbox = Sandbox::new().with_host();
    let run = sandbox.run_with(&["status"], &[("HERDR_PORTFWD_MODE", "remote")]);
    run.succeeded();
    let text = run.stdout();
    assert!(text.contains("buildbox"), "{text}");
    assert!(text.contains("forwards.json"), "{text}");
    assert!(text.contains("requests.json"), "{text}");
}

#[test]
fn an_unknown_host_is_refused_with_the_configured_ones_named() {
    let sandbox = Sandbox::new().with_host();
    sandbox
        .run(&["--host", "typo", "list"])
        .failed()
        .stderr_mentions("unknown host")
        .stderr_mentions("buildbox");
}

#[test]
fn a_host_name_that_would_escape_the_socket_directory_is_refused() {
    // The name becomes `<dir>/<name>.ctl`. A path separator in it would put the
    // socket somewhere other than the directory whose length was checked.
    let sandbox = Sandbox::new().with_host();
    for name in ["../elsewhere", "a/b", ".."] {
        sandbox
            .run(&["attach", name])
            .failed()
            .stderr_mentions("herdr-portfwd:");
    }
    assert_eq!(sandbox.log("herdr.log"), "");
}

#[test]
fn the_host_environment_variable_selects_a_host_like_the_flag() {
    let sandbox = Sandbox::new().with_host();
    let run = sandbox.run_with(
        &["--json", "list"],
        &[
            ("HERDR_PORTFWD_HOST", "buildbox"),
            ("HERDR_PORTFWD_MODE", "remote"),
        ],
    );
    run.succeeded();
    assert_eq!(run.json()["host"], serde_json::json!("buildbox"));
}

#[test]
fn a_malformed_invocation_context_is_reported_not_ignored() {
    let sandbox = Sandbox::new().with_host();
    sandbox
        .run_with(&["list"], &[("HERDR_PLUGIN_CONTEXT_JSON", "{not json")])
        .failed()
        .stderr_mentions("HERDR_PLUGIN_CONTEXT_JSON");
}

#[test]
fn a_truncated_state_file_does_not_stop_the_binary() {
    let sandbox = Sandbox::new().with_host();
    fs::write(
        sandbox.state_path("forwards.json"),
        "{\"version\":1,\"forwards\":[{\"host\"",
    )
    .expect("write a half-written state file");
    let run = sandbox.run_with(&["--json", "list"], &[("HERDR_PORTFWD_MODE", "remote")]);
    run.succeeded();
    assert_eq!(run.json()["forwards"], serde_json::json!([]));
    assert!(
        run.stderr().contains("warning"),
        "a damaged state file has to be announced: {:?}",
        run.stderr()
    );
}

#[test]
fn version_and_help_work_without_any_configuration() {
    let sandbox = Sandbox::new();
    let version = sandbox.run(&["--version"]);
    version.succeeded();
    assert!(version.stdout().contains(env!("CARGO_PKG_VERSION")));
    sandbox.run(&["--help"]).succeeded();
    sandbox.run(&["pane-ui", "--help"]).succeeded();
}

#[test]
fn an_unknown_subcommand_is_a_usage_error_not_a_crash() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["definitely-not-a-subcommand"]);
    assert_eq!(run.code(), 2, "clap reports a usage error as 2");
}

#[test]
fn a_pane_reads_to_end_of_input_and_exits_instead_of_hanging() {
    // Herdr closes a pane when its command exits. With stdin at EOF — which is
    // what the sandbox gives every run — both entrypoints must render once and
    // return, never block.
    let sandbox = Sandbox::new().with_host();
    for entrypoint in ["list", "picker"] {
        let run = sandbox.run_with(
            &["pane-ui", entrypoint],
            &[
                // What Herdr injects when it launches a declared pane. It is
                // also what tells this process it is the pane rather than the
                // headless action that shares the argv.
                ("HERDR_PLUGIN_ENTRYPOINT_ID", entrypoint),
                ("HERDR_PLUGIN_ID", "portfwd"),
                ("HERDR_PORTFWD_MODE", "remote"),
            ],
        );
        run.succeeded();
        assert!(
            !run.stdout().is_empty(),
            "the {entrypoint} pane drew nothing"
        );
    }
}

#[test]
fn the_manage_forwards_action_asks_herdr_for_the_pane_instead_of_drawing_into_the_log() {
    // `[[actions]] id = "list"` invokes the `list` pane's own argv, and Herdr
    // runs an action headless with both streams captured into the plugin log.
    // Drawing there paints one frame of the overlay into the log, hits EOF on
    // stdin and exits 0 — the palette entry whose entire purpose is that pane
    // showed the user nothing at all.
    let sandbox = Sandbox::new().with_host();
    let run = sandbox.run_with(&["pane-ui", "list"], &action_env("portfwd.list"));
    run.succeeded();

    assert!(
        sandbox
            .log("herdr.log")
            .contains("plugin pane open --plugin portfwd --entrypoint list"),
        "the action never asked Herdr for the pane; herdr calls were:\n{}",
        sandbox.log("herdr.log")
    );
    assert!(
        !run.stdout().contains('\u{1b}'),
        "an action drew a pane into its captured stdout: {:?}",
        run.stdout()
    );
}

#[test]
fn a_pane_argv_with_nowhere_to_draw_and_nobody_to_ask_refuses_rather_than_drawing() {
    // No entrypoint id, no action id, no terminal: a hook or a shell pipeline.
    // Escape sequences into a captured stdout and a zero exit read as success
    // and show nobody anything.
    let sandbox = Sandbox::new().with_host();
    for entrypoint in ["list", "picker"] {
        let run = sandbox.run_with(
            &["pane-ui", entrypoint],
            &[("HERDR_PORTFWD_MODE", "remote")],
        );
        run.failed().stderr_mentions("no terminal to draw on");
        assert!(
            !run.stdout().contains('\u{1b}'),
            "the pane drew anyway: {:?}",
            run.stdout()
        );
    }
    assert!(
        !sandbox.log("herdr.log").contains("pane open"),
        "there was no Herdr action to ask on behalf of:\n{}",
        sandbox.log("herdr.log")
    );
}
