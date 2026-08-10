//! Process entry point: parse an argv, run what it names, and leave behind a
//! plugin log entry that explains itself.
//!
//! Herdr spawns these commands with stdout and stderr captured into
//! `herdr plugin log list`, so what this file prints matters as much as what it
//! does. Reports and `--json` go to stdout; failures go to stderr and set a
//! non-zero exit. A failure is prose even under `--json`, because a caller that
//! got no JSON needs the reason where a log will show it, not a second format
//! to parse. Nothing here panics on a hostile environment: a release build
//! aborts on panic, which would leave the log with no explanation at all.
//!
//! This is also the only place the real `ssh`, the real `herdr`, the real state
//! files and the real terminal are chosen. Every module below composes over
//! traits precisely so that this file is the single seam where they are picked.
//!
//! # When a command was not told enough
//!
//! The manifest invokes `add` and `stop` with no port, because a person picks
//! one. A plugin action runs headless, so there is nobody here to ask and no
//! guarantee stdin will ever reach EOF — blocking a pane-less action on a read
//! would hang Herdr's command log instead of filling it. Drawing a pane on a
//! terminal this process does not have is no better.
//!
//! But Herdr *can* open a pane, and this plugin declares two. So the rule is
//! [`route`]:
//!
//! - a plugin **action** asks Herdr for it —
//!   `herdr plugin pane open --plugin <id> --entrypoint <list|picker>` — and
//!   the pane it opens is the one that can ask the user;
//! - a terminal draws the pane itself;
//! - anything else refuses, naming what was missing, because a refusal in the
//!   command log is a sentence somebody can act on.
//!
//! "A plugin action" is decided by [`PluginInvocation::pane_to_open`] from the
//! variables Herdr injects, not by looking at the terminal: an action carries
//! `HERDR_PLUGIN_ACTION_ID`, a pane command carries
//! `HERDR_PLUGIN_ENTRYPOINT_ID` and must never ask for another pane, and
//! `HERDR_BIN_PATH` is what proves there is a Herdr on the other end at all.
//!
//! **Herdr is asked before the terminal is consulted.** Herdr pipes an action's
//! stdout and stderr but leaves its stdin alone, so an action spawned by a
//! server running in the user's foreground terminal — `herdr server`, or
//! `herdr --no-session` — inherits that terminal on stdin. Looking at
//! `is_terminal()` first would draw the pane into the plugin log and then read
//! keys off the terminal Herdr itself is reading, taking the user's keystrokes
//! and never reaching EOF.
//!
//! `--json` is never interactive, since a caller parsing JSON is not sitting at
//! a keyboard either. Neither is a redirected stdout: a pane reads keys from
//! stdin and paints frames to stdout, so both ends have to be the terminal or
//! there is nowhere to draw.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context as _, Result};
use clap::Parser;
use serde::Serialize;

use herdr_portfwd::actions::{
    Actions, AddOutcome, Applied, DropReason, Env, ForwardKey, HostHerdr, ListReport, Liveness,
    OpenOutcome, Placement, PlacementReason, ReconcileReport, ReconcileTrigger, ScanReport, Scope,
    StatusReport, StopOutcome, SystemOpener, SystemPorts, Where, MODE_ENV,
};
use herdr_portfwd::cli::{Cli, Command, PaneEntrypoint};
use herdr_portfwd::config::HostsConfig;
use herdr_portfwd::confirm::{ConfirmState, CONFIRM_ENV};
use herdr_portfwd::context::{Invoked, PluginInvocation};
use herdr_portfwd::herdr::{HerdrClient, Notification};
use herdr_portfwd::herdr_cli::{HerdrCli, LocalHerdr};
use herdr_portfwd::local_agent;
use herdr_portfwd::request::RequestLog;
use herdr_portfwd::ssh::{control_socket_path, MasterState, SshRunner};
use herdr_portfwd::ssh_process::SshProcess;
use herdr_portfwd::state::Store;
use herdr_portfwd::types::{AttributedListener, Host, Origin, PaneRef};
use herdr_portfwd::ui::{
    self, ConfirmActions, ConfirmSnapshot, Exit, ForwardRow, Handoff, HostFilter, ListActions,
    ListSnapshot, ListenerRow, Listing, PickerActions, PickerSnapshot, Status,
};

/// Anything went wrong. One code, because a plugin log reads the message, not
/// the number.
const EXIT_FAILURE: u8 = 1;

/// Ctrl+C, by the usual shell convention. Distinct from a failure so a pane the
/// user interrupted does not read as a broken plugin.
const EXIT_INTERRUPTED: u8 = 130;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            report_error(&err);
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let host = cli.host.clone();
    let json = cli.json;
    wired(host, json, |ctx| ctx.dispatch(cli.command))
}

/// Print a failure the way a plugin log has to read it: what failed, then why,
/// all on stderr so stdout stays parseable.
fn report_error(err: &anyhow::Error) {
    let mut out = io::stderr().lock();
    let _ = writeln!(out, "herdr-portfwd: {err}");
    for cause in err.chain().skip(1) {
        let _ = writeln!(out, "  caused by: {cause}");
    }
}

/// Write to stdout, ignoring a closed pipe. A reader that hung up does not
/// undo the work already done, and panicking here would replace a useful log
/// line with a crash.
fn write_out(text: &str) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

// ---------------------------------------------------------------------------
// wiring
// ---------------------------------------------------------------------------

/// Everything the real world is plugged into, plus the two globals that shape
/// output.
struct Ctx<'a> {
    actions: Actions<'a>,
    /// The invoking server's client, kept concretely so workspace labels — which
    /// are not on [`HerdrClient`] — are still reachable for rendering.
    herdr: &'a HerdrCli<LocalHerdr>,
    json: bool,
    /// Whether stdin is a terminal, and whether stdout is. Both matter and
    /// they are not the same question: a pane reads its keys from stdin and
    /// paints its frames to stdout, so `herdr-portfwd add > out` has a keyboard
    /// and nowhere to draw. Fields rather than calls, because `is_terminal()`
    /// cannot be faked: every test runs with stdin at `/dev/null`, so the
    /// branch that opens a pane would otherwise be the one user-facing
    /// behaviour nothing can cover.
    stdin_terminal: bool,
    stdout_terminal: bool,
    /// How Herdr started this process, read once at the edge. What a command
    /// with no port does about that is a pure function of it; see [`route`].
    plugin: PluginInvocation,
    /// The click the `confirm` pane was opened for, when this process is that
    /// pane. Read once at the edge like everything else here; see
    /// [`herdr_portfwd::confirm`] for why it arrives in the environment.
    confirm: Option<ConfirmState>,
}

/// Build the real wiring and hand it to `run`.
///
/// A closure rather than a returned value because [`Actions`] borrows every
/// piece: the trait objects have to outlive it, and locals in one frame are the
/// simplest way to say so.
fn wired<T>(
    host: Option<String>,
    json: bool,
    run: impl FnOnce(&Ctx<'_>) -> Result<T>,
) -> Result<T> {
    let ssh = SshProcess::new();
    let herdr = HerdrCli::local();
    let ports = SystemPorts;
    let opener = SystemOpener;
    let store = Store::from_env().context("opening the forward state file")?;
    let requests = RequestLog::from_env().context("opening the request log")?;
    let env = Env::from_process(host)?;
    let host_herdr = HostServers {
        control_dir: env.control_dir.clone(),
        fallback: &herdr,
    };
    let ctx = Ctx {
        actions: Actions {
            ssh: &ssh,
            herdr: &herdr,
            host_herdr: &host_herdr,
            ports: &ports,
            opener: &opener,
            store: &store,
            requests: &requests,
            probe: None,
            env,
        },
        herdr: &herdr,
        json,
        stdin_terminal: io::stdin().is_terminal(),
        stdout_terminal: io::stdout().is_terminal(),
        plugin: PluginInvocation::from_env(),
        confirm: ConfirmState::from_env().map_err(|message| {
            anyhow::anyhow!("{CONFIRM_ENV} is not a click this pane can draw: {message}")
        })?,
    };
    run(&ctx)
}

/// Resolves a host to the Herdr server that owns its panes.
///
/// Under herdr-mirror the panes whose `shell_pid`s attribution walks live on
/// the host's own server, not on the local one showing mirrored copies of them,
/// so the client has to run the *remote* `herdr` over our control master. Under
/// `herdr --remote` there is no master on this side and the invoking server
/// already is the host's, so the fallback is the right answer there.
///
/// A live master is the same signal [`Actions::locate`] uses to decide
/// placement, and asking for it costs no network: `ssh -O check` only talks to
/// a local socket.
struct HostServers<'a> {
    control_dir: PathBuf,
    fallback: &'a dyn HerdrClient,
}

impl HostHerdr for HostServers<'_> {
    fn client(&self, host: &Host) -> herdr_portfwd::actions::Result<Box<dyn HerdrClient + '_>> {
        let path = control_socket_path(&self.control_dir, &host.name)?;
        let ssh = SshProcess::new();
        if matches!(
            ssh.check_master(&host.target, &path),
            Ok(MasterState::Running)
        ) {
            return Ok(Box::new(HerdrCli::remote(ssh, host.target.clone(), path)));
        }
        Ok(Box::new(BorrowedHerdr(self.fallback)))
    }
}

/// A `HerdrClient` that owns nothing, so a borrowed one can be boxed.
struct BorrowedHerdr<'a>(&'a dyn HerdrClient);

impl HerdrClient for BorrowedHerdr<'_> {
    fn pane_list(&self) -> herdr_portfwd::herdr::Result<Vec<PaneRef>> {
        self.0.pane_list()
    }

    fn pane_list_labelled(&self) -> herdr_portfwd::herdr::Result<Vec<PaneRef>> {
        self.0.pane_list_labelled()
    }

    fn shell_pid(&self, pane_id: &str) -> herdr_portfwd::herdr::Result<Option<u32>> {
        self.0.shell_pid(pane_id)
    }

    fn notify(&self, notification: &Notification) -> herdr_portfwd::herdr::Result<()> {
        self.0.notify(notification)
    }
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

/// Which of the two panes an invocation opens.
///
/// Named rather than matched inline so the mapping is something a test can
/// assert: the manifest declares both entrypoints the same way, and swapping
/// them shows up only as the wrong overlay in front of a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    /// The overlay of live forwards, with stop keys.
    Manager,
    /// The popup over attributed listeners.
    Picker,
    /// The popup a Ctrl+click lands on.
    Confirm,
}

fn pane_for(entrypoint: PaneEntrypoint) -> Pane {
    match entrypoint {
        PaneEntrypoint::List => Pane::Manager,
        PaneEntrypoint::Picker => Pane::Picker,
        PaneEntrypoint::Confirm => Pane::Confirm,
    }
}

/// The pane a cross-link key asks for.
fn handoff_pane(handoff: Handoff) -> Pane {
    match handoff {
        Handoff::List => Pane::Manager,
        Handoff::Picker => Pane::Picker,
    }
}

/// Whether the manifest declares this pane as a popup.
///
/// Mirrors `placement` in `herdr-plugin.toml`, and
/// `the_panes_this_binary_calls_popups_are_the_ones_the_manifest_declares`
/// checks it against the file rather than trusting the copy.
fn is_popup(pane: Pane) -> bool {
    match pane {
        Pane::Manager => false,
        Pane::Picker | Pane::Confirm => true,
    }
}

/// The `--placement` to pass when `from` sends the user to `to`, or `None` to
/// take the manifest's.
///
/// Herdr keeps one popup slot, and a pane handing off is still holding its own
/// until its process exits — so a popup asking for a popup is asking for the
/// slot it occupies, and gets `popup already open` back. It asks for an overlay
/// instead, which Herdr opens behind the popup and reveals when the popup goes.
/// Every other pair keeps the shape the manifest declares. See
/// [`herdr_portfwd::ui::Handoff`].
fn handoff_placement(from: Pane, to: Pane) -> Option<&'static str> {
    (is_popup(from) && is_popup(to)).then_some("overlay")
}

/// The `[[panes]] id` Herdr knows this pane by, for
/// `plugin pane open --entrypoint <id>`.
///
/// The inverse of [`pane_for`]: Herdr hands the id back as the `pane-ui`
/// subcommand when it launches the pane, so these two functions have to agree
/// or a request for one pane returns the other.
fn entrypoint_id(pane: Pane) -> &'static str {
    match pane {
        Pane::Manager => "list",
        Pane::Picker => "picker",
        Pane::Confirm => "confirm",
    }
}

/// Whether this invocation may take over the terminal.
///
/// Both ends of it, not just stdin. A pane reads keys from stdin and paints
/// frames to stdout, and [`ui::terminal_size`] measures stdout; deciding from
/// stdin alone meant `herdr-portfwd add > add.txt` from an interactive shell
/// put the terminal in raw mode, drew the picker into `add.txt` at a guessed
/// 80x24, and sat waiting for keys with nothing on screen. The user got neither
/// the report nor the refusal that names what was missing.
///
/// `--json` is never interactive however the process was started: a caller
/// parsing JSON is not sitting at a keyboard, and a pane would write a screen
/// full of drawing where the report should be.
fn interactive(json: bool, stdin_terminal: bool, stdout_terminal: bool) -> bool {
    stdin_terminal && stdout_terminal && !json
}

/// What a command does about input it was not given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route<'a> {
    /// It was told enough, or has an answer without asking: run it.
    Run,
    /// Draw the pane here, on this terminal.
    Pane,
    /// Headless, but Herdr is on the other end: ask it to open the pane, and
    /// name this plugin as `--plugin <id>`.
    AskHerdr(&'a str),
    /// Nobody to draw for and no Herdr to ask. Report what was missing.
    Refuse,
}

/// Where an invocation that may need to ask the user something goes.
///
/// Pure, and total over the three inputs, because this is the whole reason the
/// manifest's `add` and `stop` actions do anything at all: with only the first
/// input they were dead, taking the refusal path from every keybinding and
/// every command-palette entry, and no test could see it because the deciding
/// value was a terminal that tests do not have.
///
/// What Herdr injected outranks what stdin happens to be. Herdr pipes an
/// action's stdout and stderr and leaves stdin inherited, so a server running
/// in the user's foreground terminal — `herdr server`, `herdr --no-session` —
/// hands its own tty to every action it spawns. Asking `is_terminal()` first
/// drew the picker into the plugin log and then read keys off the terminal
/// Herdr was reading: the user's keystrokes disappeared, EOF never came, and
/// the action held one of Herdr's in-flight command slots forever.
fn route<'a>(
    interactive: bool,
    nothing_was_named: bool,
    plugin: &'a PluginInvocation,
) -> Route<'a> {
    if !nothing_was_named {
        // The user already named a port; there is nothing left to ask, even at
        // a terminal.
        return Route::Run;
    }
    if let Some(plugin_id) = plugin.pane_to_open() {
        return Route::AskHerdr(plugin_id);
    }
    if interactive {
        return Route::Pane;
    }
    Route::Refuse
}

/// Where a `scan` invocation goes.
///
/// `scan` never names a port, so something is always left to pick and [`route`]
/// decides it — with one exception. `scan` alone has a complete answer without
/// asking anybody, and `--json` names a caller that wants exactly that answer
/// parsed: a pane in front of it would be a screenful of drawing where the
/// report should be, and there is nobody at that end to drive it anyway.
///
/// What this is *not* allowed to do is treat a headless plugin action as a
/// caller that wants the report. `[[actions]] id = "scan"` is the manifest's
/// flagship keybinding — "shows attributed listeners and forwards the ones you
/// pick" — and Herdr runs an action with both streams captured into the plugin
/// log. Printing the report there told nobody anything: the person who pressed
/// the key saw no picker, no listeners, and no error. An action's stdout is a
/// log, not a person, so it asks Herdr for the pane exactly like `add` and
/// `stop` do.
fn scan_route<'a>(json: bool, interactive: bool, plugin: &'a PluginInvocation) -> Route<'a> {
    if json {
        return Route::Run;
    }
    route(interactive, true, plugin)
}

/// Where an `open-url` invocation goes.
///
/// A Ctrl+click used to forward the port and hand the URL to a browser in one
/// motion, with no screen in between. It now lands on the `confirm` pane, and
/// the pane performs whatever the user picks — so this decides who, if anybody,
/// is there to be asked.
///
/// The exceptions are `scan`'s exactly, and for the same reasons. `--json`
/// names a caller that wants the outcome parsed, not a screen. A shell
/// invocation with no Herdr to ask has nowhere to put a pane, and unlike `add`
/// it was told everything it needs — so it does what it was asked to do rather
/// than refusing. And an interactive terminal draws the screen itself, because
/// that is what invariant 10 says a terminal is for.
///
/// `confirm_on_click = false` short-circuits all of it: the user has said they
/// want the old one-motion behaviour, and every branch here is about who sees
/// the screen they turned off.
fn open_url_route<'a>(
    json: bool,
    interactive: bool,
    confirm_on_click: bool,
    plugin: &'a PluginInvocation,
) -> Route<'a> {
    if !confirm_on_click || json {
        return Route::Run;
    }
    // `nothing_was_named` is true because the question is never "which port" —
    // it is "should this happen at all", and only a person can answer that.
    // The recursion guard rides along: a pane command gets `None` from
    // `pane_to_open`, so the confirm pane can never ask for another confirm.
    route(interactive, true, plugin)
}

/// Where a `pane-ui` invocation goes.
///
/// It already names a pane, so nothing is missing — but `pane-ui list` is
/// declared twice in the manifest: as the `list` **pane**, and as the
/// "Port: manage forwards" **action**. Herdr runs an action headless with both
/// output streams captured, so drawing there paints one frame of the overlay
/// into the plugin log, hits EOF, and exits 0 without a user ever seeing it —
/// the same dead action [`route`] fixes for `add` and `stop`. So only the
/// action asks; the pane Herdr then launches carries
/// `HERDR_PLUGIN_ENTRYPOINT_ID`, draws, and cannot ask again.
fn pane_route<'a>(interactive: bool, plugin: &'a PluginInvocation) -> Route<'a> {
    if let Some(plugin_id) = plugin.pane_to_open() {
        return Route::AskHerdr(plugin_id);
    }
    // A pane Herdr launched draws whatever its streams are: Herdr owns them,
    // and it is the one that asked for this pane.
    if interactive || plugin.kind() == Invoked::Pane {
        return Route::Pane;
    }
    Route::Refuse
}

impl Ctx<'_> {
    fn dispatch(&self, command: Command) -> Result<ExitCode> {
        match command {
            Command::Attach { host, herdr_args } => self.attach(&host, herdr_args),
            Command::LocalAgent { host, oneshot } => self.local_agent(&host, oneshot),
            Command::Scan => self.scan(),
            Command::Add { port, local_port } => self.add(port, local_port),
            Command::List => self.list(),
            Command::Stop { port } => self.stop(port),
            Command::StopAll => self.stop_all(),
            Command::OpenUrl { url } => self.open_url(url.as_deref()),
            Command::Reconcile { startup, event } => self.reconcile(startup, event),
            Command::PaneUi { entrypoint } => self.pane_ui(entrypoint),
            Command::Status => self.status(),
        }
    }

    /// True when this invocation may take over the terminal.
    fn interactive(&self) -> bool {
        interactive(self.json, self.stdin_terminal, self.stdout_terminal)
    }

    /// Where this invocation goes when it was not told which port to act on.
    fn route(&self, nothing_was_named: bool) -> Route<'_> {
        route(self.interactive(), nothing_was_named, &self.plugin)
    }

    /// Ask Herdr to open one of the declared panes, because this process has no
    /// terminal to draw one on.
    ///
    /// The pane Herdr launches gets `HERDR_PLUGIN_ENTRYPOINT_ID`, which is what
    /// stops it from arriving back here and asking for another one.
    fn ask_herdr(&self, plugin_id: &str, pane: Pane) -> Result<ExitCode> {
        self.ask_herdr_with(plugin_id, pane, None, &[])
    }

    /// [`Ctx::ask_herdr`], with a placement override and environment for the
    /// pane. Only two callers pass either: a click, which has a payload the
    /// pane cannot otherwise be told, and a pane handing off to another one.
    fn ask_herdr_with(
        &self,
        plugin_id: &str,
        pane: Pane,
        placement: Option<&str>,
        env: &[String],
    ) -> Result<ExitCode> {
        let entrypoint = entrypoint_id(pane);
        self.herdr
            .open_pane_with(plugin_id, entrypoint, placement, env)
            .with_context(|| format!("asking Herdr to open the {entrypoint} pane"))?;
        let report = serde_json::json!({
            "opened_pane": entrypoint,
            "plugin": plugin_id,
        });
        self.emit(&report, |_| format!("opening the {entrypoint} pane\n"))?;
        Ok(ExitCode::SUCCESS)
    }

    /// The plugin id to name when talking back to Herdr. Herdr's own wins, so a
    /// plugin registered under some other id still opens its own panes.
    fn plugin_id(&self) -> &str {
        self.plugin
            .plugin_id
            .as_deref()
            .unwrap_or(herdr_portfwd::PLUGIN_ID)
    }

    /// Render a declared pane — or, when Herdr ran this argv as an *action*
    /// rather than as the pane, ask it to open the pane properly.
    fn pane_ui(&self, entrypoint: PaneEntrypoint) -> Result<ExitCode> {
        let pane = pane_for(entrypoint);
        match pane_route(self.interactive(), &self.plugin) {
            Route::AskHerdr(plugin_id) => self.ask_herdr(plugin_id, pane),
            Route::Pane => self.open(pane),
            // Nowhere to draw and nobody to ask. Drawing anyway would write a
            // screenful of escape sequences into whatever captured stdout and
            // exit 0, which reads as success and shows the user nothing.
            Route::Run | Route::Refuse => {
                let id = entrypoint_id(pane);
                bail!(
                    "the {id} pane has no terminal to draw on: run it from a terminal, \
                     or let Herdr open it with `herdr plugin pane open --plugin {} \
                     --entrypoint {id}`",
                    herdr_portfwd::PLUGIN_ID
                )
            }
        }
    }

    fn open(&self, pane: Pane) -> Result<ExitCode> {
        match pane {
            Pane::Manager => self.manager_pane(),
            Pane::Picker => self.picker_pane(),
            // The click is in the environment Herdr was asked to inject; a
            // confirm pane without one has nothing to confirm.
            Pane::Confirm => match self.confirm.clone() {
                Some(state) => self.confirm_pane(state),
                None => bail!(
                    "the confirm pane was opened with no click to confirm: \
                     {CONFIRM_ENV} was not set. Ctrl+click a localhost URL, or run \
                     `herdr-portfwd open-url <url>`"
                ),
            },
        }
    }

    fn emit<T: Serialize>(&self, value: &T, human: impl FnOnce(&T) -> String) -> Result<()> {
        let text = if self.json {
            let mut text =
                serde_json::to_string_pretty(value).context("serializing the report as JSON")?;
            text.push('\n');
            text
        } else {
            human(value)
        };
        write_out(&text);
        Ok(())
    }

    // -- attach / local-agent -----------------------------------------------

    fn attach(&self, name: &str, herdr_args: Vec<String>) -> Result<ExitCode> {
        let host = named_host(&self.actions.env.hosts, name)?;
        let outcome = local_agent::attach_main(host, herdr_args)?;
        if !outcome.agent_started {
            // Not fatal — the session itself ran — but the agent was the whole
            // point of going through `attach`, so it must not vanish silently.
            let mut err = io::stderr().lock();
            let _ = writeln!(
                err,
                "herdr-portfwd: the port-forward agent never started; \
                 forwards requested from the remote were not applied"
            );
        }
        // The session's own output already went to the terminal; only `--json`
        // adds anything of its own.
        let summary = serde_json::json!({
            "status": outcome.status,
            "signal": outcome.signal,
            "agent_started": outcome.agent_started,
        });
        self.emit(&summary, |_| String::new())?;
        Ok(exit_code_for(outcome.status, outcome.signal))
    }

    fn local_agent(&self, name: &str, oneshot: bool) -> Result<ExitCode> {
        let host = named_host(&self.actions.env.hosts, name)?;
        let summary = local_agent::local_agent_main(host, oneshot)?;
        let value = serde_json::json!({
            "reason": format!("{:?}", summary.reason),
            "steps": summary.steps,
            "applied": summary.applied,
            "offered": summary.offered,
            "auto": summary.auto,
        });
        self.emit(&value, |_| {
            format!(
                "agent stopped after {} passes: {} applied, {} offered, {} auto\n",
                summary.steps, summary.applied, summary.offered, summary.auto
            )
        })?;
        Ok(ExitCode::SUCCESS)
    }

    // -- the actions --------------------------------------------------------

    fn scan(&self) -> Result<ExitCode> {
        match scan_route(self.json, self.interactive(), &self.plugin) {
            Route::Pane => return self.open(Pane::Picker),
            Route::AskHerdr(plugin_id) => return self.ask_herdr(plugin_id, Pane::Picker),
            // A shell invocation with nowhere to draw still has a complete
            // answer to print: unlike `add`, `scan` was not missing anything.
            Route::Run | Route::Refuse => {}
        }
        let report = self.actions.scan()?;
        self.emit(&report, render_scan)?;
        Ok(ExitCode::SUCCESS)
    }

    fn add(&self, port: Option<u16>, local_port: Option<u16>) -> Result<ExitCode> {
        match self.route(port.is_none() && local_port.is_none()) {
            Route::Pane => return self.open(Pane::Picker),
            Route::AskHerdr(plugin_id) => return self.ask_herdr(plugin_id, Pane::Picker),
            // `add` with no port has nothing to forward, so this reaches
            // `ActionError::NoPort` and its refusal.
            Route::Run | Route::Refuse => {}
        }
        let outcome = self.actions.add(port, local_port)?;
        self.emit(&outcome, render_add)?;
        Ok(ExitCode::SUCCESS)
    }

    fn list(&self) -> Result<ExitCode> {
        let report = self.actions.list()?;
        self.emit(&report, render_list)?;
        Ok(ExitCode::SUCCESS)
    }

    fn stop(&self, port: Option<u16>) -> Result<ExitCode> {
        // The manager overlay, not the picker: stopping is picking from what is
        // already live, and that pane is the one with the stop keys.
        match self.route(port.is_none()) {
            Route::Pane => return self.open(Pane::Manager),
            Route::AskHerdr(plugin_id) => return self.ask_herdr(plugin_id, Pane::Manager),
            Route::Run | Route::Refuse => {}
        }
        let outcome = self.actions.stop(port)?;
        self.emit(&outcome, render_stop)?;
        Ok(ExitCode::SUCCESS)
    }

    fn stop_all(&self) -> Result<ExitCode> {
        let outcome = self.actions.stop_all()?;
        self.emit(&outcome, render_stop)?;
        Ok(ExitCode::SUCCESS)
    }

    fn open_url(&self, url: Option<&str>) -> Result<ExitCode> {
        match open_url_route(
            self.json,
            self.interactive(),
            self.actions.env.settings.confirm_on_click,
            &self.plugin,
        ) {
            Route::AskHerdr(plugin_id) => {
                let state = self.actions.preview_open_url(url)?;
                // No placement override: `open-url` is an action, not a popup,
                // so the only popup slot is free and the manifest's shape is
                // the right one.
                let env = confirm_env(&state)?;
                match self.ask_herdr_with(plugin_id, Pane::Confirm, None, &env) {
                    Ok(code) => return Ok(code),
                    // Herdr refused the pane. The commonest reason is the one
                    // popup slot already being held — a picker or a previous
                    // confirmation is up — and it is not the user's mistake.
                    // Stopping here made the Ctrl+click a complete no-op whose
                    // only trace was a plugin log nobody reads. Invariant 10
                    // already settles what to do when the screen cannot be
                    // shown: act on the click, because a screen nobody can see
                    // is worse than the forward the user asked for. So say why,
                    // where a person will see it, and fall through.
                    Err(err) => self.confirmation_unavailable(&err),
                }
            }
            Route::Pane => {
                let state = self.actions.preview_open_url(url)?;
                return self.confirm_pane(state);
            }
            // Either the user turned the screen off, or there is nobody to show
            // it to and nothing was missing: do what the click asked for.
            Route::Run | Route::Refuse => {}
        }
        let outcome = self.actions.open_url(url)?;
        self.emit(&outcome, render_open)?;
        Ok(ExitCode::SUCCESS)
    }

    /// Say that the confirmation could not be shown, on both channels a plugin
    /// command has.
    ///
    /// stderr is the plugin log, which is where the reason belongs; a Herdr
    /// notification is the only one of the two a person sees without going
    /// looking. Both are best effort: the click is about to be acted on, and it
    /// must not fail because a toast did not land.
    fn confirmation_unavailable(&self, err: &anyhow::Error) {
        let mut errout = io::stderr().lock();
        let _ = writeln!(
            errout,
            "herdr-portfwd: {err:#}; acting on the click without asking first"
        );
        let _ = self.herdr.notify(&Notification {
            title: "Port forward: no confirmation screen".to_string(),
            body: "Herdr would not open the confirmation, so the clicked URL is being \
                   forwarded without it."
                .to_string(),
        });
    }

    fn reconcile(&self, startup: bool, event: bool) -> Result<ExitCode> {
        // Startup wins when both are set: it is the permissive trigger, and the
        // one that may dial. Guessing the restrictive one would silently skip
        // restoring forwards.
        let trigger = match (startup, event) {
            (true, _) => ReconcileTrigger::Startup,
            (false, true) => ReconcileTrigger::Event,
            (false, false) => ReconcileTrigger::Manual,
        };
        let report = self.actions.reconcile(trigger)?;
        self.emit(&report, render_reconcile)?;
        Ok(ExitCode::SUCCESS)
    }

    /// The one command that must not become the failure it is reporting.
    ///
    /// A file it could not read is data in the report — named, with everything
    /// that *was* readable printed beside it — and `--json` carries the same
    /// fields rather than an empty stdout. The exit code is still the tool's
    /// own: something went wrong, so this run failed, and the reason is on
    /// stderr where every other failure puts it and where
    /// `herdr plugin log list` reads it.
    fn status(&self) -> Result<ExitCode> {
        let report = self.actions.status()?;
        self.emit(&report, render_status)?;
        let failures = report.failures();
        if failures.is_empty() {
            return Ok(ExitCode::SUCCESS);
        }
        let mut err = io::stderr().lock();
        for failure in failures {
            let _ = writeln!(err, "herdr-portfwd: {}", ui::sanitize(failure));
        }
        Ok(ExitCode::from(EXIT_FAILURE))
    }

    // -- panes --------------------------------------------------------------

    fn manager_pane(&self) -> Result<ExitCode> {
        let mut app = self.manager();
        // Every row, plus whatever `--host` narrowed to as the filter to open
        // with. Never a narrowed listing: the header describes the filter, so
        // rows the filter does not know about would make it describe a scope
        // the table does not have.
        let (view, status) = split_initial(app.forward_view());
        // A listing that never arrived is not a world with no forwards in it.
        // The pane opens on what it knows — nothing — and says that, because
        // "No forwards are live on any host." over an unresolvable `--host`
        // was a positive claim about every machine this process never reached.
        let (rows, filter, listing) = match view {
            Some((rows, filter)) => (rows, filter, Listing::Fetched),
            None => (Vec::new(), HostFilter::All, Listing::Unavailable),
        };
        let (width, height) = ui::terminal_size();
        let snapshot = ListSnapshot {
            rows,
            listing,
            filter,
            status,
            width,
            height,
            ..ListSnapshot::default()
        };
        let outcome = ui::open_list(&mut app, snapshot).context("running the forward manager")?;
        self.finish_pane(Pane::Manager, &outcome)
    }

    fn picker_pane(&self) -> Result<ExitCode> {
        let mut app = self.manager();
        let (listeners, status) = split_initial(app.listener_rows());
        // Same rule as the manager: a scan that did not run has not established
        // that no listener here belongs to a pane.
        let (rows, listing) = match listeners {
            Some(rows) => (rows, Listing::Fetched),
            None => (Vec::new(), Listing::Unavailable),
        };
        let (width, height) = ui::terminal_size();
        // A failed resolve is already reported through `status`; the header just
        // loses its host name rather than the pane refusing to open.
        let host = self
            .actions
            .resolve()
            .map(|resolved| resolved.host.name)
            .unwrap_or_default();
        let snapshot = PickerSnapshot {
            host,
            rows,
            listing,
            status,
            width,
            height,
            ..PickerSnapshot::default()
        };
        let outcome = ui::open_picker(&mut app, snapshot).context("running the port picker")?;
        self.finish_pane(Pane::Picker, &outcome)
    }

    fn confirm_pane(&self, state: ConfirmState) -> Result<ExitCode> {
        let (width, height) = ui::terminal_size();
        let snapshot = ConfirmSnapshot {
            state: state.clone(),
            status: None,
            width,
            height,
            color: true,
        };
        let mut app = Confirm {
            actions: &self.actions,
            state: &state,
        };
        let outcome = ui::open_confirm(&mut app, snapshot).context("running the confirmation")?;
        self.finish_pane(Pane::Confirm, &outcome)
    }

    fn manager(&self) -> Manager<'_> {
        Manager {
            actions: &self.actions,
            herdr: self.herdr,
        }
    }

    /// Leave the pane's last status in the plugin log — the pane itself is gone
    /// by the time this runs — open whatever pane the user asked to go to next,
    /// and turn the pane's exit into a process exit.
    ///
    /// The order matters: this process exiting is what closes the pane it just
    /// ran, so the request for the next one has to be made first. See
    /// [`herdr_portfwd::ui::Handoff`].
    fn finish_pane(&self, from: Pane, outcome: &ui::Outcome) -> Result<ExitCode> {
        if let Some(status) = &outcome.status {
            // The pane sanitized this for the screen; stderr is a second
            // terminal that will render it, and the text can carry an ssh
            // failure verbatim.
            let mut err = io::stderr().lock();
            let _ = writeln!(err, "herdr-portfwd: {}", ui::sanitize(&status.text));
        }
        if let Some(handoff) = outcome.handoff {
            let to = handoff_pane(handoff);
            let entrypoint = entrypoint_id(to);
            self.herdr
                .open_pane_with(
                    self.plugin_id(),
                    entrypoint,
                    handoff_placement(from, to),
                    &[],
                )
                .with_context(|| format!("asking Herdr to open the {entrypoint} pane"))?;
        }
        Ok(match outcome.exit {
            Exit::Closed => ExitCode::SUCCESS,
            Exit::Interrupted => ExitCode::from(EXIT_INTERRUPTED),
        })
    }
}

/// The `--env` words that tell the confirm pane which click it is confirming.
///
/// The host rides along as `HERDR_PORTFWD_HOST` — the variable `--host` already
/// reads — so the pane resolves the same host this process did rather than
/// re-deriving it from a context that has moved on. A click with no host to
/// name sends none, and the pane says so instead of guessing.
///
/// The placement rides along with it, and has to. Naming the host explicitly
/// resolves it as [`herdr_portfwd::context::HostSource::Explicit`], and
/// `locate_resolved` reads `WorkspacePrefix` as the mirror signal — so the one
/// variable that stops the pane guessing the *host* would also, silently, turn
/// off the `MirrorWorkspace` rule that decided *where* this very click applies.
/// The pane would then re-derive placement from a popup context that has no
/// pane identity and whatever workspace happens to be focused, and could queue
/// a request for a forward it is standing on the machine to make. Placement is
/// decided once, by the click, and the pane inherits the answer through
/// [`herdr_portfwd::actions::MODE_ENV`] rather than working it out again from
/// worse evidence.
fn confirm_env(state: &ConfirmState) -> Result<Vec<String>> {
    let mut env = vec![state
        .env_assignment()
        .map_err(|message| anyhow::anyhow!("describing the click for the pane: {message}"))?];
    if let Some(host) = &state.host {
        env.push(format!("HERDR_PORTFWD_HOST={host}"));
        let mode = if state.applies_here {
            "local"
        } else {
            "remote"
        };
        env.push(format!("{MODE_ENV}={mode}"));
    }
    Ok(env)
}

/// Rows for the first frame, with a failure turned into a status line instead
/// of a refusal to open: a pane that will not open cannot show why.
///
/// The absence is handed back as an absence rather than swallowed into a
/// default, because `T::default()` for a table is an empty table and an empty
/// table is an answer — the wrong one. Deciding what to do with nothing belongs
/// to the caller building the snapshot, which is the one place that can mark it
/// as [`herdr_portfwd::ui::Listing::Unavailable`] rather than as a world with
/// no forwards in it.
fn split_initial<T>(value: std::result::Result<T, String>) -> (Option<T>, Option<Status>) {
    match value {
        Ok(value) => (Some(value), None),
        Err(message) => (None, Some(Status::failed(message))),
    }
}

// ---------------------------------------------------------------------------
// the panes' callbacks
// ---------------------------------------------------------------------------

/// What a pane's keys do. The UI renders and reads; every effect goes back
/// through [`Actions`], so a pane can forward exactly what an explicit `add`
/// can and nothing more.
struct Manager<'a> {
    actions: &'a Actions<'a>,
    herdr: &'a HerdrCli<LocalHerdr>,
}

impl Manager<'_> {
    /// Panes on the invoking server, for workspace labels. Best effort: a label
    /// that could not be fetched costs a column, and must not empty the table.
    fn panes(&self) -> Vec<PaneRef> {
        self.herdr.pane_list_labelled().unwrap_or_default()
    }

    /// What the manager opens with: every live forward on every host, and the
    /// filter the pane starts narrowed to.
    ///
    /// One call for both, so the rows and the filter cannot come from two
    /// listings that disagree about scope — which is exactly what a header
    /// saying "all hosts" over one host's rows was. `--host` reaches the pane
    /// here and nowhere else: it is the same narrowing `h` performs, preset.
    fn forward_view(&self) -> std::result::Result<(Vec<ForwardRow>, HostFilter), String> {
        let listing = self
            .actions
            .list_for_pane()
            .map_err(|err| err.to_string())?;
        let panes = self.panes();
        let rows = listing
            .report
            .hosts
            .iter()
            .flat_map(|host| host.forwards.iter())
            .map(|status| {
                let pane = ui::find_pane(&panes, status.forward.spec.pane_id.as_deref());
                // `status.live` travels with the record. Dropping it here is
                // what made the pane count every record as a live forward and
                // draw a dead one exactly like an open port.
                ForwardRow::from_forward(&status.forward, pane, status.live)
            })
            .collect();
        let filter = match listing.focus.host() {
            Some(name) => HostFilter::Host(name.to_string()),
            None => HostFilter::All,
        };
        Ok((rows, filter))
    }

    /// Rows only, for a refresh: `r` and every action reload the table without
    /// touching the filter the user is looking through.
    fn forward_rows(&self) -> std::result::Result<Vec<ForwardRow>, String> {
        self.forward_view().map(|(rows, _)| rows)
    }

    fn listener_rows(&self) -> std::result::Result<Vec<ListenerRow>, String> {
        let report = self.actions.scan().map_err(|err| err.to_string())?;
        Ok(report
            .candidates
            .into_iter()
            .map(|candidate| {
                let port = candidate.listener.port;
                let already = candidate.already_forwarded;
                let attributed = AttributedListener {
                    listener: candidate.listener,
                    pane: candidate.pane,
                };
                // Same number on both ends: the picker offers no remap, because
                // a remap has to be asked for and never implied.
                ListenerRow::from_attributed(&attributed, port, already)
            })
            .collect())
    }
}

/// A row as the store names it. The pane acts on what it is showing, so the
/// host travels with the ports rather than being re-derived from the
/// invocation — which is what made `enter` miss a forward on any other host.
fn row_key(row: &ForwardRow) -> ForwardKey {
    ForwardKey {
        host: row.host.clone(),
        remote_port: row.remote_port,
        local_port: row.local_port,
    }
}

impl ListActions for Manager<'_> {
    fn stop(&mut self, row: &ForwardRow) -> std::result::Result<String, String> {
        let outcome = self
            .actions
            .stop_rows(&[row_key(row)])
            .map_err(|err| err.to_string())?;
        Ok(stop_summary(&outcome))
    }

    fn stop_shown(&mut self, rows: &[ForwardRow]) -> std::result::Result<String, String> {
        let keys: Vec<ForwardKey> = rows.iter().map(row_key).collect();
        let outcome = self
            .actions
            .stop_rows(&keys)
            .map_err(|err| err.to_string())?;
        Ok(stop_summary(&outcome))
    }

    fn rows(&mut self) -> std::result::Result<Vec<ForwardRow>, String> {
        self.forward_rows()
    }
}

impl PickerActions for Manager<'_> {
    fn forward(&mut self, rows: &[ListenerRow]) -> std::result::Result<String, String> {
        let mut forwarded: Vec<u16> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        for row in rows {
            match self.actions.add(Some(row.remote_port), None) {
                Ok(outcome) => forwarded.push(outcome.spec.local_port),
                Err(err) => failed.push(format!("{}: {err}", row.remote_port)),
            }
        }
        if failed.is_empty() {
            return Ok(format!("forwarded {}", ports(&forwarded)));
        }
        // Partial success is reported with the successes named: the user has to
        // know which ports are now live before deciding what to do about the
        // rest.
        let mut message = failed.join("; ");
        if !forwarded.is_empty() {
            message = format!("forwarded {}, but {message}", ports(&forwarded));
        }
        Err(message)
    }

    fn rows(&mut self) -> std::result::Result<Vec<ListenerRow>, String> {
        self.listener_rows()
    }
}

/// What the `confirm` pane's keys do.
///
/// The state is the one the asking side encoded, not one this process derived:
/// the URL, and the pane the URL was printed in. `open_url_for` re-validates
/// the URL anyway (invariant 11) — under `herdr --remote` it made a round trip
/// through an environment variable on the host whose terminal wrote it.
struct Confirm<'a> {
    actions: &'a Actions<'a>,
    state: &'a ConfirmState,
}

impl ConfirmActions for Confirm<'_> {
    fn forward(&mut self, open_browser: bool) -> std::result::Result<String, String> {
        let outcome = self
            .actions
            .open_url_for(&self.state.url, self.state.pane_id.clone(), open_browser)
            .map_err(|err| err.to_string())?;
        Ok(open_summary(&outcome, open_browser))
    }
}

/// One status line for a finished confirmation.
///
/// Says who did what, because the three cases are genuinely different machines:
/// the forward happened here, it was already up, or it is a record the local
/// agent will act on — and in that last case the browser is the agent's job,
/// not this pane's.
fn open_summary(outcome: &OpenOutcome, open_browser: bool) -> String {
    let spec = &outcome.add.spec;
    let what = mapping(&spec.host, spec.remote_port, spec.local_port);
    match outcome.add.applied {
        Applied::Requested if open_browser => {
            format!("requested {what}; the local agent forwards it and opens the page")
        }
        Applied::Requested => format!("requested {what}; the local agent applies it"),
        Applied::Forwarded if outcome.opened => {
            format!("forwarded {what}; opened {}", outcome.url)
        }
        Applied::Forwarded => format!("forwarded {what}"),
        Applied::AlreadyLive if outcome.opened => {
            format!("already live: {what}; opened {}", outcome.url)
        }
        Applied::AlreadyLive => format!("already live: {what}"),
    }
}

fn ports(ports: &[u16]) -> String {
    ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a stop did, host by host.
///
/// The empty case has to say *where* it looked. "nothing to stop on
/// mac-mini-honkoma" while the port was live on mac-studio-honkoma is the
/// report that sent a user hunting for a bug in `ssh`, so a search that covered
/// every host says so, and one that was narrowed names the host that does carry
/// the port.
fn stop_summary(outcome: &StopOutcome) -> String {
    if outcome.stopped.is_empty() {
        // Invariant 4: nothing to cancel is a no-op, so this is a status line
        // and not an error.
        let mut text = match outcome.hosts.as_slice() {
            [] => "nothing to stop: no forward on any host".to_string(),
            hosts => format!("nothing to stop on {}", hosts.join(", ")),
        };
        if !outcome.elsewhere.is_empty() {
            text.push_str(&format!(
                "; that port is forwarded on {}, so name it with --host",
                outcome.elsewhere.join(", ")
            ));
        }
        return text;
    }
    // Grouped, in the order the hosts were acted on: "stopped 5178, 8788"
    // across two machines names neither of them.
    let mut groups: Vec<(&str, Vec<u16>)> = Vec::new();
    for forward in &outcome.stopped {
        let host = forward.spec.host.as_str();
        match groups.iter_mut().find(|(name, _)| *name == host) {
            Some((_, ports)) => ports.push(forward.spec.remote_port),
            None => groups.push((host, vec![forward.spec.remote_port])),
        }
    }
    let stopped: Vec<String> = groups
        .iter()
        .map(|(host, on_host)| format!("{} on {host}", ports(on_host)))
        .collect();
    format!("stopped {}", stopped.join("; "))
}

// ---------------------------------------------------------------------------
// host naming
// ---------------------------------------------------------------------------

/// The host a command named on its own command line.
///
/// Falls back to using the name as an ssh target when `hosts.toml` does not
/// list it. `attach buildbox` has nothing to disambiguate — the user pointed at
/// one host — and demanding a config file to reach an ssh alias they already
/// have would be a second place to keep the same name. `attach` re-invokes this
/// binary as `local-agent <name>`, so both go through this one function and
/// cannot disagree about what the name meant.
fn named_host(hosts: &HostsConfig, name: &str) -> Result<Host> {
    validate_host_name(name)?;
    Ok(hosts.by_name(name).cloned().unwrap_or_else(|| Host {
        name: name.to_string(),
        target: name.to_string(),
        // No prefix: nothing claims this name labels a mirrored workspace.
        prefix: None,
    }))
}

/// A host name becomes a filename: `<dir>/<name>.ctl` is the control socket.
/// A name carrying a path separator would put that socket somewhere other than
/// the directory whose length invariant 8 checked.
fn validate_host_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("the host name is empty");
    }
    if name.contains('/') || name.contains('\0') {
        bail!("host name {name:?} cannot be used: it would not be a single path component");
    }
    if name == "." || name == ".." {
        bail!("host name {name:?} cannot be used: it names a directory, not a host");
    }
    Ok(())
}

/// Exit the way the process `attach` supervised exited.
///
/// A signalled child becomes `128 + signal`, the shell convention, so a session
/// killed with SIGTERM does not read as an ordinary failure. Anything that does
/// not fit a `u8` — a negative placeholder status, an out-of-range signal —
/// falls back to a plain failure rather than wrapping into a lie.
fn exit_code_for(status: i32, signal: Option<i32>) -> ExitCode {
    if let Some(signal) = signal {
        return ExitCode::from(u8::try_from(128 + signal).unwrap_or(EXIT_FAILURE));
    }
    ExitCode::from(u8::try_from(status).unwrap_or(EXIT_FAILURE))
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------
//
// Pure `&Report -> String`, so every line a plugin log will carry is testable
// without a process, a terminal, or a filesystem.

fn origin_label(origin: Origin) -> &'static str {
    match origin {
        Origin::Explicit => "explicit",
        Origin::Offered => "offered",
        Origin::Auto => "auto",
    }
}

fn liveness_label(live: Liveness) -> &'static str {
    match live {
        Liveness::Live => "live",
        Liveness::Missing => "missing",
        Liveness::Unknown => "unknown",
    }
}

fn placement_label(placement: &Where) -> String {
    let side = match placement.placement {
        Placement::Local => "local",
        Placement::Remote => "remote",
    };
    let reason = match placement.reason {
        PlacementReason::Forced => "HERDR_PORTFWD_MODE",
        PlacementReason::MirrorWorkspace => "mirrored workspace",
        PlacementReason::LiveMaster => "live control master",
        PlacementReason::MirroredPane => "mirrored pane on this server",
        PlacementReason::Assumed => "assumed",
    };
    format!("{side} ({reason})")
}

/// Why a record went away.
///
/// The two that carry text carry `ssh`'s stderr, which is the remote sshd's
/// own output — the most attacker-influenced string in the program — so it goes
/// through [`ui::sanitize`] like every other value that came off a host. A
/// report read in a terminal must not be able to drive it.
fn drop_reason_label(reason: &DropReason) -> String {
    match reason {
        DropReason::HostGone => "host is no longer configured".to_string(),
        DropReason::PaneGone => "owning pane is gone".to_string(),
        DropReason::MasterGone => "no control master".to_string(),
        DropReason::Superseded => "another process changed it while this pass ran".to_string(),
        DropReason::Unreachable(why) => format!("unreachable: {}", ui::sanitize(why)),
        DropReason::ReAddFailed(why) => format!("could not be re-added: {}", ui::sanitize(why)),
    }
}

/// `5173 -> 5173`, with the remap called out, because a remapped port is what
/// breaks anything that embeds it.
fn mapping(host: &str, remote_port: u16, local_port: u16) -> String {
    let mut text = format!("{host}:{remote_port} -> localhost:{local_port}");
    if remote_port != local_port {
        text.push_str(" (remapped; URLs that embed the port will not work)");
    }
    text
}

fn render_add(outcome: &AddOutcome) -> String {
    let spec = &outcome.spec;
    let what = mapping(&spec.host, spec.remote_port, spec.local_port);
    match outcome.applied {
        Applied::Forwarded => format!("forwarded {what}\n"),
        Applied::AlreadyLive => format!("already live: {what}\n"),
        Applied::Requested => {
            let id = outcome
                .request_id
                .map(|id| format!(" as request {id}"))
                .unwrap_or_default();
            format!(
                "requested {what}{id}\n\
                 this Herdr server is not on the machine that opens the port; \
                 the local agent applies it\n"
            )
        }
    }
}

fn render_open(outcome: &OpenOutcome) -> String {
    let mut text = render_add(&outcome.add);
    if outcome.opened {
        text.push_str(&format!("opened {}\n", outcome.url));
    } else {
        text.push_str(&format!(
            "not opened here: point a browser on your machine at {}\n",
            outcome.url
        ));
    }
    text
}

/// The listing, host by host, under a first line that says how much of the
/// world it is.
///
/// That first line is the honest replacement for a per-host header on a
/// listing that is no longer per host: with one section it still read as the
/// whole truth, which is exactly how a forward on a second machine stayed
/// invisible.
fn render_list(report: &ListReport) -> String {
    let total = report.total();
    let mut text = match &report.scope {
        Scope::Host(name) => {
            format!("{total} forward(s) on {name} only (--host); other hosts not shown\n")
        }
        Scope::AllHosts if report.hosts.is_empty() => "no forwards on any host\n".to_string(),
        Scope::AllHosts => format!(
            "{total} forward(s) on {} host(s); every host, --host narrows\n",
            report.hosts.len()
        ),
    };
    for host in &report.hosts {
        let master = match host.master_running {
            Some(true) => " (master running)",
            Some(false) => " (no master)",
            None => "",
        };
        text.push_str(&format!(
            "{}: {} forward(s){master}, {}\n",
            host.host,
            host.forwards.len(),
            placement_label(&host.placement)
        ));
        for status in &host.forwards {
            let spec = &status.forward.spec;
            text.push_str(&format!(
                "  {:>5} -> {:<5}  {:<7}  {:<8}{}\n",
                spec.remote_port,
                spec.local_port,
                liveness_label(status.live),
                origin_label(spec.origin),
                // A pane id under `--remote` arrives in a request the remote
                // side wrote, so it is no more trustworthy than a workspace
                // label.
                spec.pane_id
                    .as_deref()
                    .map(|id| format!("  pane {}", ui::sanitize(id)))
                    .unwrap_or_default(),
            ));
        }
        if !host.pending.is_empty() {
            text.push_str(&format!(
                "  {} request(s) waiting for the local agent: {}\n",
                host.pending.len(),
                ports(
                    &host
                        .pending
                        .iter()
                        .map(|request| request.remote_port)
                        .collect::<Vec<_>>()
                )
            ));
        }
    }
    text
}

fn render_stop(outcome: &StopOutcome) -> String {
    format!("{}\n", stop_summary(outcome))
}

fn render_scan(report: &ScanReport) -> String {
    let mut text = format!(
        "{}: {} candidate(s), {} unattributed, {} not forwardable, {}\n",
        report.host,
        report.candidates.len(),
        report.unattributed,
        report.unforwardable,
        placement_label(&report.placement)
    );
    for candidate in &report.candidates {
        let listener = &candidate.listener;
        let workspace = candidate
            .pane
            .as_ref()
            .and_then(|pane| pane.workspace_label.clone())
            .or_else(|| {
                candidate
                    .pane
                    .as_ref()
                    .map(|pane| pane.workspace_id.clone())
            })
            .unwrap_or_else(|| "-".to_string());
        let agent = candidate
            .pane
            .as_ref()
            .and_then(|pane| pane.agent.clone())
            .unwrap_or_else(|| "-".to_string());
        let mut notes: Vec<&str> = Vec::new();
        if candidate.already_forwarded {
            notes.push("already forwarded");
        }
        // `None` means the scan ran on the far side and nothing here knows what
        // the user's machine has bound. Saying nothing beats saying the
        // opposite of the truth.
        if candidate.local_port_free == Some(false) {
            notes.push("local port in use");
        }
        if candidate.auto_allowed {
            notes.push("allowlisted");
        }
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!("  [{}]", notes.join(", "))
        };
        text.push_str(&format!(
            "  {:>5}  {:<16}  {:<24}  {}{}\n",
            listener.port,
            ui::sanitize(&listener.process),
            ui::sanitize(&workspace),
            ui::sanitize(&agent),
            notes
        ));
    }
    if report.candidates.is_empty() {
        text.push_str("  nothing to offer; listeners with no Herdr pane behind them are hidden\n");
    }
    text
}

fn render_reconcile(report: &ReconcileReport) -> String {
    let trigger = match report.trigger {
        ReconcileTrigger::Startup => "startup",
        ReconcileTrigger::Event => "event",
        ReconcileTrigger::Manual => "manual",
    };
    let mut text = format!(
        "reconcile ({trigger}): {} kept, {} re-added, {} dropped\n",
        report.kept.len(),
        report.readded.len(),
        report.dropped.len()
    );
    for readded in &report.readded {
        text.push_str(&format!(
            "  re-added {}\n",
            mapping(
                &readded.spec.host,
                readded.spec.remote_port,
                readded.spec.local_port
            )
        ));
    }
    for dropped in &report.dropped {
        text.push_str(&format!(
            "  dropped {}: {}\n",
            mapping(
                &dropped.forward.spec.host,
                dropped.forward.spec.remote_port,
                dropped.forward.spec.local_port
            ),
            drop_reason_label(&dropped.reason)
        ));
    }
    text
}

fn render_status(report: &StatusReport) -> String {
    let mut text = String::new();
    text.push_str(&format!(
        "hosts file:   {}\n",
        show_path(&report.hosts_file)
    ));
    for shadowed in &report.shadowed {
        text.push_str(&format!("  shadowed:   {}\n", shadowed.display()));
    }
    // The path first, then what happened to it. A user who ran `status`
    // because something was wrong needs the name of the file to look at more
    // than they need the records it was supposed to hold.
    text.push_str(&format!("state file:   {}\n", report.state_file.display()));
    if let Some(why) = &report.state_error {
        text.push_str(&format!("  unreadable: {}\n", ui::sanitize(why)));
    }
    text.push_str(&format!(
        "request file: {}\n",
        report.request_file.display()
    ));
    if let Some(why) = &report.request_error {
        text.push_str(&format!("  unreadable: {}\n", ui::sanitize(why)));
    }
    match &report.selected {
        Some(selected) => text.push_str(&format!(
            "selected:     {} (via {}), {}\n",
            selected.name,
            selected.source,
            placement_label(&selected.placement)
        )),
        None => text.push_str(&format!(
            "selected:     none — {}\n",
            ui::sanitize(
                report
                    .selection_error
                    .as_deref()
                    .unwrap_or("no host could be resolved")
            )
        )),
    }
    text.push_str(&format!(
        "pending:      {}\n",
        match report.pending_requests {
            Some(count) => count.to_string(),
            None => "unknown".to_string(),
        }
    ));
    for warning in &report.warnings {
        // Config text and error strings both end up here, and `status` is what
        // someone runs when something is already wrong.
        text.push_str(&format!("warning:      {}\n", ui::sanitize(warning)));
    }
    for host in &report.hosts {
        let master = match host.master_running {
            Some(true) => "master running",
            Some(false) => "no master",
            None => "master not checked",
        };
        // "0 forward(s)" is a count; a file that could not be read has none to
        // give, and printing one would be the report agreeing with itself
        // about something it never learned.
        let counted = match &host.forwards {
            Some(forwards) => format!("{} forward(s)", forwards.len()),
            None => "forwards unknown".to_string(),
        };
        text.push_str(&format!(
            "\n{} -> {} ({master}, {counted})\n",
            ui::sanitize(&host.host.name),
            ui::sanitize(&host.host.target),
        ));
        for forward in host.forwards.iter().flatten() {
            text.push_str(&format!(
                "  {}  {}\n",
                mapping(
                    &forward.spec.host,
                    forward.spec.remote_port,
                    forward.spec.local_port
                ),
                origin_label(forward.spec.origin)
            ));
        }
    }
    if report.hosts.is_empty() {
        text.push_str("\nno hosts configured; add [hosts.<name>] to hosts.toml\n");
    }
    text
}

fn show_path(path: &Option<PathBuf>) -> String {
    match path {
        Some(path) => path.display().to_string(),
        None => "none found".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use herdr_portfwd::actions::{
        Candidate, Dropped, ForwardStatus, HostForwards, HostStatus, SelectedHost,
    };
    use herdr_portfwd::request::Request;
    use herdr_portfwd::types::{Forward, ForwardSpec, Listener};

    fn spec(host: &str, remote: u16, local: u16) -> ForwardSpec {
        ForwardSpec {
            host: host.to_string(),
            remote_port: remote,
            local_port: local,
            remote_bind: "127.0.0.1".to_string(),
            origin: Origin::Explicit,
            pane_id: Some("pane-1".to_string()),
        }
    }

    fn forward(host: &str, remote: u16, local: u16) -> Forward {
        Forward {
            spec: spec(host, remote, local),
            created_unix_ms: 1,
        }
    }

    fn local_placement() -> Where {
        Where {
            placement: Placement::Local,
            reason: PlacementReason::LiveMaster,
        }
    }

    fn remote_placement() -> Where {
        Where {
            placement: Placement::Remote,
            reason: PlacementReason::Assumed,
        }
    }

    // -- host names ---------------------------------------------------------

    #[test]
    fn a_configured_host_keeps_its_target_and_prefix() {
        let hosts = herdr_portfwd::config::parse_hosts(
            r#"
            [hosts.buildbox]
            target = "user@10.0.0.9"
            prefix = "build"
            "#,
        )
        .expect("fixture parses");
        let host = named_host(&hosts, "buildbox").expect("configured host resolves");
        assert_eq!(host.target, "user@10.0.0.9");
        assert_eq!(host.prefix.as_deref(), Some("build"));
    }

    #[test]
    fn an_unconfigured_name_is_used_as_an_ssh_target() {
        let hosts = HostsConfig::default();
        let host = named_host(&hosts, "buildbox").expect("a bare ssh alias is allowed");
        assert_eq!(host.name, "buildbox");
        assert_eq!(host.target, "buildbox");
        assert_eq!(host.prefix, None, "nothing claims it labels a workspace");
    }

    #[test]
    fn a_host_name_that_would_escape_the_socket_directory_is_refused() {
        let hosts = HostsConfig::default();
        for name in ["../evil", "a/b", ".", "..", "", "   ", "nul\0byte"] {
            assert!(
                named_host(&hosts, name).is_err(),
                "{name:?} must not become a control socket path"
            );
        }
    }

    #[test]
    fn an_ordinary_dotted_or_at_signed_name_is_still_fine() {
        let hosts = HostsConfig::default();
        for name in ["build.example.com", "user@host", ".hidden", "host-1_2"] {
            assert!(named_host(&hosts, name).is_ok(), "{name:?} should resolve");
        }
    }

    // -- exit codes ---------------------------------------------------------

    #[test]
    fn a_clean_child_exits_clean() {
        assert_eq!(
            format!("{:?}", exit_code_for(0, None)),
            format!("{:?}", ExitCode::SUCCESS)
        );
    }

    #[test]
    fn exit_codes_at_and_past_the_byte_boundary_do_not_wrap() {
        // 255 fits; 256 and a negative placeholder do not, and must degrade to a
        // plain failure rather than becoming 0 or some unrelated number.
        let failure = format!("{:?}", ExitCode::from(EXIT_FAILURE));
        assert_eq!(
            format!("{:?}", exit_code_for(255, None)),
            format!("{:?}", ExitCode::from(255u8))
        );
        assert_eq!(format!("{:?}", exit_code_for(256, None)), failure);
        assert_eq!(format!("{:?}", exit_code_for(-1, None)), failure);
    }

    #[test]
    fn a_signalled_child_reports_the_shell_convention() {
        assert_eq!(
            format!("{:?}", exit_code_for(0, Some(15))),
            format!("{:?}", ExitCode::from(143u8))
        );
        // A signal number nobody can name would push 128 + n past a byte.
        assert_eq!(
            format!("{:?}", exit_code_for(0, Some(200))),
            format!("{:?}", ExitCode::from(EXIT_FAILURE))
        );
    }

    #[test]
    fn a_signal_outranks_the_exit_status() {
        assert_eq!(
            format!("{:?}", exit_code_for(0, Some(2))),
            format!("{:?}", ExitCode::from(130u8))
        );
    }

    // -- where a click goes -------------------------------------------------

    fn plugin(action: Option<&str>, entrypoint: Option<&str>) -> PluginInvocation {
        PluginInvocation {
            action_id: action.map(str::to_string),
            entrypoint_id: entrypoint.map(str::to_string),
            plugin_id: Some("portfwd".to_string()),
            herdr_bin: Some("/opt/herdr/bin/herdr".to_string()),
        }
    }

    #[test]
    fn a_click_lands_on_the_confirmation_instead_of_forwarding_outright() {
        // The behaviour change in one line: the link handler runs as an action,
        // and an action now asks Herdr for the screen rather than opening a
        // port and a browser with nothing in between.
        let action = plugin(Some("portfwd.open-url"), None);
        assert_eq!(
            open_url_route(false, false, true, &action),
            Route::AskHerdr("portfwd")
        );
    }

    #[test]
    fn turning_the_confirmation_off_restores_the_immediate_forward_and_open() {
        let action = plugin(Some("portfwd.open-url"), None);
        assert_eq!(open_url_route(false, false, false, &action), Route::Run);
        assert_eq!(open_url_route(false, true, false, &action), Route::Run);
    }

    #[test]
    fn a_parsing_caller_and_a_bare_shell_still_get_the_click_acted_on() {
        // `--json` names a caller that wants the outcome, not a screen; a shell
        // with no Herdr has nowhere to put one and, unlike `add`, was told
        // everything it needs. Same rule `scan` follows.
        let action = plugin(Some("portfwd.open-url"), None);
        assert_eq!(open_url_route(true, false, true, &action), Route::Run);
        assert_eq!(open_url_route(true, true, true, &action), Route::Run);

        let shell = PluginInvocation::default();
        assert_eq!(open_url_route(false, false, true, &shell), Route::Refuse);
        // ...and `Refuse` is the branch `open_url` treats as "just do it".
    }

    #[test]
    fn a_terminal_draws_the_confirmation_itself() {
        let shell = PluginInvocation::default();
        assert_eq!(open_url_route(false, true, true, &shell), Route::Pane);
    }

    #[test]
    fn the_confirm_pane_can_never_ask_for_another_confirm_pane() {
        // The recursion guard, at the one command that now opens a pane where
        // it used to act: a pane command carries HERDR_PLUGIN_ENTRYPOINT_ID,
        // and `pane_to_open` refuses on that alone — even when an action id
        // somehow rode along too.
        for invocation in [
            plugin(None, Some("confirm")),
            plugin(Some("portfwd.open-url"), Some("confirm")),
        ] {
            assert_eq!(invocation.pane_to_open(), None);
            assert_ne!(
                open_url_route(false, false, true, &invocation),
                Route::AskHerdr("portfwd")
            );
            assert_eq!(pane_route(false, &invocation), Route::Pane);
        }
    }

    // -- one pane reaching another ------------------------------------------

    #[test]
    fn a_popup_handing_off_to_a_popup_asks_for_an_overlay_instead() {
        // Herdr keeps one popup slot and the pane handing off is holding it
        // until its own process exits, so asking for a popup gets
        // `popup already open` back. Every other pair keeps the manifest's
        // shape.
        assert_eq!(
            handoff_placement(Pane::Confirm, Pane::Picker),
            Some("overlay")
        );
        assert_eq!(handoff_placement(Pane::Confirm, Pane::Manager), None);
        assert_eq!(handoff_placement(Pane::Manager, Pane::Picker), None);
        assert_eq!(handoff_placement(Pane::Picker, Pane::Manager), None);
    }

    #[test]
    fn the_panes_this_binary_calls_popups_are_the_ones_it_names_that_way() {
        // The other half lives in tests/cli.rs, which reads the placements out
        // of herdr-plugin.toml; between them the manifest and this function
        // cannot drift.
        assert!(!is_popup(Pane::Manager));
        assert!(is_popup(Pane::Picker));
        assert!(is_popup(Pane::Confirm));
    }

    #[test]
    fn every_pane_maps_to_the_entrypoint_id_herdr_launches_it_by() {
        // Herdr hands the entrypoint id back as the `pane-ui` subcommand, so
        // these two have to be inverses or a request for one pane returns
        // another.
        for entrypoint in [
            PaneEntrypoint::List,
            PaneEntrypoint::Picker,
            PaneEntrypoint::Confirm,
        ] {
            let pane = pane_for(entrypoint);
            let id = entrypoint_id(pane);
            let parsed = Cli::try_parse_from(["herdr-portfwd", "pane-ui", id])
                .unwrap_or_else(|err| panic!("`pane-ui {id}` does not parse: {err}"));
            match parsed.command {
                Command::PaneUi { entrypoint: back } => assert_eq!(back, entrypoint),
                other => panic!("expected pane-ui, got {other:?}"),
            }
        }
        assert_eq!(entrypoint_id(handoff_pane(Handoff::List)), "list");
        assert_eq!(entrypoint_id(handoff_pane(Handoff::Picker)), "picker");
    }

    #[test]
    fn the_confirm_pane_names_the_click_and_the_host_it_resolved() {
        let state = ConfirmState {
            url: "http://localhost:5173/app".to_string(),
            host: Some("buildbox".to_string()),
            applies_here: true,
            ..ConfirmState::default()
        };
        let env = confirm_env(&state).expect("assignments");
        assert_eq!(env.len(), 3);
        assert!(env[0].starts_with("HERDR_PORTFWD_CONFIRM={"), "{env:?}");
        assert!(env[0].contains("http://localhost:5173/app"), "{env:?}");
        // The pane resolves the host this process already chose rather than
        // re-deriving it from a context that has moved on.
        assert_eq!(env[1], "HERDR_PORTFWD_HOST=buildbox");
        // And with it the placement that host was chosen *under*. Naming the
        // host resolves it as `Explicit`, which is exactly the source
        // `locate_resolved` reads as "no mirror prefix matched" — so without
        // this the one variable that pins the host would quietly un-pin the
        // side of the connection the click had already decided on.
        assert_eq!(env[2], "HERDR_PORTFWD_MODE=local");
    }

    #[test]
    fn a_click_the_far_side_decided_hands_the_pane_that_same_side() {
        // The mirror of the case above: a pane that re-derived placement from
        // its own popup context could apply here a forward the click had
        // already resolved as the local agent's job, or the reverse.
        let state = ConfirmState {
            url: "http://localhost:5173/app".to_string(),
            host: Some("buildbox".to_string()),
            applies_here: false,
            ..ConfirmState::default()
        };
        let env = confirm_env(&state).expect("assignments");
        assert_eq!(
            env.iter()
                .find(|word| word.starts_with("HERDR_PORTFWD_MODE=")),
            Some(&"HERDR_PORTFWD_MODE=remote".to_string()),
            "{env:?}"
        );
    }

    #[test]
    fn a_click_with_no_host_sends_none_rather_than_a_guess() {
        let state = ConfirmState {
            url: "http://localhost:5173/".to_string(),
            problem: Some("cannot tell which host to use".to_string()),
            ..ConfirmState::default()
        };
        let env = confirm_env(&state).expect("assignments");
        assert_eq!(env.len(), 1);
        assert!(
            !env.iter()
                .any(|word| word.starts_with("HERDR_PORTFWD_HOST=")),
            "{env:?}"
        );
    }

    #[test]
    fn a_confirmed_forward_says_which_machine_did_what() {
        let opened = open_summary(
            &OpenOutcome {
                url: "http://localhost:5173/".to_string(),
                add: AddOutcome {
                    spec: spec("buildbox", 5173, 5173),
                    applied: Applied::Forwarded,
                    request_id: None,
                    placement: local_placement(),
                },
                opened: true,
            },
            true,
        );
        assert!(opened.contains("forwarded buildbox:5173"), "{opened}");
        assert!(opened.contains("opened http://localhost:5173/"), "{opened}");

        let quiet = open_summary(
            &OpenOutcome {
                url: "http://localhost:5173/".to_string(),
                add: AddOutcome {
                    spec: spec("buildbox", 5173, 5173),
                    applied: Applied::Forwarded,
                    request_id: None,
                    placement: local_placement(),
                },
                opened: false,
            },
            false,
        );
        assert!(quiet.contains("forwarded buildbox:5173"), "{quiet}");
        assert!(!quiet.contains("opened"), "`f` opens nothing: {quiet}");
    }

    #[test]
    fn a_confirmed_forward_on_the_far_side_credits_the_agent_with_the_browser() {
        // Under `herdr --remote` the pane opens nothing: it records a request,
        // and the local agent forwards it and opens the page. Saying "opened"
        // here would be a claim about a machine this process cannot reach.
        let requested = |open_browser| {
            open_summary(
                &OpenOutcome {
                    url: "http://localhost:5173/".to_string(),
                    add: AddOutcome {
                        spec: spec("buildbox", 5173, 5173),
                        applied: Applied::Requested,
                        request_id: Some(3),
                        placement: remote_placement(),
                    },
                    opened: false,
                },
                open_browser,
            )
        };
        let with_page = requested(true);
        assert!(with_page.contains("requested"), "{with_page}");
        assert!(with_page.contains("local agent"), "{with_page}");
        assert!(with_page.contains("opens the page"), "{with_page}");

        let without = requested(false);
        assert!(without.contains("local agent applies it"), "{without}");
        assert!(!without.contains("opens the page"), "{without}");
    }

    // -- add / open-url -----------------------------------------------------

    #[test]
    fn a_forward_on_the_same_port_says_so_without_a_remap_note() {
        let text = render_add(&AddOutcome {
            spec: spec("buildbox", 5173, 5173),
            applied: Applied::Forwarded,
            request_id: None,
            placement: local_placement(),
        });
        assert!(
            text.contains("forwarded buildbox:5173 -> localhost:5173"),
            "{text}"
        );
        assert!(!text.contains("remapped"), "{text}");
    }

    #[test]
    fn a_remap_is_always_called_out() {
        let text = render_add(&AddOutcome {
            spec: spec("buildbox", 5173, 15173),
            applied: Applied::Forwarded,
            request_id: None,
            placement: local_placement(),
        });
        assert!(
            text.contains("remapped"),
            "a remap must never be silent: {text}"
        );
    }

    #[test]
    fn a_request_explains_that_nothing_is_live_yet() {
        let text = render_add(&AddOutcome {
            spec: spec("buildbox", 5173, 5173),
            applied: Applied::Requested,
            request_id: Some(7),
            placement: remote_placement(),
        });
        assert!(text.contains("requested"), "{text}");
        assert!(text.contains("request 7"), "{text}");
        assert!(text.contains("local agent"), "{text}");
    }

    #[test]
    fn a_request_with_no_id_still_renders() {
        let text = render_add(&AddOutcome {
            spec: spec("buildbox", 5173, 5173),
            applied: Applied::Requested,
            request_id: None,
            placement: remote_placement(),
        });
        assert!(text.starts_with("requested"), "{text}");
    }

    #[test]
    fn an_unopened_url_tells_the_user_where_to_point_a_browser() {
        let text = render_open(&OpenOutcome {
            url: "http://localhost:5173/app".to_string(),
            add: AddOutcome {
                spec: spec("buildbox", 5173, 5173),
                applied: Applied::Requested,
                request_id: Some(1),
                placement: remote_placement(),
            },
            opened: false,
        });
        assert!(text.contains("http://localhost:5173/app"), "{text}");
        assert!(text.contains("not opened here"), "{text}");
    }

    #[test]
    fn an_opened_url_is_reported_as_opened() {
        let text = render_open(&OpenOutcome {
            url: "http://127.0.0.1:8787/".to_string(),
            add: AddOutcome {
                spec: spec("buildbox", 8787, 8787),
                applied: Applied::Forwarded,
                request_id: None,
                placement: local_placement(),
            },
            opened: true,
        });
        assert!(text.contains("opened http://127.0.0.1:8787/"), "{text}");
    }

    // -- list ---------------------------------------------------------------

    fn section(host: &str, master: Option<bool>, forwards: Vec<ForwardStatus>) -> HostForwards {
        HostForwards {
            host: host.to_string(),
            master_running: master,
            forwards,
            pending: Vec::new(),
            placement: local_placement(),
        }
    }

    #[test]
    fn an_empty_list_still_names_the_host_it_was_narrowed_to() {
        let text = render_list(&ListReport {
            scope: Scope::Host("buildbox".to_string()),
            hosts: vec![section("buildbox", Some(false), Vec::new())],
        });
        assert!(text.contains("buildbox: 0 forward(s)"), "{text}");
        assert!(text.contains("no master"), "{text}");
        assert!(text.contains("--host"), "the narrowing has to be visible");
    }

    #[test]
    fn a_list_reports_each_forwards_liveness_and_origin() {
        let mut missing = forward("buildbox", 8787, 8787);
        missing.spec.origin = Origin::Offered;
        let text = render_list(&ListReport {
            scope: Scope::AllHosts,
            hosts: vec![section(
                "buildbox",
                Some(true),
                vec![
                    ForwardStatus {
                        forward: forward("buildbox", 5173, 5173),
                        live: Liveness::Live,
                    },
                    ForwardStatus {
                        forward: missing,
                        live: Liveness::Missing,
                    },
                ],
            )],
        });
        assert!(text.contains("live"), "{text}");
        assert!(text.contains("missing"), "{text}");
        assert!(text.contains("offered"), "{text}");
        assert!(text.contains("pane pane-1"), "{text}");
    }

    #[test]
    fn a_listing_says_how_many_hosts_it_covers_rather_than_reading_as_the_world() {
        // The report a user pasted: one host's header over a listing that was
        // silently one host of two. The first line has to say which it is.
        let text = render_list(&ListReport {
            scope: Scope::AllHosts,
            hosts: vec![
                section(
                    "mac-mini-honkoma",
                    Some(true),
                    vec![ForwardStatus {
                        forward: forward("mac-mini-honkoma", 8788, 8788),
                        live: Liveness::Live,
                    }],
                ),
                section(
                    "mac-studio-honkoma",
                    Some(true),
                    vec![ForwardStatus {
                        forward: forward("mac-studio-honkoma", 5178, 5178),
                        live: Liveness::Live,
                    }],
                ),
            ],
        });
        assert!(text.starts_with("2 forward(s) on 2 host(s)"), "{text}");
        assert!(text.contains("mac-mini-honkoma: 1 forward(s)"), "{text}");
        assert!(text.contains("mac-studio-honkoma: 1 forward(s)"), "{text}");
        assert!(text.contains("5178"), "{text}");
    }

    #[test]
    fn a_listing_with_nothing_anywhere_says_so_in_those_words() {
        let text = render_list(&ListReport {
            scope: Scope::AllHosts,
            hosts: Vec::new(),
        });
        assert_eq!(text, "no forwards on any host\n");
    }

    #[test]
    fn pending_requests_are_listed_when_the_forward_is_not_ours_to_apply() {
        let text = render_list(&ListReport {
            scope: Scope::AllHosts,
            hosts: vec![HostForwards {
                host: "buildbox".to_string(),
                master_running: None,
                forwards: Vec::new(),
                pending: vec![Request::new(1, 0, spec("buildbox", 5173, 5173))],
                placement: remote_placement(),
            }],
        });
        assert!(text.contains("1 request(s) waiting"), "{text}");
        assert!(text.contains("5173"), "{text}");
    }

    // -- stop ---------------------------------------------------------------

    #[test]
    fn stopping_nothing_is_reported_as_a_no_op() {
        // Invariant 4: this is success, and the wording must not read as one.
        let text = render_stop(&StopOutcome {
            hosts: vec!["buildbox".to_string()],
            stopped: Vec::new(),
            elsewhere: Vec::new(),
        });
        assert_eq!(text, "nothing to stop on buildbox\n");
    }

    #[test]
    fn a_stop_that_looked_everywhere_says_so_rather_than_naming_one_host() {
        let text = render_stop(&StopOutcome {
            hosts: Vec::new(),
            stopped: Vec::new(),
            elsewhere: Vec::new(),
        });
        assert_eq!(text, "nothing to stop: no forward on any host\n");
    }

    #[test]
    fn a_stop_narrowed_past_the_port_says_where_the_port_actually_is() {
        let text = render_stop(&StopOutcome {
            hosts: vec!["mac-mini-honkoma".to_string()],
            stopped: Vec::new(),
            elsewhere: vec!["mac-studio-honkoma".to_string()],
        });
        assert!(
            text.contains("nothing to stop on mac-mini-honkoma"),
            "{text}"
        );
        assert!(
            text.contains("forwarded on mac-studio-honkoma"),
            "the port is up one host over, and the report has to say so: {text}"
        );
        assert!(text.contains("--host"), "{text}");
    }

    #[test]
    fn stopping_several_names_every_port() {
        let text = render_stop(&StopOutcome {
            hosts: vec!["buildbox".to_string()],
            stopped: vec![
                forward("buildbox", 5173, 5173),
                forward("buildbox", 8787, 8787),
            ],
            elsewhere: Vec::new(),
        });
        assert!(text.contains("5173, 8787"), "{text}");
    }

    #[test]
    fn a_stop_across_two_hosts_says_which_ports_went_with_which() {
        let text = render_stop(&StopOutcome {
            hosts: vec!["mini".to_string(), "studio".to_string()],
            stopped: vec![forward("mini", 8788, 8788), forward("studio", 5178, 5178)],
            elsewhere: Vec::new(),
        });
        assert_eq!(text, "stopped 8788 on mini; 5178 on studio\n");
    }

    // -- scan ---------------------------------------------------------------

    fn candidate(port: u16, pane: Option<PaneRef>) -> Candidate {
        Candidate {
            listener: Listener {
                port,
                bind: "127.0.0.1".to_string(),
                pid: 42,
                process: "node".to_string(),
            },
            pane,
            already_forwarded: false,
            local_port_free: Some(true),
            auto_allowed: false,
        }
    }

    fn pane(label: &str) -> PaneRef {
        PaneRef {
            pane_id: "pane-1".to_string(),
            workspace_id: "ws-1".to_string(),
            workspace_label: Some(label.to_string()),
            agent: Some("claude".to_string()),
            shell_pid: Some(100),
        }
    }

    #[test]
    fn a_scan_with_nothing_to_offer_explains_the_filter() {
        let text = render_scan(&ScanReport {
            host: "buildbox".to_string(),
            candidates: Vec::new(),
            unattributed: 12,
            unforwardable: 3,
            placement: local_placement(),
        });
        assert!(text.contains("12 unattributed"), "{text}");
        assert!(text.contains("3 not forwardable"), "{text}");
        assert!(text.contains("nothing to offer"), "{text}");
    }

    #[test]
    fn a_scan_row_carries_the_workspace_and_agent() {
        let text = render_scan(&ScanReport {
            host: "buildbox".to_string(),
            candidates: vec![candidate(5173, Some(pane("mini: scholion")))],
            unattributed: 0,
            unforwardable: 0,
            placement: local_placement(),
        });
        assert!(text.contains("mini: scholion"), "{text}");
        assert!(text.contains("claude"), "{text}");
    }

    #[test]
    fn a_scan_row_with_no_pane_renders_placeholders() {
        let text = render_scan(&ScanReport {
            host: "buildbox".to_string(),
            candidates: vec![candidate(5173, None)],
            unattributed: 1,
            unforwardable: 0,
            placement: local_placement(),
        });
        assert!(text.contains("5173"), "{text}");
        assert!(!text.contains("nothing to offer"), "{text}");
    }

    #[test]
    fn remote_terminal_escapes_never_reach_the_log_verbatim() {
        // Process names and workspace labels come off the far side; a raw escape
        // sequence in one would drive the terminal reading the log.
        let mut hostile = candidate(5173, Some(pane("\u{1b}]0;pwned\u{7}")));
        hostile.listener.process = "no\u{1b}[2Jde".to_string();
        let text = render_scan(&ScanReport {
            host: "buildbox".to_string(),
            candidates: vec![hostile],
            unattributed: 0,
            unforwardable: 0,
            placement: local_placement(),
        });
        assert!(!text.contains('\u{1b}'), "escape survived: {text:?}");
        assert!(!text.contains('\u{7}'), "bell survived: {text:?}");
    }

    #[test]
    fn candidate_notes_are_all_reported_together() {
        let mut busy = candidate(5173, Some(pane("mini: scholion")));
        busy.already_forwarded = true;
        busy.local_port_free = Some(false);
        busy.auto_allowed = true;
        let text = render_scan(&ScanReport {
            host: "buildbox".to_string(),
            candidates: vec![busy],
            unattributed: 0,
            unforwardable: 0,
            placement: local_placement(),
        });
        assert!(text.contains("already forwarded"), "{text}");
        assert!(text.contains("local port in use"), "{text}");
        assert!(text.contains("allowlisted"), "{text}");
    }

    #[test]
    fn a_scan_from_the_far_side_does_not_claim_the_local_port_is_taken() {
        // Under `herdr --remote` nothing here can know what the user's machine
        // has bound, and every candidate is a listener on this one.
        let mut unknown = candidate(5173, Some(pane("mini: scholion")));
        unknown.local_port_free = None;
        let text = render_scan(&ScanReport {
            host: "buildbox".to_string(),
            candidates: vec![unknown],
            unattributed: 0,
            unforwardable: 0,
            placement: remote_placement(),
        });
        assert!(
            !text.contains("local port in use"),
            "an unknown port must not read as a taken one: {text}"
        );
    }

    // -- reconcile ----------------------------------------------------------

    #[test]
    fn a_reconcile_that_changed_nothing_still_says_which_trigger_ran() {
        for (startup, event, expected) in [
            (true, false, "startup"),
            (false, true, "event"),
            (false, false, "manual"),
            // Both flags at once: startup wins, because it is the trigger
            // allowed to dial, and picking the other would skip restoring.
            (true, true, "startup"),
        ] {
            let trigger = match (startup, event) {
                (true, _) => ReconcileTrigger::Startup,
                (false, true) => ReconcileTrigger::Event,
                (false, false) => ReconcileTrigger::Manual,
            };
            let text = render_reconcile(&ReconcileReport {
                trigger,
                kept: Vec::new(),
                readded: Vec::new(),
                dropped: Vec::new(),
            });
            assert!(text.contains(expected), "{text}");
        }
    }

    #[test]
    fn every_drop_reason_renders_a_sentence() {
        let reasons = [
            DropReason::HostGone,
            DropReason::PaneGone,
            DropReason::MasterGone,
            DropReason::Unreachable("control path too long".to_string()),
            DropReason::ReAddFailed("connection closed".to_string()),
        ];
        for reason in reasons {
            let text = render_reconcile(&ReconcileReport {
                trigger: ReconcileTrigger::Startup,
                kept: Vec::new(),
                readded: Vec::new(),
                dropped: vec![Dropped {
                    forward: forward("buildbox", 5173, 5173),
                    reason: reason.clone(),
                }],
            });
            assert!(text.contains("dropped buildbox:5173"), "{text}");
            assert!(
                text.trim_end().lines().count() == 2,
                "one line per dropped forward: {text}"
            );
        }
    }

    #[test]
    fn a_readded_forward_is_distinguishable_from_a_dropped_one() {
        let text = render_reconcile(&ReconcileReport {
            trigger: ReconcileTrigger::Startup,
            kept: vec![forward("buildbox", 3000, 3000)],
            readded: vec![forward("buildbox", 5173, 5173)],
            dropped: vec![Dropped {
                forward: forward("buildbox", 8787, 8787),
                reason: DropReason::PaneGone,
            }],
        });
        assert!(text.contains("1 kept, 1 re-added, 1 dropped"), "{text}");
        assert!(text.contains("re-added buildbox:5173"), "{text}");
        assert!(text.contains("dropped buildbox:8787"), "{text}");
    }

    // -- status -------------------------------------------------------------

    fn empty_status() -> StatusReport {
        StatusReport {
            hosts_file: None,
            shadowed: Vec::new(),
            warnings: Vec::new(),
            state_file: PathBuf::from("/state/forwards.json"),
            request_file: PathBuf::from("/state/requests.json"),
            selected: None,
            selection_error: Some("no hosts are configured".to_string()),
            hosts: Vec::new(),
            pending_requests: Some(0),
            state_error: None,
            request_error: None,
        }
    }

    #[test]
    fn status_with_nothing_configured_says_what_to_do() {
        let text = render_status(&empty_status());
        assert!(text.contains("none found"), "{text}");
        assert!(text.contains("no hosts are configured"), "{text}");
        assert!(text.contains("hosts.toml"), "{text}");
    }

    #[test]
    fn status_reports_shadowed_files_and_warnings_rather_than_hiding_them() {
        let mut report = empty_status();
        report.hosts_file = Some(PathBuf::from("/c/herdr-portfwd/hosts.toml"));
        report.shadowed = vec![PathBuf::from("/c/herdr-mirror/hosts.toml")];
        report.warnings = vec!["skipping host [hosts.x]: target is empty".to_string()];
        let text = render_status(&report);
        assert!(text.contains("/c/herdr-mirror/hosts.toml"), "{text}");
        assert!(text.contains("target is empty"), "{text}");
    }

    #[test]
    fn status_lists_each_host_with_its_master_and_forwards() {
        let mut report = empty_status();
        report.selected = Some(SelectedHost {
            name: "buildbox".to_string(),
            source: "explicit",
            placement: local_placement(),
        });
        report.selection_error = None;
        report.hosts = vec![HostStatus {
            host: Host {
                name: "buildbox".to_string(),
                target: "user@10.0.0.9".to_string(),
                prefix: Some("build".to_string()),
            },
            master_running: Some(true),
            forwards: Some(vec![forward("buildbox", 5173, 5173)]),
        }];
        report.pending_requests = Some(2);
        let text = render_status(&report);
        assert!(text.contains("buildbox -> user@10.0.0.9"), "{text}");
        assert!(text.contains("master running"), "{text}");
        assert!(text.contains("pending:      2"), "{text}");
        assert!(text.contains("local (live control master)"), "{text}");
    }

    #[test]
    fn status_names_a_record_file_it_could_not_read_and_still_prints_the_rest() {
        // `status` is what the READMEs tell a user to run first when something
        // is already wrong, and the record file is one of the things that can
        // be wrong. Returning `Err` for it printed nothing at all: not the path
        // of the broken file, not the hosts, not the requests that were
        // perfectly readable.
        let mut report = empty_status();
        report.state_error =
            Some("forwards.json could not be read, so this process knows nothing of what it holds: /state/forwards.json".to_string());
        report.hosts = vec![HostStatus {
            host: Host {
                name: "buildbox".to_string(),
                target: "user@10.0.0.9".to_string(),
                prefix: None,
            },
            master_running: Some(true),
            forwards: None,
        }];
        report.pending_requests = Some(3);
        let text = render_status(&report);
        assert!(text.contains("/state/forwards.json"), "{text}");
        assert!(text.contains("could not be read"), "{text}");
        // Everything it *did* learn is still on the screen.
        assert!(text.contains("buildbox -> user@10.0.0.9"), "{text}");
        assert!(text.contains("master running"), "{text}");
        assert!(text.contains("pending:      3"), "{text}");
        // And nothing it did not: "0 forward(s)" here is the count the empty
        // read used to invent.
        assert!(
            text.contains("forwards unknown") && !text.contains("0 forward(s)"),
            "a file that was never read was counted anyway:\n{text}"
        );
    }

    #[test]
    fn status_that_could_not_read_the_queue_says_so_rather_than_counting_zero() {
        let mut report = empty_status();
        report.request_error = Some("requests.json could not be read".to_string());
        report.pending_requests = None;
        let text = render_status(&report);
        assert!(text.contains("pending:      unknown"), "{text}");
        assert!(!text.contains("pending:      0"), "{text}");
        assert!(text.contains("requests.json could not be read"), "{text}");
    }

    #[test]
    fn a_status_that_read_everything_reports_no_failure() {
        // The gate the exit code is taken from: an ordinary report must not
        // start exiting non-zero because the fields exist.
        assert!(empty_status().failures().is_empty());
        let mut broken = empty_status();
        broken.state_error = Some("forwards.json could not be read".to_string());
        broken.request_error = Some("requests.json could not be read".to_string());
        assert_eq!(broken.failures().len(), 2);
    }

    #[test]
    fn an_unchecked_master_is_not_reported_as_absent() {
        let mut report = empty_status();
        report.hosts = vec![HostStatus {
            host: Host {
                name: "buildbox".to_string(),
                target: "buildbox".to_string(),
                prefix: None,
            },
            master_running: None,
            forwards: Some(Vec::new()),
        }];
        let text = render_status(&report);
        assert!(text.contains("master not checked"), "{text}");
        assert!(!text.contains("no master"), "{text}");
    }

    // -- interactive routing --------------------------------------------------

    #[test]
    fn a_terminal_gets_a_pane_and_anything_else_gets_a_report() {
        assert!(interactive(false, true, true), "a person at a terminal");
        assert!(
            !interactive(false, false, false),
            "a headless plugin action"
        );
        assert!(
            !interactive(true, true, true),
            "--json is never interactive, terminal or not"
        );
        assert!(!interactive(true, false, false));
    }

    #[test]
    fn a_pane_needs_a_terminal_at_both_ends_of_it() {
        // `herdr-portfwd add > add.txt` from an interactive shell: a keyboard
        // on stdin, and nowhere to draw. Deciding from stdin alone put the tty
        // in raw mode and painted a blind picker into `add.txt`, so the user
        // got neither the report nor the refusal.
        assert!(
            !interactive(false, true, false),
            "stdout is redirected: the frames would go into the redirect target"
        );
        // And the mirror image: a script feeding keys in with the screen still
        // attached has no way to answer what the pane draws.
        assert!(!interactive(false, false, true), "stdin is not a terminal");
    }

    /// The environment Herdr injects for a plugin action.
    fn action_invocation() -> PluginInvocation {
        PluginInvocation {
            action_id: Some("portfwd.add".to_string()),
            entrypoint_id: None,
            plugin_id: Some("portfwd".to_string()),
            herdr_bin: Some("/opt/herdr/bin/herdr".to_string()),
        }
    }

    /// The environment a pane command gets: the same, plus the entrypoint id.
    fn pane_invocation() -> PluginInvocation {
        PluginInvocation {
            entrypoint_id: Some("picker".to_string()),
            ..action_invocation()
        }
    }

    #[test]
    fn a_command_told_which_port_to_act_on_never_opens_a_pane() {
        // `scan` always has something left to pick, so it passes `true`; `add
        // 5173` and `stop 5173` do not.
        for interactive in [true, false] {
            for plugin in [
                PluginInvocation::default(),
                action_invocation(),
                pane_invocation(),
            ] {
                assert_eq!(
                    route(interactive, false, &plugin),
                    Route::Run,
                    "the user already named a port"
                );
            }
        }
    }

    #[test]
    fn a_command_that_was_not_told_enough_goes_where_it_can_ask() {
        let shell = PluginInvocation::default();
        let action = action_invocation();
        let pane = pane_invocation();
        // An action Herdr injected no bin path into: there is nobody to ask.
        let no_herdr = PluginInvocation {
            herdr_bin: None,
            ..action_invocation()
        };
        let cases: &[(&str, bool, &PluginInvocation, Route<'_>)] = &[
            (
                "a person at a terminal draws it here",
                true,
                &shell,
                Route::Pane,
            ),
            (
                // Herdr pipes an action's stdout and stderr but leaves stdin
                // inherited, so a foreground `herdr server` hands its own tty
                // to every action it spawns. Drawing there would read keys off
                // the terminal Herdr is reading and never reach EOF.
                "an action that inherited a terminal still asks Herdr, not the tty",
                true,
                &action,
                Route::AskHerdr("portfwd"),
            ),
            (
                "a headless action asks Herdr for the pane instead of refusing",
                false,
                &action,
                Route::AskHerdr("portfwd"),
            ),
            (
                "a pane command must not ask for another pane: that recurses",
                false,
                &pane,
                Route::Refuse,
            ),
            (
                "a shell invocation gets a sentence it can act on",
                false,
                &shell,
                Route::Refuse,
            ),
            (
                "an action with no herdr to ask still refuses",
                false,
                &no_herdr,
                Route::Refuse,
            ),
        ];
        for (what, interactive, plugin, expected) in cases {
            assert_eq!(route(*interactive, true, plugin), *expected, "{what}");
        }
    }

    #[test]
    fn a_pane_command_never_asks_herdr_to_open_a_pane_whatever_it_is_running() {
        // The recursion guard, on its own: a pane whose command asked for a
        // pane would launch a command that asked again.
        let pane = pane_invocation();
        for nothing_was_named in [true, false] {
            assert!(
                !matches!(route(false, nothing_was_named, &pane), Route::AskHerdr(_)),
                "a pane command asked Herdr for another pane"
            );
        }
    }

    /// The environment Herdr injects for the `list` action, whose command is
    /// the `list` pane's own argv.
    fn manage_forwards_action() -> PluginInvocation {
        PluginInvocation {
            action_id: Some("portfwd.list".to_string()),
            ..action_invocation()
        }
    }

    #[test]
    fn the_manage_forwards_action_asks_herdr_for_the_pane_instead_of_drawing() {
        // `[[actions]] id = "list"` runs `pane-ui list`, and Herdr runs an
        // action headless with both streams captured. Drawing there paints one
        // frame of the overlay into the plugin log, hits EOF on stdin, and
        // exits 0: the palette entry whose whole purpose is that pane showed
        // the user nothing.
        let action = manage_forwards_action();
        for interactive in [true, false] {
            assert_eq!(
                pane_route(interactive, &action),
                Route::AskHerdr("portfwd"),
                "an action must ask Herdr for the pane, whatever its streams are"
            );
        }
    }

    #[test]
    fn the_pane_herdr_launched_draws_and_never_asks_for_another() {
        // The pane command carries HERDR_PLUGIN_ENTRYPOINT_ID. If it asked, the
        // pane Herdr opened would open a pane, whose command would ask again.
        let pane = pane_invocation();
        for interactive in [true, false] {
            assert_eq!(pane_route(interactive, &pane), Route::Pane);
        }
    }

    #[test]
    fn the_scan_action_asks_herdr_for_the_picker_instead_of_printing_into_the_log() {
        // The flagship keybinding. Herdr invokes it as an action, with stdout
        // and stderr piped into `herdr plugin log list`, so the report it used
        // to print reached nobody: the user pressed the key and saw nothing at
        // all. `add` and `stop` already ask; there is no reason `scan` differs,
        // because an action's stdout is a log and not a person.
        let action = PluginInvocation {
            action_id: Some("portfwd.scan".to_string()),
            ..action_invocation()
        };
        for interactive in [true, false] {
            assert_eq!(
                scan_route(false, interactive, &action),
                Route::AskHerdr("portfwd"),
                "a scan action must open the picker, whatever its streams are"
            );
        }
    }

    #[test]
    fn scan_still_prints_its_report_for_a_caller_that_can_read_one() {
        // The two cases the picker would be wrong for: a caller parsing JSON,
        // and a shell invocation with no Herdr on the other end. Both get the
        // complete report `scan` can always produce.
        let shell = PluginInvocation::default();
        assert_eq!(scan_route(false, false, &shell), Route::Refuse);
        assert_eq!(
            scan_route(true, false, &action_invocation()),
            Route::Run,
            "--json asked for the report, not for a pane in front of nobody"
        );
        assert_eq!(scan_route(true, false, &shell), Route::Run);
        assert_eq!(
            scan_route(true, false, &pane_invocation()),
            Route::Run,
            "--json outranks every invocation kind: a pane cannot print JSON"
        );
    }

    #[test]
    fn a_person_at_a_terminal_still_gets_the_scan_picker_drawn_here() {
        let shell = PluginInvocation::default();
        assert_eq!(scan_route(false, true, &shell), Route::Pane);
        // And the recursion guard still holds: the picker pane's own `scan`
        // must not ask for another picker.
        assert!(!matches!(
            scan_route(false, false, &pane_invocation()),
            Route::AskHerdr(_)
        ));
    }

    #[test]
    fn a_pane_with_nowhere_to_draw_and_nobody_to_ask_refuses() {
        // A hook, or a shell pipeline. Drawing would write a screenful of
        // escape sequences into whatever captured stdout and exit 0.
        let shell = PluginInvocation::default();
        assert_eq!(pane_route(false, &shell), Route::Refuse);
        assert_eq!(pane_route(true, &shell), Route::Pane, "a person at a tty");
    }

    #[test]
    fn each_pane_entrypoint_opens_the_pane_the_manifest_describes() {
        // `[[panes]] id = "list"` is the overlay of live forwards, and
        // `id = "picker"` is the popup over listeners. Swapped, both still run.
        assert_eq!(pane_for(PaneEntrypoint::List), Pane::Manager);
        assert_eq!(pane_for(PaneEntrypoint::Picker), Pane::Picker);
    }

    #[test]
    fn the_entrypoint_this_binary_asks_for_is_the_one_it_would_be_launched_with() {
        // `entrypoint_id` names the pane to Herdr and `pane_for` reads Herdr's
        // launch back. Disagreeing means asking for one pane and drawing the
        // other.
        for entrypoint in [PaneEntrypoint::List, PaneEntrypoint::Picker] {
            let pane = pane_for(entrypoint);
            let id = entrypoint_id(pane);
            let parsed = Cli::try_parse_from(["herdr-portfwd", "pane-ui", id])
                .unwrap_or_else(|err| panic!("herdr launches `pane-ui {id}`: {err}"));
            match parsed.command {
                Command::PaneUi { entrypoint } => assert_eq!(pane_for(entrypoint), pane),
                other => panic!("expected pane-ui, got {other:?}"),
            }
        }
        assert_eq!(entrypoint_id(Pane::Manager), "list");
        assert_eq!(entrypoint_id(Pane::Picker), "picker");
    }

    // -- hostile text on every path out ---------------------------------------

    /// What a remote can put in front of a local terminal: an OSC title change
    /// terminated by a bell, a screen clear, and a bidi override that reorders
    /// a rendered line without changing its width.
    const HOSTILE: &str = "\u{1b}]0;pwned\u{7}\u{1b}[2J\u{202e}3715";

    fn assert_nothing_a_terminal_would_obey(text: &str, what: &str) {
        for (ch, name) in [
            ('\u{1b}', "an escape"),
            ('\u{7}', "a bell"),
            ('\u{202e}', "a bidi override"),
        ] {
            assert!(!text.contains(ch), "{name} survived {what}: {text:?}");
        }
    }

    #[test]
    fn an_ssh_failure_cannot_drive_the_terminal_reading_a_reconcile_report() {
        // `DropReason`'s text is `ssh`'s stderr, which is the remote sshd's own
        // output: the most attacker-influenced string in the program.
        for reason in [
            DropReason::Unreachable(HOSTILE.to_string()),
            DropReason::ReAddFailed(HOSTILE.to_string()),
        ] {
            let text = render_reconcile(&ReconcileReport {
                trigger: ReconcileTrigger::Event,
                kept: Vec::new(),
                readded: Vec::new(),
                dropped: vec![Dropped {
                    forward: forward("buildbox", 5173, 5173),
                    reason,
                }],
            });
            assert_nothing_a_terminal_would_obey(&text, "a drop reason");
            assert!(text.contains("pwned"), "the reason itself is still there");
        }
    }

    #[test]
    fn a_pane_id_from_a_remote_request_is_sanitized_in_the_list() {
        // Under `--remote` the pane id arrives in a request the remote wrote.
        let mut hostile = forward("buildbox", 5173, 5173);
        hostile.spec.pane_id = Some(HOSTILE.to_string());
        let text = render_list(&ListReport {
            scope: Scope::AllHosts,
            hosts: vec![HostForwards {
                host: "buildbox".to_string(),
                master_running: Some(true),
                forwards: vec![ForwardStatus {
                    forward: hostile,
                    live: Liveness::Live,
                }],
                pending: Vec::new(),
                placement: remote_placement(),
            }],
        });
        assert_nothing_a_terminal_would_obey(&text, "a pane id");
    }

    #[test]
    fn a_hostile_warning_or_host_name_is_sanitized_in_status() {
        let mut report = empty_status();
        report.warnings = vec![format!("skipping host: {HOSTILE}")];
        report.selection_error = Some(format!("unknown host {HOSTILE}"));
        report.hosts = vec![HostStatus {
            host: Host {
                name: HOSTILE.to_string(),
                target: HOSTILE.to_string(),
                prefix: None,
            },
            master_running: None,
            forwards: Some(Vec::new()),
        }];
        let text = render_status(&report);
        assert_nothing_a_terminal_would_obey(&text, "a status report");
    }

    // -- shared helpers -----------------------------------------------------

    #[test]
    fn every_placement_reason_has_wording() {
        for reason in [
            PlacementReason::Forced,
            PlacementReason::MirrorWorkspace,
            PlacementReason::LiveMaster,
            PlacementReason::MirroredPane,
            PlacementReason::Assumed,
        ] {
            for placement in [Placement::Local, Placement::Remote] {
                let text = placement_label(&Where { placement, reason });
                assert!(!text.contains("()"), "empty reason for {reason:?}: {text}");
            }
        }
    }

    #[test]
    fn a_port_list_of_one_has_no_separator() {
        assert_eq!(ports(&[5173]), "5173");
        assert_eq!(ports(&[]), "");
        assert_eq!(ports(&[0, 65535]), "0, 65535");
    }

    #[test]
    fn every_render_ends_with_exactly_one_newline_per_line() {
        // A report that forgets its trailing newline runs into the next log
        // entry; one that doubles it pads the log.
        let texts = [
            render_stop(&StopOutcome {
                hosts: vec!["buildbox".to_string()],
                stopped: Vec::new(),
                elsewhere: Vec::new(),
            }),
            render_add(&AddOutcome {
                spec: spec("buildbox", 5173, 5173),
                applied: Applied::Forwarded,
                request_id: None,
                placement: local_placement(),
            }),
            render_status(&empty_status()),
        ];
        for text in texts {
            assert!(text.ends_with('\n'), "{text:?}");
            assert!(!text.ends_with("\n\n"), "{text:?}");
        }
    }
}
