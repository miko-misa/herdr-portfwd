//! The plugin actions, composed over the traits.
//!
//! `scan`, `add`, `list`, `stop`, `stop-all`, `open-url`, `reconcile`,
//! `status`. Each resolves a host from the invocation context, then either
//! applies a forward directly (server is local) or writes a request for the
//! local agent (server is remote).
//!
//! Rules these share, from `ARCHITECTURE.md`:
//! - the local port equals the remote port unless the user explicitly asked
//!   otherwise; a conflict is reported, never silently remapped;
//! - `open-url` accepts loopback hosts only, and re-validates the URL rather
//!   than trusting that the manifest pattern already did;
//! - nothing is forwarded without an explicit action, an accepted offer, or an
//!   allowlist match.
//!
//! # Which side of the SSH connection am I on?
//!
//! Nothing Herdr injects says so: `PluginInvocationContext` is identical under
//! `herdr --remote` and under herdr-mirror, and the remote server is a plain
//! `herdr server` with no marker in its environment. So the rule has to be
//! built from things a process can observe about *its own* machine, and
//! [`Actions::locate`] uses these, in order:
//!
//! 1. `HERDR_PORTFWD_MODE=local|remote` — an explicit override, for a setup
//!    the rules below get wrong. It always wins.
//! 2. **The invoking workspace carries this host's mirror prefix.** A
//!    `"<prefix>: <name>"` workspace only exists on a machine running
//!    herdr-mirror against that host, which is by definition the user's
//!    machine. Free: it is already in the invocation context.
//! 3. **A live control master for this host exists here.** Only
//!    `herdr-portfwd attach` and a previous local action create one, and both
//!    run on the user's machine; `attach` starts the master *before* exec'ing
//!    `herdr --remote`, so under `--remote` the master is on the far side and
//!    invisible to this process. `ssh -O check` only talks to a local socket,
//!    so this costs no network.
//! 4. **This machine's Herdr server mirrors the host.** Any pane in a
//!    `"<prefix>: ..."` workspace proves herdr-mirror is mirroring the host
//!    into the server that invoked us — again, the user's machine. This covers
//!    a global action with `--host`, where rule 2 has nothing to look at.
//! 5. Otherwise: **remote**. Write a request and let the local agent apply it.
//!
//! Defaulting to *remote* is the safe direction. Guessing "local" on the far
//! side would open an SSH connection from the host to itself and bind a port
//! nobody can reach; guessing "remote" on the near side leaves a request that
//! the user can see and act on.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::config::{self, HostsConfig, Settings};
use crate::confirm::ConfirmState;
use crate::context::{self, HostSource, Invocation};
use crate::discover::{self, ProcessTree};
use crate::herdr::{HerdrClient, Notification};
use crate::request::{Request, RequestLog};
use crate::ssh::{self, MasterState, SshRunner};
use crate::state::Store;
use crate::types::{Forward, ForwardSpec, Host, Listener, Origin, PaneRef};

/// Whether a record describes a forward that is actually up.
///
/// Defined in [`crate::types`] so the pure rendering layer can name it without
/// depending on this module, and re-exported here because this is where it is
/// decided. See [`Liveness`].
pub use crate::types::Liveness;

/// The only remote-side destination v1 forwards to.
pub const LOOPBACK: &str = "127.0.0.1";

/// Override for the placement rules. `local` or `remote`.
pub const MODE_ENV: &str = "HERDR_PORTFWD_MODE";

/// Hosts a loopback URL may name. Exact matches only: `localhost.evil.com`
/// starts with `localhost`, and `[0:0:0:0:0:0:0:1]` is `::1` written long, but
/// neither is worth the parser it would take to be sure.
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error(transparent)]
    Context(#[from] context::ContextError),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    State(#[from] crate::state::StateError),
    #[error(transparent)]
    Ssh(#[from] ssh::SshError),
    #[error(transparent)]
    Herdr(#[from] crate::herdr::HerdrError),
    #[error(
        "local port {local_port} is already in use by {holder}; \
         forwarding {host} port {remote_port} there would collide with it. \
         Nothing was changed. Re-run with --local-port <other> to bind a \
         different local port, which breaks anything that embeds port \
         {remote_port}: Vite HMR, OAuth redirect_uri, and absolute URLs the \
         app prints"
    )]
    LocalPortInUse {
        host: String,
        remote_port: u16,
        local_port: u16,
        holder: String,
    },
    #[error("{url:?} is not a forwardable loopback URL: {reason}")]
    InvalidUrl { url: String, reason: String },
    #[error("no URL to open: pass one as an argument, or let the link handler set HERDR_PLUGIN_CLICKED_URL")]
    NoUrl,
    /// Each command names itself: a hint that suggests a different verb from
    /// the one that was run reads as a correction, and sends the user to
    /// `add` when what they wanted was `stop`.
    #[error("no port given: pass one, for example `herdr-portfwd {command} 5173`")]
    NoPort { command: &'static str },
    #[error("could not read listeners on {host}: {message}")]
    Probe { host: String, message: String },
    #[error("could not open {url}: {message}")]
    Open { url: String, message: String },
    #[error(
        "`{action}` has to run on the machine holding the forward. This Herdr \
         server is on {host}, so run `herdr-portfwd {action} --host {host}` \
         where the local agent is running"
    )]
    NotHere { action: &'static str, host: String },
    /// Two hosts forwarding the same port is ordinary — the port number comes
    /// from whatever each dev server chose — so a bare `stop <port>` cannot
    /// pick for the user. Naming both is the whole answer.
    #[error(
        "port {port} is forwarded on more than one host ({hosts}); \
         say which with --host, for example \
         `herdr-portfwd stop {port} --host {first}`"
    )]
    PortOnSeveralHosts {
        port: u16,
        hosts: String,
        first: String,
    },
}

pub type Result<T> = std::result::Result<T, ActionError>;

// ---------------------------------------------------------------------------
// injectable edges
// ---------------------------------------------------------------------------

/// Reading the wall clock, so every timestamp in a test is chosen by the test.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Milliseconds since the Unix epoch, saturating at 0 for a clock set before it.
pub fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0)
    })
}

/// Whether a local port is already taken. Behind a seam because the real
/// implementation binds sockets and a test must not.
pub trait LocalPorts {
    fn in_use(&self, port: u16) -> bool;
}

/// Binds to decide. Purely local: no packet leaves the machine, and the socket
/// is dropped immediately.
pub struct SystemPorts;

impl LocalPorts for SystemPorts {
    fn in_use(&self, port: u16) -> bool {
        if port == 0 {
            // Port 0 means "pick one" to bind(2), so probing it proves nothing.
            return false;
        }
        // Both families: a listener bound only to ::1 is invisible to a v4
        // probe and vice versa. Only AddrInUse counts — a host without IPv6
        // fails the v6 bind for an unrelated reason.
        let v4 = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
        let v6 = TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port)));
        addr_in_use(&v4) || addr_in_use(&v6)
    }
}

fn addr_in_use(result: &io::Result<TcpListener>) -> bool {
    matches!(result, Err(err) if err.kind() == io::ErrorKind::AddrInUse)
}

/// Handing a URL to the desktop. Argv array, never a shell.
pub trait UrlOpener {
    fn open(&self, url: &str) -> std::result::Result<(), String>;
}

pub struct SystemOpener;

/// The program and argv that hand `url` to the desktop.
///
/// Split out of [`SystemOpener::open`] so a test can pin the shape rather than
/// trusting it. This is the crate's only handoff to a program that renders the
/// URL, and the URL's path, query and fragment are copied verbatim out of
/// whatever a remote pane printed (see [`parse_loopback_url`]): `;`, `|`, a
/// backtick, `$(` and spaces all survive validation. As one argv word they are
/// bytes; through `sh -c` they would be command injection reachable from a
/// Ctrl+click. Invariant 9.
fn opener_argv(url: &str) -> (&'static str, Vec<String>) {
    let bin = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    (bin, vec![url.to_string()])
}

/// What a finished opener process means. Separated so the failure branch is
/// reachable from a test: an `open` that exited non-zero opened nothing, and
/// reporting it as success leaves the user waiting for a window.
fn opener_outcome(bin: &str, status: std::process::ExitStatus) -> std::result::Result<(), String> {
    if status.success() {
        Ok(())
    } else {
        Err(format!("{bin} exited with {status}"))
    }
}

impl UrlOpener for SystemOpener {
    fn open(&self, url: &str) -> std::result::Result<(), String> {
        let (bin, args) = opener_argv(url);
        let status = std::process::Command::new(bin)
            .args(&args)
            .status()
            .map_err(|err| format!("{bin}: {err}"))?;
        opener_outcome(bin, status)
    }
}

/// Running one of [`crate::discover`]'s fixed commands on the machine that owns
/// the listeners, and returning its stdout.
pub trait HostProbe {
    fn run(&self, command: &str) -> std::result::Result<String, String>;
}

/// Probe a host across its control master.
struct SshProbe<'a> {
    ssh: &'a dyn SshRunner,
    target: &'a str,
    control_path: PathBuf,
}

impl HostProbe for SshProbe<'_> {
    fn run(&self, command: &str) -> std::result::Result<String, String> {
        self.ssh
            .exec(self.target, &self.control_path, command)
            .map_err(|err| err.to_string())
    }
}

/// Probe the machine this process is on, for `herdr --remote`, where the plugin
/// already runs on the host whose listeners it wants.
pub struct HereProbe;

impl HostProbe for HereProbe {
    fn run(&self, command: &str) -> std::result::Result<String, String> {
        let argv = fixed_argv(command);
        let (bin, args) = argv
            .split_first()
            .ok_or_else(|| "empty discovery command".to_string())?;
        let output = std::process::Command::new(bin)
            .args(args)
            .output()
            .map_err(|err| format!("{bin}: {err}"))?;
        if !output.status.success() {
            return Err(format!("{bin} exited with {}", output.status));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Split one of `discover`'s command constants into argv.
///
/// Safe only because those constants are fixed literals with no quoting and no
/// interpolation; `fixed_argv_matches_the_discover_constants` pins that.
fn fixed_argv(command: &str) -> Vec<String> {
    command.split_whitespace().map(str::to_string).collect()
}

/// Resolving a host to the Herdr server that owns its panes.
///
/// Under `herdr --remote` that is the server this plugin was invoked by, for
/// every host. Under herdr-mirror the panes live on the host's own server,
/// reached by running its `herdr` over the control master, so the caller builds
/// one client per host.
pub trait HostHerdr {
    fn client(&self, host: &Host) -> Result<Box<dyn HerdrClient + '_>>;
}

// ---------------------------------------------------------------------------
// placement
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// This process can open the local end of the forward itself.
    Local,
    /// This process is on the far side; it can only record the intent.
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementReason {
    /// `HERDR_PORTFWD_MODE`.
    Forced,
    /// The invoking workspace carries this host's mirror prefix.
    MirrorWorkspace,
    /// A control master for this host is live on this machine.
    LiveMaster,
    /// Some workspace on this Herdr server carries this host's mirror prefix.
    MirroredPane,
    /// None of the above; assume the far side.
    Assumed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Where {
    pub placement: Placement,
    pub reason: PlacementReason,
}

impl Where {
    pub fn is_local(&self) -> bool {
        self.placement == Placement::Local
    }
}

/// Read a `HERDR_PORTFWD_MODE` value. Pure, so the rule is testable without the
/// process environment. Anything unrecognised is ignored rather than fatal: a
/// typo must not make the plugin refuse to run.
pub fn placement_override(value: Option<&str>) -> Option<Placement> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "local" => Some(Placement::Local),
        "remote" => Some(Placement::Remote),
        _ => None,
    }
}

fn placement_from_env() -> Option<Placement> {
    placement_override(std::env::var(MODE_ENV).ok().as_deref())
}

// ---------------------------------------------------------------------------
// environment
// ---------------------------------------------------------------------------

/// Everything an action needs that is not a live connection to something.
pub struct Env {
    pub hosts: HostsConfig,
    pub settings: Settings,
    pub context: Invocation,
    /// `--host` or `HERDR_PORTFWD_HOST`.
    pub explicit_host: Option<String>,
    pub control_dir: PathBuf,
    /// Forced placement; `None` runs the detection rules.
    pub placement: Option<Placement>,
    pub now_ms: Clock,
}

impl Env {
    /// Load from the config files and the process environment.
    pub fn from_process(explicit_host: Option<String>) -> Result<Self> {
        Ok(Self {
            hosts: config::load_hosts(&config::hosts_file_candidates())?,
            settings: config::load_settings(&config::settings_file())?,
            context: context::from_env()?,
            explicit_host,
            control_dir: ssh::control_socket_dir()?,
            placement: placement_from_env(),
            now_ms: system_clock(),
        })
    }
}

// ---------------------------------------------------------------------------
// outcomes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Applied {
    /// The forward was added to a master on this machine.
    Forwarded,
    /// It was already live; nothing changed.
    AlreadyLive,
    /// Recorded for the local agent to apply.
    Requested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddOutcome {
    pub spec: ForwardSpec,
    pub applied: Applied,
    pub request_id: Option<u64>,
    pub placement: Where,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenOutcome {
    /// The URL to point a local browser at, with the port the forward actually
    /// bound.
    pub url: String,
    pub add: AddOutcome,
    pub opened: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ForwardStatus {
    pub forward: Forward,
    pub live: Liveness,
}

/// Which hosts a listing or a stop covers.
///
/// Every host, unless the invocation narrowed it. A forward on a host this
/// invocation did not happen to resolve to is still a port open on this
/// machine: hiding it made it unstoppable, and made a one-host listing
/// indistinguishable from a one-forward world.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The default: every host that has a forward or a queued request.
    AllHosts,
    /// `--host`, or `HERDR_PORTFWD_HOST`.
    Host(String),
}

impl Scope {
    /// The host this scope narrows to, if it narrows at all.
    pub fn host(&self) -> Option<&str> {
        match self {
            Scope::AllHosts => None,
            Scope::Host(name) => Some(name.as_str()),
        }
    }

    fn covers(&self, host: &str) -> bool {
        self.host().is_none_or(|only| only == host)
    }
}

/// One host's section of a listing. Master and placement are per host, because
/// they are questions about a connection rather than about the invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostForwards {
    pub host: String,
    pub master_running: Option<bool>,
    pub forwards: Vec<ForwardStatus>,
    pub pending: Vec<Request>,
    pub placement: Where,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListReport {
    pub scope: Scope,
    /// Hosts with something to show, in configuration order. A host with no
    /// forward and no pending request is left out rather than listed empty —
    /// except the one `--host` named, which is what the user asked about.
    pub hosts: Vec<HostForwards>,
}

impl ListReport {
    /// Forwards across every host in the report.
    pub fn total(&self) -> usize {
        self.hosts.iter().map(|host| host.forwards.len()).sum()
    }
}

/// What the manager pane opens with: every host's forwards, and the host this
/// invocation asked to look at.
///
/// The pane always holds every row — [`ListReport::scope`] here is always
/// [`Scope::AllHosts`], and the rows really are every host's — because the
/// screen narrows on top of what it holds. That is what lets its header say how
/// many forwards it is hiding, and what lets `h` reveal the hosts it is not
/// showing.
///
/// So `--host` cannot narrow the *listing* here the way it narrows
/// [`Actions::list`]: a pane whose rows were narrowed and whose filter was not
/// has a header describing a scope its rows do not have, and `h` with nothing
/// behind it to reveal. It narrows the *screen* instead, through `focus`, which
/// makes the flag and the key the same mechanism rather than two that can
/// disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneListing {
    /// Every live forward on every host, scoped [`Scope::AllHosts`].
    pub report: ListReport,
    /// The screen's initial narrowing: what `--host` or `HERDR_PORTFWD_HOST`
    /// named, resolved to its configured name so it matches the host a record
    /// carries, or [`Scope::AllHosts`] when the invocation named none.
    pub focus: Scope,
}

/// One forward as a screen names it.
///
/// `(host, local_port)` is what identifies a record — the pair a cancel has to
/// name — and the remote port rides along so a row cannot cancel a record that
/// was rewritten between the frame being drawn and the key being pressed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ForwardKey {
    pub host: String,
    pub remote_port: u16,
    pub local_port: u16,
}

impl ForwardKey {
    pub fn matches(&self, forward: &Forward) -> bool {
        forward.spec.host == self.host
            && forward.spec.local_port == self.local_port
            && forward.spec.remote_port == self.remote_port
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StopOutcome {
    /// Hosts this stop acted on, or was looking at when it found nothing.
    /// Empty when it searched every host and matched nowhere.
    pub hosts: Vec<String>,
    /// Empty when nothing matched: cancelling a forward that was never created
    /// is a no-op, not an error (invariant 4).
    pub stopped: Vec<Forward>,
    /// Hosts that do carry the port, when the stop was narrowed away from them.
    /// A refusal that says "nothing to stop" while the port is live one host
    /// over is the report that sent a user looking for a bug in `ssh`.
    pub elsewhere: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub listener: Listener,
    pub pane: Option<PaneRef>,
    /// A record already exists for this remote port on this host.
    pub already_forwarded: bool,
    /// Whether the matching local port is free right now.
    ///
    /// `None` on the far side: under `herdr --remote` this process runs on the
    /// host, where binding a socket says nothing about the user's machine — and
    /// would answer "in use" for every candidate, since every candidate is by
    /// construction a listener on this very machine. [`Actions::list`] reports
    /// [`Liveness::Unknown`] for the same reason.
    pub local_port_free: Option<bool>,
    /// The workspace allowlist covers this port, so the agent may take it
    /// without asking (invariant 7's third case).
    pub auto_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanReport {
    pub host: String,
    pub candidates: Vec<Candidate>,
    /// Listeners dropped because nothing tied them to a Herdr pane.
    pub unattributed: usize,
    /// Listeners dropped because their bind address is not reachable through a
    /// loopback forward.
    pub unforwardable: usize,
    pub placement: Where,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileTrigger {
    /// `[[startup]]`: may open connections to restore forwards.
    Startup,
    /// `[[events]]`: must not open a connection on every workspace focus.
    Event,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    /// The host is no longer in `hosts.toml`.
    HostGone,
    /// The owning pane is gone and the forward was not user-pinned.
    PaneGone,
    /// No control master, so the forward is provably not live.
    MasterGone,
    Unreachable(String),
    ReAddFailed(String),
    /// Another process stopped the forward while this pass was deciding about
    /// it. That write is newer than this pass's snapshot, so it wins and the
    /// record stays gone.
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dropped {
    pub forward: Forward,
    pub reason: DropReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReconcileReport {
    pub trigger: ReconcileTrigger,
    pub kept: Vec<Forward>,
    pub readded: Vec<Forward>,
    pub dropped: Vec<Dropped>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostStatus {
    pub host: Host,
    pub master_running: Option<bool>,
    /// `None` when the record file could not be read, which is not the same
    /// host with no forwards on it. An empty list here is the answer that
    /// `status` exists to give honestly; `null` is the absence of one, and
    /// [`StatusReport::state_error`] says why.
    pub forwards: Option<Vec<Forward>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SelectedHost {
    pub name: String,
    pub source: &'static str,
    pub placement: Where,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    pub hosts_file: Option<PathBuf>,
    pub shadowed: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub state_file: PathBuf,
    pub request_file: PathBuf,
    pub selected: Option<SelectedHost>,
    /// Why no host could be selected, when `selected` is `None`.
    pub selection_error: Option<String>,
    pub hosts: Vec<HostStatus>,
    /// `None` when the request file could not be read. See
    /// [`StatusReport::request_error`].
    pub pending_requests: Option<usize>,
    /// Why the record file yielded nothing, when it could not be read. Every
    /// [`HostStatus::forwards`] is `None` in that case.
    pub state_error: Option<String>,
    /// Why the request queue yielded nothing, when it could not be read.
    pub request_error: Option<String>,
}

impl StatusReport {
    /// What `status` could not find out, in the order the report prints it.
    ///
    /// Read by the caller to decide the exit code: a report is still a report
    /// when part of it is missing, but a run that could not read a file has
    /// failed and says so the way every other command does.
    pub fn failures(&self) -> Vec<&str> {
        [self.state_error.as_deref(), self.request_error.as_deref()]
            .into_iter()
            .flatten()
            .collect()
    }
}

fn source_label(source: HostSource) -> &'static str {
    match source {
        HostSource::Explicit => "explicit",
        HostSource::WorkspacePrefix => "workspace-prefix",
        HostSource::Default => "default_host",
        HostSource::OnlyHost => "only-host",
    }
}

// ---------------------------------------------------------------------------
// URL validation
// ---------------------------------------------------------------------------

/// A URL that survived re-validation: loopback host, explicit port, nothing
/// else that could point somewhere other than where it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopbackUrl {
    pub scheme: &'static str,
    /// Lowercased, exactly as one of [`LOOPBACK_HOSTS`].
    pub host: String,
    pub port: u16,
    /// Path, query and fragment, verbatim.
    pub rest: String,
}

impl LoopbackUrl {
    /// The same URL against a different port, for a forward the user remapped.
    pub fn with_port(&self, port: u16) -> String {
        format!("{}://{}:{}{}", self.scheme, self.host, port, self.rest)
    }
}

/// Re-validate a clicked URL.
///
/// The manifest's `pattern` already filtered it, but a link handler is the one
/// place untrusted terminal output reaches this crate and the regex is one
/// backtrack away from being wrong. Everything here is a rejection by default:
/// only a known scheme, a host that is exactly loopback, and a plain decimal
/// port in `1..=65535` get through.
pub fn parse_loopback_url(raw: &str) -> std::result::Result<LoopbackUrl, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("it is empty".to_string());
    }
    if !trimmed.is_ascii() {
        // A non-ASCII host can render as `localhost` and resolve elsewhere.
        return Err("it contains non-ASCII characters".to_string());
    }
    if trimmed.bytes().any(|b| b.is_ascii_control()) {
        return Err("it contains control characters".to_string());
    }

    let (scheme, after_scheme) = if let Some(rest) = strip_prefix_ci(trimmed, "http://") {
        ("http", rest)
    } else if let Some(rest) = strip_prefix_ci(trimmed, "https://") {
        ("https", rest)
    } else {
        return Err("only http:// and https:// URLs can be forwarded".to_string());
    };

    let end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let (authority, rest) = after_scheme.split_at(end);

    if authority.is_empty() {
        return Err("it names no host".to_string());
    }
    if authority.contains('@') {
        return Err(
            "it carries userinfo before the host, which hides where it really points".to_string(),
        );
    }
    if authority.contains('\\') {
        return Err("its authority contains a backslash".to_string());
    }
    if authority.bytes().any(|b| b.is_ascii_whitespace()) {
        return Err("its authority contains whitespace".to_string());
    }

    let (host, port_text) = split_host_port(authority)?;
    let host = host.to_ascii_lowercase();
    if !LOOPBACK_HOSTS.contains(&host.as_str()) {
        return Err(format!(
            "host {host:?} is not loopback; only {} are forwarded",
            LOOPBACK_HOSTS.join(", ")
        ));
    }

    let port = parse_port(port_text)?;
    Ok(LoopbackUrl {
        scheme,
        host,
        port,
        rest: rest.to_string(),
    })
}

fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &value[prefix.len()..])
}

fn split_host_port(authority: &str) -> std::result::Result<(&str, &str), String> {
    if let Some(close) = authority.find(']') {
        if !authority.starts_with('[') {
            return Err("its authority has a stray bracket".to_string());
        }
        let (host, tail) = authority.split_at(close + 1);
        let port = tail
            .strip_prefix(':')
            .ok_or_else(|| "it has no port; there is nothing to forward".to_string())?;
        return Ok((host, port));
    }
    if authority.starts_with('[') {
        return Err("its bracketed host is unterminated".to_string());
    }
    match authority.split(':').count() {
        1 => Err("it has no port; there is nothing to forward".to_string()),
        2 => {
            let (host, port) = authority
                .split_once(':')
                .ok_or_else(|| "it has no port".to_string())?;
            Ok((host, port))
        }
        // `a:1:2` is either a bare IPv6 address or a second port. Both are
        // ways of writing something other than what the text appears to say.
        _ => Err("its authority has more than one colon".to_string()),
    }
}

fn parse_port(text: &str) -> std::result::Result<u16, String> {
    if text.is_empty() {
        return Err("its port is empty".to_string());
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("port {text:?} is not a plain decimal number"));
    }
    if text.len() > 5 {
        return Err(format!("port {text:?} has too many digits"));
    }
    if text.len() > 1 && text.starts_with('0') {
        // `05173` and `5173` are the same number to a parser and different
        // strings to a human reading the URL.
        return Err(format!("port {text:?} has a leading zero"));
    }
    let port: u32 = text
        .parse()
        .map_err(|_| format!("port {text:?} is not a number"))?;
    if port == 0 || port > u32::from(u16::MAX) {
        return Err(format!("port {port} is outside 1..=65535"));
    }
    Ok(port as u16)
}

// ---------------------------------------------------------------------------
// the actions
// ---------------------------------------------------------------------------

/// The plugin actions, over the seams that reach outside this process.
///
/// `herdr` is the server that invoked us — always on this machine.
/// `host_herdr` resolves a *host* to the server that owns its panes, which is a
/// different thing under herdr-mirror and the same thing under `--remote`.
pub struct Actions<'a> {
    pub ssh: &'a dyn SshRunner,
    pub herdr: &'a dyn HerdrClient,
    pub host_herdr: &'a dyn HostHerdr,
    pub ports: &'a dyn LocalPorts,
    pub opener: &'a dyn UrlOpener,
    pub store: &'a Store,
    pub requests: &'a RequestLog,
    /// Overrides the probe [`Actions::scan`] would build from the placement.
    pub probe: Option<&'a dyn HostProbe>,
    pub env: Env,
}

impl Actions<'_> {
    fn now(&self) -> u64 {
        (self.env.now_ms)()
    }

    /// The host this invocation acts on.
    pub fn resolve(&self) -> Result<context::ResolvedHost> {
        Ok(context::resolve_host(
            self.env.explicit_host.as_deref(),
            &self.env.context,
            &self.env.hosts.hosts,
            self.env.hosts.default_host.as_deref(),
        )?)
    }

    fn control_path(&self, host: &Host) -> Result<PathBuf> {
        Ok(ssh::control_socket_path(&self.env.control_dir, &host.name)?)
    }

    /// Which side of the connection this process is on, for `host`.
    /// The rules and their order are argued for in the module docs.
    pub fn locate(&self, host: &Host, prefix_matched: bool) -> Where {
        if let Some(placement) = self.env.placement {
            return Where {
                placement,
                reason: PlacementReason::Forced,
            };
        }
        if prefix_matched {
            return Where {
                placement: Placement::Local,
                reason: PlacementReason::MirrorWorkspace,
            };
        }
        if self.master_is_running(host) == Some(true) {
            return Where {
                placement: Placement::Local,
                reason: PlacementReason::LiveMaster,
            };
        }
        if self.server_mirrors(host) {
            return Where {
                placement: Placement::Local,
                reason: PlacementReason::MirroredPane,
            };
        }
        Where {
            placement: Placement::Remote,
            reason: PlacementReason::Assumed,
        }
    }

    /// `None` when the question could not be answered — an over-long control
    /// path, or `ssh` refusing to run. Neither is evidence of placement.
    fn master_is_running(&self, host: &Host) -> Option<bool> {
        let path = self.control_path(host).ok()?;
        match self.ssh.check_master(&host.target, &path) {
            Ok(MasterState::Running) => Some(true),
            Ok(MasterState::Absent) => Some(false),
            Err(_) => None,
        }
    }

    /// Whether the Herdr server that invoked us has a workspace mirrored from
    /// `host`. A failed pane list is not evidence either way.
    fn server_mirrors(&self, host: &Host) -> bool {
        let Some(prefix) = host.prefix.as_deref() else {
            return false;
        };
        // Labelled, not the plain listing: `pane_list` never carries a label,
        // so reading one off it saw `None` everywhere and concluded that a
        // machine covered in mirrors had none.
        let Ok(panes) = self.herdr.pane_list_labelled() else {
            return false;
        };
        panes.iter().any(|pane| {
            pane.workspace_label
                .as_deref()
                .and_then(context::workspace_label_prefix)
                == Some(prefix)
        })
    }

    fn locate_resolved(&self, resolved: &context::ResolvedHost) -> Where {
        self.locate(
            &resolved.host,
            resolved.source == HostSource::WorkspacePrefix,
        )
    }

    // -- add ----------------------------------------------------------------

    /// Forward `remote_port` to the same local port, or to `local_port` when
    /// the user named one.
    pub fn add(&self, remote_port: Option<u16>, local_port: Option<u16>) -> Result<AddOutcome> {
        let remote_port = remote_port.ok_or(ActionError::NoPort { command: "add" })?;
        let resolved = self.resolve()?;
        let placement = self.locate_resolved(&resolved);
        // `unwrap_or(remote_port)` is the only place a local port is chosen,
        // and it can only differ when the caller passed one.
        let spec = ForwardSpec {
            host: resolved.host.name.clone(),
            remote_port,
            local_port: local_port.unwrap_or(remote_port),
            remote_bind: LOOPBACK.to_string(),
            origin: Origin::Explicit,
            pane_id: self.env.context.pane_id.clone(),
        };
        self.apply_or_request(&resolved.host, spec, placement)
    }

    /// Apply `spec` here, or record it for the machine that can.
    ///
    /// Invariant 1 lives here: a local port that is already taken is reported,
    /// with both port numbers and what renumbering would break, and nothing is
    /// changed. There is no path that picks a different port on its own.
    pub fn apply_or_request(
        &self,
        host: &Host,
        spec: ForwardSpec,
        placement: Where,
    ) -> Result<AddOutcome> {
        self.apply_or_request_opening(host, spec, placement, None)
    }

    /// [`Actions::apply_or_request`], with a URL for whoever ends up applying
    /// the forward to open afterwards.
    ///
    /// Only the request path carries it: when the forward is applied here, this
    /// process is on the machine with the browser and opens the URL itself.
    /// What the applier is allowed to believe about the string is
    /// [`Request::open_url`] — nothing, until it has re-validated it.
    pub fn apply_or_request_opening(
        &self,
        host: &Host,
        spec: ForwardSpec,
        placement: Where,
        open_url: Option<&str>,
    ) -> Result<AddOutcome> {
        if placement.placement == Placement::Remote {
            let request = self.requests.enqueue_opening(
                spec.clone(),
                self.now(),
                open_url.map(str::to_string),
            )?;
            return Ok(AddOutcome {
                spec,
                applied: Applied::Requested,
                request_id: Some(request.id),
                placement,
            });
        }

        let occupied = self.ports.in_use(spec.local_port);
        let recorded = self.store.get(&spec.host, spec.local_port)?;
        if occupied {
            match recorded {
                Some(existing)
                    if existing.spec.remote_port == spec.remote_port
                        && existing.spec.remote_bind == spec.remote_bind =>
                {
                    // Nothing to open, but possibly something to record: an
                    // explicit add over a forward the agent made for a pane is
                    // the user pinning it, and a record that still says
                    // otherwise loses the port when that pane exits.
                    let existing = match crate::state::promoted(&existing, &spec) {
                        Some(promoted) => {
                            self.store.upsert(promoted.clone())?;
                            promoted
                        }
                        None => existing,
                    };
                    return Ok(AddOutcome {
                        spec: existing.spec,
                        applied: Applied::AlreadyLive,
                        request_id: None,
                        placement,
                    });
                }
                Some(existing) => {
                    return Err(ActionError::LocalPortInUse {
                        host: spec.host,
                        remote_port: spec.remote_port,
                        local_port: spec.local_port,
                        holder: format!(
                            "the forward of {} port {}",
                            existing.spec.host, existing.spec.remote_port
                        ),
                    })
                }
                None => {
                    return Err(ActionError::LocalPortInUse {
                        host: spec.host,
                        remote_port: spec.remote_port,
                        local_port: spec.local_port,
                        holder: "another process on this machine".to_string(),
                    })
                }
            }
        }
        // A record over a free port is stale by definition, and invariant 3
        // says it must not shadow the port: it is replaced, not obeyed.

        let control_path = self.control_path(host)?;
        self.ssh.ensure_master(&host.target, &control_path)?;
        self.ssh.add_forward(&host.target, &control_path, &spec)?;
        self.store.upsert(Forward {
            spec: spec.clone(),
            created_unix_ms: self.now(),
        })?;
        Ok(AddOutcome {
            spec,
            applied: Applied::Forwarded,
            request_id: None,
            placement,
        })
    }

    // -- open-url -----------------------------------------------------------

    /// The URL a click carried, from the argument or from the context Herdr
    /// injected.
    fn clicked(&self, url: Option<&str>) -> Result<String> {
        url.map(str::to_string)
            .or_else(|| self.env.context.clicked_url.clone())
            .ok_or(ActionError::NoUrl)
    }

    /// Forward the port in a loopback URL and open it on this machine.
    pub fn open_url(&self, url: Option<&str>) -> Result<OpenOutcome> {
        let raw = self.clicked(url)?;
        self.open_url_for(&raw, self.env.context.pane_id.clone(), true)
    }

    /// [`Actions::open_url`], with the pane the URL belongs to named and the
    /// browser optional.
    ///
    /// Both extras exist for the `confirm` pane. It runs in a popup Herdr
    /// launched *without* a pane identity, and the context it is handed names
    /// whatever pane is focused when the popup opens rather than the one that
    /// printed the URL — so attribution has to be passed in, or the forward
    /// belongs to the wrong pane and `reconcile` releases it at the wrong
    /// moment. And "forward only" is a choice the screen offers, which here
    /// means: apply exactly the same forward, and hand nobody the URL.
    pub fn open_url_for(
        &self,
        raw: &str,
        pane_id: Option<String>,
        open_browser: bool,
    ) -> Result<OpenOutcome> {
        // Re-validated even when the caller says it already did: under
        // `herdr --remote` this string made a round trip through an environment
        // variable on the host whose terminal printed it. Invariant 11.
        let parsed = parse_loopback_url(raw).map_err(|reason| ActionError::InvalidUrl {
            url: raw.to_string(),
            reason,
        })?;

        let resolved = self.resolve()?;
        let placement = self.locate_resolved(&resolved);
        let spec = ForwardSpec {
            host: resolved.host.name.clone(),
            remote_port: parsed.port,
            // A clicked URL never remaps: the port in the text is the point.
            local_port: parsed.port,
            remote_bind: LOOPBACK.to_string(),
            origin: Origin::Explicit,
            pane_id,
        };
        // Re-rendered from the validated parts rather than passed through as
        // the raw click text, and pinned to the port this request asks for, so
        // the string that travels is one this process built. The local port
        // cannot move under a request: `apply_or_request` queues the spec as
        // written, and only the local branch — which opens the URL here — can
        // meet a remap at all.
        let requested_url = parsed.with_port(spec.local_port);
        // A forward the user asked for without a browser carries no URL into
        // the request either: the agent opens what the record tells it to, so
        // leaving it out is the only way "forward only" survives the trip.
        let carried = open_browser.then_some(requested_url.as_str());
        let add = self.apply_or_request_opening(&resolved.host, spec, placement, carried)?;
        let local_url = parsed.with_port(add.spec.local_port);

        if !open_browser {
            return Ok(OpenOutcome {
                url: local_url,
                add,
                opened: false,
            });
        }

        if placement.is_local() {
            self.opener
                .open(&local_url)
                .map_err(|message| ActionError::Open {
                    url: local_url.clone(),
                    message,
                })?;
            return Ok(OpenOutcome {
                url: local_url,
                add,
                opened: true,
            });
        }

        // The browser is on the other end of the SSH connection, so the most
        // this side can do is say where to point it.
        self.herdr.notify(&Notification {
            title: "Port forward requested".to_string(),
            body: format!("{local_url} opens once the local agent applies the forward"),
        })?;
        Ok(OpenOutcome {
            url: local_url,
            add,
            opened: false,
        })
    }

    /// Everything the `confirm` pane needs to describe a click, without doing
    /// any of it.
    ///
    /// A URL that cannot be forwarded at all is still an error — there is
    /// nothing to confirm about a click this tool will never act on, and the
    /// refusal names the reason. A host that cannot be *resolved* is not: that
    /// is a question the screen can put to a person, so it becomes
    /// [`crate::confirm::ConfirmState::problem`] rather than a failure in a log
    /// nobody is reading.
    pub fn preview_open_url(&self, url: Option<&str>) -> Result<ConfirmState> {
        let raw = self.clicked(url)?;
        let parsed = parse_loopback_url(&raw).map_err(|reason| ActionError::InvalidUrl {
            url: raw.clone(),
            reason,
        })?;
        let mut state = ConfirmState {
            url: parsed.with_port(parsed.port),
            remote_port: parsed.port,
            local_port: parsed.port,
            workspace: self.env.context.workspace_label.clone(),
            agent: self.env.context.agent.clone(),
            pane_id: self.env.context.pane_id.clone(),
            ..ConfirmState::default()
        };

        let resolved = match self.resolve() {
            Ok(resolved) => resolved,
            Err(err) => {
                state.problem = Some(err.to_string());
                return Ok(state);
            }
        };
        let placement = self.locate_resolved(&resolved);
        state.host = Some(resolved.host.name.clone());
        state.applies_here = placement.is_local();
        // Both of the next two are questions about the machine that would bind
        // the port, and under `herdr --remote` this process is not on it. The
        // records live beside the local agent that applies the forwards, and
        // the store reachable from here has never seen one — so reading it
        // would answer `false` to a question it cannot hear. `None` says so,
        // exactly as it does for the port probe below.
        state.already_forwarded = if placement.is_local() {
            Some(
                self.store
                    .get(&resolved.host.name, parsed.port)?
                    .is_some_and(|forward| forward.spec.remote_port == parsed.port),
            )
        } else {
            None
        };
        // Only meaningful on the machine that would bind it. On the far side
        // every candidate is by construction a listener on *this* host, so a
        // probe here answers a different question; `None` says so.
        state.local_free = placement
            .is_local()
            .then(|| !self.ports.in_use(parsed.port));
        Ok(state)
    }

    // -- scope --------------------------------------------------------------

    /// Which hosts this invocation acts on.
    ///
    /// Only an explicit `--host` narrows. Everything else — the mirror prefix,
    /// `default_host`, the only-host rule — decides which host a *new* forward
    /// is made on, and reading it as "the only host worth showing" is what made
    /// a forward on a second host invisible and unstoppable.
    fn scope(&self) -> Result<Scope> {
        match self
            .env
            .explicit_host
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            // Resolved rather than taken verbatim, so an unknown name is still
            // the error it always was rather than an empty listing.
            Some(_) => Ok(Scope::Host(self.resolve()?.host.name)),
            None => Ok(Scope::AllHosts),
        }
    }

    /// The configured host of that name, or a stand-in using the name as its
    /// ssh target.
    ///
    /// The same fallback `attach` makes. A record whose host has left
    /// `hosts.toml` is still an open port until `reconcile` gets to it, so it
    /// is still listed and still stoppable rather than vanishing from the one
    /// screen that could do something about it.
    fn host_named(&self, name: &str) -> Host {
        self.env
            .hosts
            .by_name(name)
            .cloned()
            .unwrap_or_else(|| Host {
                name: name.to_string(),
                target: name.to_string(),
                prefix: None,
            })
    }

    /// Placement for an arbitrary host.
    ///
    /// The mirror-prefix rule only applies to the host this invocation resolved
    /// to *through* that prefix: another host's forwards were not made by the
    /// workspace whose label carries it.
    fn locate_named(&self, host: &Host) -> Where {
        let prefix_matched = matches!(
            self.resolve(),
            Ok(resolved)
                if resolved.source == HostSource::WorkspacePrefix
                    && resolved.host.name == host.name
        );
        self.locate(host, prefix_matched)
    }

    /// Hosts worth a section: those with a record or a queued request, in
    /// configuration order, plus any host named only by a record.
    fn hosts_in_scope(
        &self,
        scope: &Scope,
        recorded: &[Forward],
        pending: &[Request],
    ) -> Vec<Host> {
        if let Scope::Host(name) = scope {
            return vec![self.host_named(name)];
        }
        let mentions = |name: &str| {
            recorded.iter().any(|f| f.spec.host == name)
                || pending.iter().any(|request| request.host == name)
        };
        let mut hosts: Vec<Host> = Vec::new();
        for host in &self.env.hosts.hosts {
            if mentions(&host.name) {
                hosts.push(host.clone());
            }
        }
        let named = recorded
            .iter()
            .map(|forward| forward.spec.host.as_str())
            .chain(pending.iter().map(|request| request.host.as_str()));
        for name in named {
            if !hosts.iter().any(|host| host.name == name) {
                hosts.push(self.host_named(name));
            }
        }
        hosts
    }

    // -- list ---------------------------------------------------------------

    /// Recorded forwards, with whether each looks live.
    ///
    /// Every host by default; `--host` narrows. See [`Actions::scope`].
    ///
    /// This is what `--host` means to the CLI: print that host's forwards. A
    /// *screen* narrows differently — see [`Actions::list_for_pane`].
    pub fn list(&self) -> Result<ListReport> {
        self.list_in(self.scope()?)
    }

    /// The listing the manager pane opens with.
    ///
    /// Every host's forwards whatever `--host` said, plus that narrowing as the
    /// pane's initial filter. The argument for splitting it this way is on
    /// [`PaneListing`].
    ///
    /// The narrowing is resolved first, so an invocation naming a host that
    /// does not exist still fails the way `list` does rather than opening a
    /// pane filtered to nothing.
    pub fn list_for_pane(&self) -> Result<PaneListing> {
        let focus = self.scope()?;
        Ok(PaneListing {
            report: self.list_in(Scope::AllHosts)?,
            focus,
        })
    }

    fn list_in(&self, scope: Scope) -> Result<ListReport> {
        let recorded = self.store.list()?;
        let pending = self.requests.pending()?;

        let hosts = self
            .hosts_in_scope(&scope, &recorded, &pending)
            .into_iter()
            .map(|host| {
                let placement = self.locate_named(&host);
                let master_running = if placement.is_local() {
                    self.master_is_running(&host)
                } else {
                    None
                };
                let forwards = recorded
                    .iter()
                    .filter(|forward| forward.spec.host == host.name)
                    .map(|forward| {
                        let live = if !placement.is_local() {
                            Liveness::Unknown
                        } else if self.ports.in_use(forward.spec.local_port) {
                            Liveness::Live
                        } else {
                            Liveness::Missing
                        };
                        ForwardStatus {
                            forward: forward.clone(),
                            live,
                        }
                    })
                    .collect();
                HostForwards {
                    host: host.name.clone(),
                    master_running,
                    forwards,
                    pending: pending
                        .iter()
                        .filter(|request| request.host == host.name)
                        .cloned()
                        .collect(),
                    placement,
                }
            })
            .collect();

        Ok(ListReport { scope, hosts })
    }

    // -- stop / stop-all ----------------------------------------------------

    /// Stop the forward for `port`, matched on the remote port first and the
    /// local port second. Nothing matching is a no-op (invariant 4).
    ///
    /// Every host is searched unless `--host` narrowed it, because that is what
    /// `list` shows. A port on two hosts is refused rather than guessed at, and
    /// a port the narrowing hid is named in the outcome instead of being
    /// reported as nothing at all.
    pub fn stop(&self, port: Option<u16>) -> Result<StopOutcome> {
        let port = port.ok_or(ActionError::NoPort { command: "stop" })?;
        let scope = self.scope()?;
        let recorded = self.store.list()?;
        let matching = matching_port(&recorded, port, &scope);

        let hosts = host_order(&matching);
        match hosts.as_slice() {
            [] => {
                self.not_here_if_remote("stop")?;
                Ok(StopOutcome {
                    hosts: scope.host().map(str::to_string).into_iter().collect(),
                    stopped: Vec::new(),
                    elsewhere: host_order(&matching_port(&recorded, port, &Scope::AllHosts)),
                })
            }
            [_] => self.stop_forwards("stop", matching),
            [first, ..] => Err(ActionError::PortOnSeveralHosts {
                port,
                hosts: hosts.join(", "),
                first: first.clone(),
            }),
        }
    }

    /// Stop every recorded forward, on every host unless `--host` narrows it.
    ///
    /// The master is left running: it is shared with the local agent and with
    /// the next `add`, and `-O exit` would take those with it.
    pub fn stop_all(&self) -> Result<StopOutcome> {
        let scope = self.scope()?;
        let matching: Vec<Forward> = self
            .store
            .list()?
            .into_iter()
            .filter(|forward| scope.covers(&forward.spec.host))
            .collect();
        if matching.is_empty() {
            self.not_here_if_remote("stop-all")?;
            return Ok(StopOutcome {
                hosts: scope.host().map(str::to_string).into_iter().collect(),
                stopped: Vec::new(),
                elsewhere: Vec::new(),
            });
        }
        self.stop_forwards("stop-all", matching)
    }

    /// The refusal a stop owes the far side when it matched nothing.
    ///
    /// Under `herdr --remote` the records live beside the local agent, on the
    /// other machine, so an empty match over here is not evidence that there is
    /// nothing to stop — it is evidence that this is the wrong machine to ask,
    /// and saying "nothing to stop" would be the confident wrong answer. A host
    /// that cannot be resolved at all leaves the no-op alone: there is then no
    /// single machine to send anyone to.
    fn not_here_if_remote(&self, action: &'static str) -> Result<()> {
        if let Ok(resolved) = self.resolve() {
            if !self.locate_named(&resolved.host).is_local() {
                return Err(ActionError::NotHere {
                    action,
                    host: resolved.host.name,
                });
            }
        }
        Ok(())
    }

    /// Stop exactly the forwards a screen is showing.
    ///
    /// This is what makes `enter` on a row stop *that* row's forward on *that*
    /// row's host, whatever host the invocation resolved to. A row whose record
    /// has changed since the frame was drawn matches nothing and is skipped:
    /// the ports it names are not the ports the record holds any more, and
    /// cancelling what replaced it would stop a forward nobody pointed at.
    pub fn stop_rows(&self, wanted: &[ForwardKey]) -> Result<StopOutcome> {
        let matching: Vec<Forward> = self
            .store
            .list()?
            .into_iter()
            .filter(|forward| wanted.iter().any(|key| key.matches(forward)))
            .collect();
        if matching.is_empty() {
            let mut hosts: Vec<String> = Vec::new();
            for key in wanted {
                if !hosts.contains(&key.host) {
                    hosts.push(key.host.clone());
                }
            }
            return Ok(StopOutcome {
                hosts,
                stopped: Vec::new(),
                elsewhere: Vec::new(),
            });
        }
        self.stop_forwards("stop", matching)
    }

    /// Cancel these records, wherever they live.
    ///
    /// Every host is placement-checked before anything is cancelled: a stop
    /// spanning two hosts that refused halfway would leave the screen and the
    /// records disagreeing about which half it did.
    fn stop_forwards(&self, action: &'static str, matching: Vec<Forward>) -> Result<StopOutcome> {
        let mut groups: Vec<(Host, Vec<Forward>)> = Vec::new();
        for forward in matching {
            match groups
                .iter_mut()
                .find(|(host, _)| host.name == forward.spec.host)
            {
                Some((_, forwards)) => forwards.push(forward),
                None => {
                    let host = self.host_named(&forward.spec.host);
                    groups.push((host, vec![forward]));
                }
            }
        }
        for (host, _) in &groups {
            if !self.locate_named(host).is_local() {
                return Err(ActionError::NotHere {
                    action,
                    host: host.name.clone(),
                });
            }
        }

        let mut hosts: Vec<String> = Vec::new();
        let mut stopped: Vec<Forward> = Vec::new();
        for (host, forwards) in &groups {
            let control_path = self.control_path(host)?;
            for forward in forwards {
                self.ssh
                    .cancel_forward(&host.target, &control_path, &forward.spec)?;
                self.store.remove(&host.name, forward.spec.local_port)?;
                stopped.push(forward.clone());
            }
            hosts.push(host.name.clone());
        }
        Ok(StopOutcome {
            hosts,
            stopped,
            elsewhere: Vec::new(),
        })
    }

    // -- scan ---------------------------------------------------------------

    /// Remote listeners that belong to a Herdr pane, as candidates.
    ///
    /// Read-only on purpose: listing is not consent. Forwarding a candidate
    /// goes back through [`Actions::add`], which is invariant 7's first case.
    pub fn scan(&self) -> Result<ScanReport> {
        let resolved = self.resolve()?;
        let placement = self.locate_resolved(&resolved);
        let host = resolved.host;

        let owned;
        let probe: &dyn HostProbe = match self.probe {
            Some(probe) => probe,
            None if placement.is_local() => {
                owned = SshProbe {
                    ssh: self.ssh,
                    target: &host.target,
                    control_path: self.control_path(&host)?,
                };
                &owned
            }
            None => &HereProbe,
        };

        if placement.is_local() {
            let control_path = self.control_path(&host)?;
            self.ssh.ensure_master(&host.target, &control_path)?;
        }

        let (listeners, tree) = snapshot(probe).map_err(|message| ActionError::Probe {
            host: host.name.clone(),
            message,
        })?;
        let panes = pane_snapshot(self.host_herdr.client(&host)?.as_ref());
        let attributed = discover::attribute(&listeners, &tree, &panes);

        let recorded = self.store.list_host(&host.name)?;
        let workspaces = self.workspace_identifiers();

        let mut unattributed = 0;
        let mut unforwardable = 0;
        let mut candidates = Vec::new();
        for entry in attributed {
            if !entry.listener.is_forwardable() {
                unforwardable += 1;
                continue;
            }
            if entry.pane.is_none() {
                unattributed += 1;
                if self.env.settings.require_pane_attribution {
                    continue;
                }
            }
            let port = entry.listener.port;
            candidates.push(Candidate {
                already_forwarded: recorded.iter().any(|f| f.spec.remote_port == port),
                local_port_free: placement.is_local().then(|| !self.ports.in_use(port)),
                auto_allowed: workspaces
                    .iter()
                    .any(|id| self.env.settings.auto_allows(id, port)),
                listener: entry.listener,
                pane: entry.pane,
            });
        }
        // Attributed first, then by port, so the picker's top entry is the one
        // a pane is responsible for.
        candidates.sort_by_key(|c| (c.pane.is_none(), c.listener.port));

        Ok(ScanReport {
            host: host.name,
            candidates,
            unattributed,
            unforwardable,
            placement,
        })
    }

    /// Every name the allowlist might be written against for this invocation:
    /// the workspace id, its label, and the label with the mirror prefix
    /// stripped, since a user writes the name they see.
    fn workspace_identifiers(&self) -> Vec<String> {
        let mut names = Vec::new();
        if let Some(id) = &self.env.context.workspace_id {
            names.push(id.clone());
        }
        if let Some(label) = &self.env.context.workspace_label {
            names.push(label.clone());
            if let Some(prefix) = context::workspace_label_prefix(label) {
                names.push(label[prefix.len() + 2..].to_string());
            }
        }
        names
    }

    // -- reconcile ----------------------------------------------------------

    /// Bring the record set back in line with what is actually live.
    ///
    /// Invariant 3: a record that no longer describes a live forward is either
    /// restored or removed on this pass, never left to shadow the port.
    ///
    /// Invariant 2 constrains how that lands on disk. Deciding takes ssh round
    /// trips — seconds, with no lock held — and an `add`, an `open-url` or the
    /// local agent can write in that window. So the result is merged into
    /// whatever the file holds at the end rather than replacing it: see
    /// [`merge_reconciled`].
    pub fn reconcile(&self, trigger: ReconcileTrigger) -> Result<ReconcileReport> {
        let mut report = ReconcileReport {
            trigger,
            kept: Vec::new(),
            readded: Vec::new(),
            dropped: Vec::new(),
        };
        let snapshot = self.store.list()?;
        if snapshot.is_empty() {
            return Ok(report);
        }

        let mut by_host: BTreeMap<String, Vec<Forward>> = BTreeMap::new();
        for forward in snapshot.iter().cloned() {
            by_host
                .entry(forward.spec.host.clone())
                .or_default()
                .push(forward);
        }

        let mut keep: Vec<Forward> = Vec::new();
        for (name, forwards) in by_host {
            let Some(host) = self.env.hosts.by_name(&name).cloned() else {
                for forward in forwards {
                    report.dropped.push(Dropped {
                        forward,
                        reason: DropReason::HostGone,
                    });
                }
                continue;
            };
            let prefix_matched = host.prefix.is_some()
                && self.env.context.workspace_prefix() == host.prefix.as_deref();
            let placement = self.locate(&host, prefix_matched);
            let live_panes = self.live_pane_ids(&host);

            if !placement.is_local() {
                // No master here to check against, and the local agent owns the
                // live forwards. Only the record-level pruning applies.
                for forward in forwards {
                    match drop_for_pane(&forward, live_panes.as_ref()) {
                        true => report.dropped.push(Dropped {
                            forward,
                            reason: DropReason::PaneGone,
                        }),
                        false => {
                            report.kept.push(forward.clone());
                            keep.push(forward);
                        }
                    }
                }
                continue;
            }

            let control_path = match self.control_path(&host) {
                Ok(path) => path,
                Err(err) => {
                    let reason = DropReason::Unreachable(err.to_string());
                    for forward in forwards {
                        report.dropped.push(Dropped {
                            forward,
                            reason: reason.clone(),
                        });
                    }
                    continue;
                }
            };

            let mut master = self.ssh.check_master(&host.target, &control_path);
            if matches!(master, Ok(MasterState::Absent)) && trigger == ReconcileTrigger::Startup {
                // Only startup may dial: an event hook fires on every workspace
                // focus, and opening a connection there would be a surprise.
                master = self
                    .ssh
                    .ensure_master(&host.target, &control_path)
                    .and_then(|()| self.ssh.check_master(&host.target, &control_path));
            }
            let running = matches!(master, Ok(MasterState::Running));
            if !running {
                let reason = match master {
                    Err(err) => DropReason::Unreachable(err.to_string()),
                    _ => DropReason::MasterGone,
                };
                for forward in forwards {
                    report.dropped.push(Dropped {
                        forward,
                        reason: reason.clone(),
                    });
                }
                continue;
            }

            for forward in forwards {
                if drop_for_pane(&forward, live_panes.as_ref()) {
                    // Best effort: the forward may already be gone with its
                    // pane, and invariant 4 makes that a no-op anyway.
                    let _ = self
                        .ssh
                        .cancel_forward(&host.target, &control_path, &forward.spec);
                    report.dropped.push(Dropped {
                        forward,
                        reason: DropReason::PaneGone,
                    });
                    continue;
                }
                if self.ports.in_use(forward.spec.local_port) {
                    report.kept.push(forward.clone());
                    keep.push(forward);
                    continue;
                }
                match self
                    .ssh
                    .add_forward(&host.target, &control_path, &forward.spec)
                {
                    Ok(()) => {
                        report.readded.push(forward.clone());
                        keep.push(forward);
                    }
                    Err(err) => report.dropped.push(Dropped {
                        forward,
                        reason: DropReason::ReAddFailed(err.to_string()),
                    }),
                }
            }
        }

        let superseded = self
            .store
            .update(move |current| merge_reconciled(&snapshot, keep, current))?;
        for forward in superseded {
            self.absorb_supersession(forward, &mut report);
        }
        Ok(report)
    }

    /// Fold one record another process removed mid-pass back into the report,
    /// and undo the forward if this pass had just re-added it.
    ///
    /// A re-added forward with no record is exactly what invariant 3 forbids:
    /// nothing would ever find it again. Cancelling is speculative and
    /// invariant 4 makes a cancel of something already gone a no-op.
    fn absorb_supersession(&self, forward: Forward, report: &mut ReconcileReport) {
        let key = record_key(&forward);
        report.kept.retain(|kept| record_key(kept) != key);
        let readded = report.readded.iter().any(|f| record_key(f) == key);
        report.readded.retain(|f| record_key(f) != key);
        if readded {
            if let Some(host) = self.env.hosts.by_name(&forward.spec.host).cloned() {
                if let Ok(control_path) = self.control_path(&host) {
                    let _ = self
                        .ssh
                        .cancel_forward(&host.target, &control_path, &forward.spec);
                }
            }
        }
        report.dropped.push(Dropped {
            forward,
            reason: DropReason::Superseded,
        });
    }

    /// Pane ids on the server that owns `host`'s panes. `None` when the server
    /// could not be reached: not knowing is not the same as the pane being
    /// gone, and dropping a forward on that would be data loss.
    fn live_pane_ids(&self, host: &Host) -> Option<HashSet<String>> {
        let client = self.host_herdr.client(host).ok()?;
        let panes = client.pane_list().ok()?;
        Some(panes.into_iter().map(|pane| pane.pane_id).collect())
    }

    // -- status -------------------------------------------------------------

    /// Hosts, masters and forwards. Total on purpose: `status` is what a user
    /// runs when something is already wrong, so it reports failures instead of
    /// becoming one.
    ///
    /// That covers the record file and the request queue, not only the host
    /// name. A damaged `forwards.json` is one of the things a user runs this
    /// command to find out about, and returning `Err` for it printed nothing at
    /// all: not the path of the file that is broken, not the hosts that are
    /// configured, not the pending requests that were perfectly readable. Each
    /// failure becomes a field, every other field is still gathered, and the
    /// caller turns "something was unreadable" into the same non-zero exit any
    /// other failed command gives.
    pub fn status(&self) -> Result<StatusReport> {
        let (recorded, state_error) = match self.store.list() {
            Ok(recorded) => (Some(recorded), None),
            Err(err) => (None, Some(err.to_string())),
        };
        let (pending, request_error) = match self.requests.pending() {
            Ok(pending) => (Some(pending), None),
            Err(err) => (None, Some(err.to_string())),
        };

        let (selected, selection_error) = match self.resolve() {
            Ok(resolved) => (
                Some(SelectedHost {
                    name: resolved.host.name.clone(),
                    source: source_label(resolved.source),
                    placement: self.locate_resolved(&resolved),
                }),
                None,
            ),
            Err(err) => (None, Some(err.to_string())),
        };

        let selected_is_local = selected
            .as_ref()
            .map(|s| s.placement.is_local())
            .unwrap_or(false);

        let hosts = self
            .env
            .hosts
            .hosts
            .iter()
            .map(|host| HostStatus {
                master_running: selected_is_local
                    .then(|| self.master_is_running(host))
                    .flatten(),
                forwards: recorded.as_ref().map(|recorded| {
                    recorded
                        .iter()
                        .filter(|f| f.spec.host == host.name)
                        .cloned()
                        .collect()
                }),
                host: host.clone(),
            })
            .collect();

        Ok(StatusReport {
            hosts_file: self.env.hosts.source.clone(),
            shadowed: self.env.hosts.shadowed.clone(),
            warnings: self.env.hosts.warnings.clone(),
            state_file: self.store.path().to_path_buf(),
            request_file: self.requests.path().to_path_buf(),
            selected,
            selection_error,
            hosts,
            pending_requests: pending.map(|pending| pending.len()),
            state_error,
            request_error,
        })
    }
}

/// Records `port` names, within `scope`.
///
/// Two passes rather than one filter: `stop 5173` means the remote port a user
/// read in a URL, and only when no host forwards that remote port at all does
/// the number get read as a local one. A single pass would let one host's
/// remapped local port answer for another host's remote port.
fn matching_port(recorded: &[Forward], port: u16, scope: &Scope) -> Vec<Forward> {
    let matches = |pick: fn(&ForwardSpec) -> u16| -> Vec<Forward> {
        recorded
            .iter()
            .filter(|forward| scope.covers(&forward.spec.host) && pick(&forward.spec) == port)
            .cloned()
            .collect()
    };
    let remote = matches(|spec| spec.remote_port);
    if !remote.is_empty() {
        return remote;
    }
    matches(|spec| spec.local_port)
}

/// The hosts these records name, each once, in the order they appear.
fn host_order(forwards: &[Forward]) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for forward in forwards {
        if !hosts.contains(&forward.spec.host) {
            hosts.push(forward.spec.host.clone());
        }
    }
    hosts
}

/// What identifies a record: the pair a cancel has to name.
fn record_key(forward: &Forward) -> (&str, u16) {
    (forward.spec.host.as_str(), forward.spec.local_port)
}

/// Land a reconcile pass on whatever the state file holds now.
///
/// Pure, so the rule can be read without a filesystem. `snapshot` is what the
/// pass read at the start, `keep` what it decided should survive, `current`
/// what the file holds under the exclusive lock — which is not the snapshot if
/// anything else wrote in between.
///
/// The rule: a record still byte-identical to the snapshot is this pass's to
/// decide about, and a record that was added or rewritten since is a newer
/// writer's and is left exactly as found. That is invariant 2 one level above
/// [`Store`]: a `add` racing an event hook cannot lose its record, and a `stop`
/// racing one cannot have its forward resurrected.
///
/// Returns the records the pass wanted to keep that are no longer in the file
/// at all — someone stopped them mid-pass. They are not put back; the caller
/// undoes anything it re-added for them.
fn merge_reconciled(
    snapshot: &[Forward],
    keep: Vec<Forward>,
    current: &mut Vec<Forward>,
) -> Vec<Forward> {
    let find = |records: &[Forward], key: (&str, u16)| -> Option<Forward> {
        records
            .iter()
            .find(|record| record_key(record) == key)
            .cloned()
    };

    let mut merged: Vec<Forward> = Vec::with_capacity(current.len());
    for record in current.iter() {
        let key = record_key(record);
        match find(snapshot, key) {
            Some(ref original) if original == record => {
                if let Some(decided) = find(&keep, key) {
                    merged.push(decided);
                }
            }
            _ => merged.push(record.clone()),
        }
    }

    let superseded: Vec<Forward> = keep
        .into_iter()
        .filter(|decided| find(current, record_key(decided)).is_none())
        .collect();
    *current = merged;
    superseded
}

/// Whether a record should go because its pane is gone. A user-pinned forward
/// outlives its pane: the user asked for the port, not for the pane.
fn drop_for_pane(forward: &Forward, live: Option<&HashSet<String>>) -> bool {
    if forward.spec.origin == Origin::Explicit {
        return false;
    }
    match (live, forward.spec.pane_id.as_deref()) {
        (Some(live), Some(pane_id)) => !live.contains(pane_id),
        _ => false,
    }
}

/// Listeners plus the process tree, from whichever tool the host has.
fn snapshot(probe: &dyn HostProbe) -> std::result::Result<(Vec<Listener>, ProcessTree), String> {
    let listeners = match probe.run(discover::LSOF_LISTENERS_COMMAND) {
        Ok(stdout) => discover::parse_lsof(&stdout),
        Err(lsof_err) => match probe.run(discover::SS_LISTENERS_COMMAND) {
            Ok(stdout) => discover::parse_ss(&stdout),
            Err(ss_err) => {
                return Err(format!("lsof: {lsof_err}; ss: {ss_err}"));
            }
        },
    };
    let tree = discover::parse_ps(&probe.run(discover::PS_TREE_COMMAND)?);
    Ok((listeners, tree))
}

/// Panes with their `shell_pid` filled in, since `pane list` does not carry it.
/// A pane that fails mid-scan keeps its `None` rather than failing the scan:
/// panes close while they are being listed.
fn pane_snapshot(client: &dyn HerdrClient) -> Vec<PaneRef> {
    let Ok(mut panes) = client.pane_list() else {
        return Vec::new();
    };
    for pane in &mut panes {
        if pane.shell_pid.is_none() {
            pane.shell_pid = client.shell_pid(&pane.pane_id).ok().flatten();
        }
    }
    panes
}

/// Convenience for a caller that has one client for every host, which is the
/// `herdr --remote` shape.
pub struct SameHerdr<'a>(pub &'a dyn HerdrClient);

impl HostHerdr for SameHerdr<'_> {
    fn client(&self, _host: &Host) -> Result<Box<dyn HerdrClient + '_>> {
        Ok(Box::new(Borrowed(self.0)))
    }
}

struct Borrowed<'a>(&'a dyn HerdrClient);

impl HerdrClient for Borrowed<'_> {
    fn pane_list(&self) -> crate::herdr::Result<Vec<PaneRef>> {
        self.0.pane_list()
    }
    fn pane_list_labelled(&self) -> crate::herdr::Result<Vec<PaneRef>> {
        self.0.pane_list_labelled()
    }
    fn shell_pid(&self, pane_id: &str) -> crate::herdr::Result<Option<u32>> {
        self.0.shell_pid(pane_id)
    }
    fn notify(&self, notification: &Notification) -> crate::herdr::Result<()> {
        self.0.notify(notification)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::*;
    use crate::config::AutoRule;
    use crate::herdr::HerdrError;
    use crate::ssh::SshError;

    // -- fakes --------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum SshCall {
        EnsureMaster(String),
        CheckMaster(String),
        AddForward(String, String),
        CancelForward(String, String),
        ExitMaster(String),
        Exec(String, String),
    }

    #[derive(Default)]
    struct FakeSsh {
        calls: Mutex<Vec<SshCall>>,
        running: Mutex<HashSet<String>>,
        ensure_fails: bool,
        add_fails: Option<String>,
        check_fails: bool,
        exec: HashMap<String, std::result::Result<String, String>>,
    }

    impl FakeSsh {
        fn new() -> Self {
            Self::default()
        }

        fn with_master(self, target: &str) -> Self {
            self.running
                .lock()
                .expect("lock")
                .insert(target.to_string());
            self
        }

        fn with_exec(mut self, command: &str, stdout: &str) -> Self {
            self.exec
                .insert(command.to_string(), Ok(stdout.to_string()));
            self
        }

        fn with_exec_err(mut self, command: &str, message: &str) -> Self {
            self.exec
                .insert(command.to_string(), Err(message.to_string()));
            self
        }

        fn calls(&self) -> Vec<SshCall> {
            self.calls.lock().expect("lock").clone()
        }

        fn record(&self, call: SshCall) {
            self.calls.lock().expect("lock").push(call);
        }

        fn failure(operation: &'static str, target: &str, stderr: &str) -> SshError {
            SshError::Command {
                operation,
                target: target.to_string(),
                code: 255,
                stderr: stderr.to_string(),
            }
        }
    }

    impl SshRunner for FakeSsh {
        fn ensure_master(&self, target: &str, _control_path: &Path) -> ssh::Result<()> {
            self.record(SshCall::EnsureMaster(target.to_string()));
            if self.ensure_fails {
                return Err(FakeSsh::failure("ensure_master", target, "refused"));
            }
            self.running
                .lock()
                .expect("lock")
                .insert(target.to_string());
            Ok(())
        }

        fn check_master(&self, target: &str, _control_path: &Path) -> ssh::Result<MasterState> {
            self.record(SshCall::CheckMaster(target.to_string()));
            if self.check_fails {
                return Err(FakeSsh::failure("check_master", target, "no ssh here"));
            }
            Ok(if self.running.lock().expect("lock").contains(target) {
                MasterState::Running
            } else {
                MasterState::Absent
            })
        }

        fn add_forward(
            &self,
            target: &str,
            _control_path: &Path,
            spec: &ForwardSpec,
        ) -> ssh::Result<()> {
            self.record(SshCall::AddForward(target.to_string(), spec.forward_arg()));
            match &self.add_fails {
                Some(message) => Err(FakeSsh::failure("add_forward", target, message)),
                None => Ok(()),
            }
        }

        fn cancel_forward(
            &self,
            target: &str,
            _control_path: &Path,
            spec: &ForwardSpec,
        ) -> ssh::Result<()> {
            self.record(SshCall::CancelForward(
                target.to_string(),
                spec.forward_arg(),
            ));
            Ok(())
        }

        fn exit_master(&self, target: &str, _control_path: &Path) -> ssh::Result<()> {
            self.record(SshCall::ExitMaster(target.to_string()));
            self.running.lock().expect("lock").remove(target);
            Ok(())
        }

        fn exec(&self, target: &str, _control_path: &Path, command: &str) -> ssh::Result<String> {
            self.record(SshCall::Exec(target.to_string(), command.to_string()));
            match self.exec.get(command) {
                Some(Ok(stdout)) => Ok(stdout.clone()),
                Some(Err(message)) => Err(FakeSsh::failure("exec", target, message)),
                None => Err(FakeSsh::failure("exec", target, "command not stubbed")),
            }
        }
    }

    #[derive(Default)]
    struct HerdrState {
        panes: Vec<PaneRef>,
        pids: HashMap<String, u32>,
        calls: Mutex<Vec<String>>,
        notifications: Mutex<Vec<(String, String)>>,
        fail_pane_list: bool,
    }

    #[derive(Clone, Default)]
    struct FakeHerdr(Arc<HerdrState>);

    impl FakeHerdr {
        fn with_panes(panes: Vec<PaneRef>) -> Self {
            Self(Arc::new(HerdrState {
                panes,
                ..HerdrState::default()
            }))
        }

        fn with_pids(panes: Vec<PaneRef>, pids: &[(&str, u32)]) -> Self {
            Self(Arc::new(HerdrState {
                panes,
                pids: pids
                    .iter()
                    .map(|(id, pid)| ((*id).to_string(), *pid))
                    .collect(),
                ..HerdrState::default()
            }))
        }

        fn unreachable() -> Self {
            Self(Arc::new(HerdrState {
                fail_pane_list: true,
                ..HerdrState::default()
            }))
        }

        fn calls(&self) -> Vec<String> {
            self.0.calls.lock().expect("lock").clone()
        }

        fn notifications(&self) -> Vec<(String, String)> {
            self.0.notifications.lock().expect("lock").clone()
        }
    }

    impl HerdrClient for FakeHerdr {
        fn pane_list(&self) -> crate::herdr::Result<Vec<PaneRef>> {
            self.0.calls.lock().expect("lock").push("pane_list".into());
            if self.0.fail_pane_list {
                return Err(HerdrError::Command {
                    command: "pane list".into(),
                    code: 1,
                    stderr: "server unreachable".into(),
                });
            }
            // Stripped, because the real client cannot fill this in from
            // `pane list` alone. A fake that handed back labels here would hide
            // any caller reading them off the wrong listing — which is exactly
            // the bug that reached a real machine.
            Ok(self
                .0
                .panes
                .iter()
                .cloned()
                .map(|pane| PaneRef {
                    workspace_label: None,
                    ..pane
                })
                .collect())
        }

        fn pane_list_labelled(&self) -> crate::herdr::Result<Vec<PaneRef>> {
            self.0
                .calls
                .lock()
                .expect("lock")
                .push("pane_list_labelled".into());
            if self.0.fail_pane_list {
                return Err(HerdrError::Command {
                    command: "workspace list".into(),
                    code: 1,
                    stderr: "server unreachable".into(),
                });
            }
            Ok(self.0.panes.clone())
        }

        fn shell_pid(&self, pane_id: &str) -> crate::herdr::Result<Option<u32>> {
            self.0
                .calls
                .lock()
                .expect("lock")
                .push(format!("shell_pid:{pane_id}"));
            Ok(self.0.pids.get(pane_id).copied())
        }

        fn notify(&self, notification: &Notification) -> crate::herdr::Result<()> {
            self.0
                .notifications
                .lock()
                .expect("lock")
                .push((notification.title.clone(), notification.body.clone()));
            Ok(())
        }
    }

    struct OneHerdr(FakeHerdr);

    impl HostHerdr for OneHerdr {
        fn client(&self, _host: &Host) -> Result<Box<dyn HerdrClient + '_>> {
            Ok(Box::new(self.0.clone()))
        }
    }

    #[derive(Default)]
    struct FakePorts {
        used: Mutex<HashSet<u16>>,
        asked: Mutex<Vec<u16>>,
    }

    impl FakePorts {
        fn taken(ports: &[u16]) -> Self {
            Self {
                used: Mutex::new(ports.iter().copied().collect()),
                asked: Mutex::new(Vec::new()),
            }
        }

        fn asked(&self) -> Vec<u16> {
            self.asked.lock().expect("lock").clone()
        }
    }

    impl LocalPorts for FakePorts {
        fn in_use(&self, port: u16) -> bool {
            self.asked.lock().expect("lock").push(port);
            self.used.lock().expect("lock").contains(&port)
        }
    }

    #[derive(Default)]
    struct FakeOpener {
        opened: Mutex<Vec<String>>,
        fails: bool,
    }

    impl FakeOpener {
        fn opened(&self) -> Vec<String> {
            self.opened.lock().expect("lock").clone()
        }
    }

    impl UrlOpener for FakeOpener {
        fn open(&self, url: &str) -> std::result::Result<(), String> {
            self.opened.lock().expect("lock").push(url.to_string());
            if self.fails {
                return Err("no browser here".to_string());
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeProbe {
        out: HashMap<String, std::result::Result<String, String>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeProbe {
        fn with(mut self, command: &str, stdout: &str) -> Self {
            self.out.insert(command.to_string(), Ok(stdout.to_string()));
            self
        }
    }

    impl HostProbe for FakeProbe {
        fn run(&self, command: &str) -> std::result::Result<String, String> {
            self.calls.lock().expect("lock").push(command.to_string());
            match self.out.get(command) {
                Some(result) => result.clone(),
                None => Err("not stubbed".to_string()),
            }
        }
    }

    // -- harness ------------------------------------------------------------

    fn host(name: &str, prefix: Option<&str>) -> Host {
        Host {
            name: name.to_string(),
            target: format!("{name}.example"),
            prefix: prefix
                .map(str::to_string)
                .or_else(|| Some(name.to_string())),
        }
    }

    const NOW: u64 = 1_700_000_000_000;

    fn env(hosts: Vec<Host>) -> Env {
        Env {
            hosts: HostsConfig {
                hosts,
                ..HostsConfig::default()
            },
            settings: Settings::default(),
            context: Invocation::default(),
            explicit_host: None,
            // Short and fixed: a control path over 103 bytes is its own test.
            control_dir: PathBuf::from("/tmp/hpf-test"),
            placement: Some(Placement::Local),
            now_ms: Arc::new(|| NOW),
        }
    }

    struct Harness {
        _dir: TempDir,
        ssh: FakeSsh,
        herdr: FakeHerdr,
        host_herdr: OneHerdr,
        ports: FakePorts,
        opener: FakeOpener,
        store: Store,
        requests: RequestLog,
    }

    impl Harness {
        fn new(ssh: FakeSsh, herdr: FakeHerdr, ports: FakePorts) -> Self {
            let dir = TempDir::new().expect("temp dir");
            let quiet: crate::state::Reporter = Arc::new(|_: &str| {});
            let store = Store::with_reporter(dir.path(), quiet.clone()).expect("store");
            let requests = RequestLog::with_reporter(dir.path(), quiet).expect("request log");
            Self {
                _dir: dir,
                ssh,
                host_herdr: OneHerdr(herdr.clone()),
                herdr,
                ports,
                opener: FakeOpener::default(),
                store,
                requests,
            }
        }

        fn simple() -> Self {
            Self::new(FakeSsh::new(), FakeHerdr::default(), FakePorts::default())
        }

        fn dir(&self) -> &Path {
            self._dir.path()
        }

        fn actions<'a>(&'a self, env: Env) -> Actions<'a> {
            self.actions_with(env, &self.ports)
        }

        /// Same wiring with someone else's port oracle, so a test can hang a
        /// concurrent writer off the probe an action makes mid-pass.
        fn actions_with<'a>(&'a self, env: Env, ports: &'a dyn LocalPorts) -> Actions<'a> {
            Actions {
                ssh: &self.ssh,
                herdr: &self.herdr,
                host_herdr: &self.host_herdr,
                ports,
                opener: &self.opener,
                store: &self.store,
                requests: &self.requests,
                probe: None,
                env,
            }
        }
    }

    /// What a second process does in the middle of a reconcile pass.
    enum Interleaved {
        /// An `add`, an `open-url`, or the local agent recording a forward.
        Upsert(Forward),
        /// A `stop` the user ran while the pass was talking to ssh.
        Remove(String, u16),
    }

    /// A port oracle that writes to the same state directory through a second
    /// `Store` the first time it is consulted.
    ///
    /// Reconcile probes the port between reading the records and writing them
    /// back, which makes this the exact window a real `add` lands in — and a
    /// separate `Store` handle contends for the lock exactly as another process
    /// would.
    struct RacingPorts {
        store: Store,
        pending: Mutex<Vec<Interleaved>>,
        answer: bool,
    }

    impl RacingPorts {
        fn new(dir: &Path, answer: bool, pending: Vec<Interleaved>) -> Self {
            let quiet: crate::state::Reporter = Arc::new(|_: &str| {});
            Self {
                store: Store::with_reporter(dir, quiet).expect("second store handle"),
                pending: Mutex::new(pending),
                answer,
            }
        }
    }

    impl LocalPorts for RacingPorts {
        fn in_use(&self, _port: u16) -> bool {
            if let Some(action) = self.pending.lock().expect("lock").pop() {
                match action {
                    Interleaved::Upsert(forward) => {
                        self.store.upsert(forward).expect("concurrent upsert");
                    }
                    Interleaved::Remove(host, port) => {
                        self.store.remove(&host, port).expect("concurrent remove");
                    }
                }
            }
            self.answer
        }
    }

    fn spec(host: &str, remote: u16, local: u16, origin: Origin) -> ForwardSpec {
        ForwardSpec {
            host: host.to_string(),
            remote_port: remote,
            local_port: local,
            remote_bind: LOOPBACK.to_string(),
            origin,
            pane_id: None,
        }
    }

    fn forward(host: &str, remote: u16, local: u16, origin: Origin, pane: Option<&str>) -> Forward {
        Forward {
            spec: ForwardSpec {
                pane_id: pane.map(str::to_string),
                ..spec(host, remote, local, origin)
            },
            created_unix_ms: NOW,
        }
    }

    fn pane(id: &str, workspace: &str, label: Option<&str>) -> PaneRef {
        PaneRef {
            pane_id: id.to_string(),
            workspace_id: workspace.to_string(),
            workspace_label: label.map(str::to_string),
            agent: None,
            shell_pid: None,
        }
    }

    // -- URL re-validation ---------------------------------------------------

    #[test]
    fn hostile_urls_are_all_rejected() {
        // The link handler is the crate's only untrusted input. Every entry
        // here is something the manifest regex could plausibly let through, or
        // something a later regex edit might.
        let hostile = [
            ("", "empty string"),
            (
                "http://localhost@evil.com:80/",
                "userinfo hides the real host",
            ),
            (
                "http://user:pass@localhost:5173/",
                "userinfo with a password",
            ),
            (
                "http://localhost.evil.com:5173/",
                "host merely starts with localhost",
            ),
            (
                "http://localhostevil.com:5173/",
                "host has no separator at all",
            ),
            ("http://127.0.0.1.evil.com:5173/", "v4 loopback as a label"),
            (
                "http://evil.com/http://localhost:5173",
                "loopback only in the path",
            ),
            ("http://localhost:0/", "port zero"),
            ("http://localhost:05173/", "port with a leading zero"),
            ("http://localhost:65536/", "port one past u16"),
            ("http://localhost:99999/", "five digits, still out of range"),
            ("http://localhost:123456/", "six digits"),
            ("http://localhost/", "no port to forward"),
            ("http://localhost:/", "empty port"),
            ("http://localhost:5173:80/", "two ports"),
            ("http://localhost:+5173/", "signed port"),
            ("http://localhost:0x1f90/", "hex port"),
            ("http://localhost: 5173/", "space inside the authority"),
            ("http://:5173/", "no host"),
            ("file:///etc/passwd", "wrong scheme"),
            ("ftp://localhost:21/", "wrong scheme"),
            ("javascript:alert(1)", "no scheme separator at all"),
            ("//localhost:5173/", "scheme-relative"),
            ("http:/localhost:5173/", "one slash"),
            ("http://[::1]x:5173/", "junk after the bracket"),
            ("http://[::1:5173/", "unterminated bracket"),
            ("http://::1:5173/", "bare IPv6, ambiguous with the port"),
            (
                "http://[::ffff:127.0.0.1]:5173/",
                "v4-mapped v6 is not on the list",
            ),
            (
                "http://[0:0:0:0:0:0:0:1]:5173/",
                "::1 written long is not on the list",
            ),
            ("http://0177.0.0.1:5173/", "octal spelling of 127.0.0.1"),
            ("http://2130706433:5173/", "decimal spelling of 127.0.0.1"),
            (
                "http://127.0.0.2:5173/",
                "loopback subnet but not the allowed host",
            ),
            ("http://①ocalhost:5173/", "non-ASCII homoglyph"),
            ("http://localhost\u{0000}:5173/", "embedded NUL"),
            (
                "http://localhost\\@evil.com:5173/",
                "backslash as a separator",
            ),
            (
                "http://localhost:5173\\@evil.com/",
                "backslash after the port",
            ),
        ];
        for (url, why) in hostile {
            assert!(
                parse_loopback_url(url).is_err(),
                "{url:?} must be rejected ({why})"
            );
        }
    }

    #[test]
    fn loopback_urls_that_should_work_are_accepted() {
        for (url, host, port, rest) in [
            ("http://localhost:5173", "localhost", 5173u16, ""),
            ("http://localhost:5173/", "localhost", 5173, "/"),
            (
                "https://127.0.0.1:8443/app?x=1",
                "127.0.0.1",
                8443,
                "/app?x=1",
            ),
            ("http://[::1]:8080/#frag", "[::1]", 8080, "/#frag"),
            ("HTTP://LOCALHOST:8080/A", "localhost", 8080, "/A"),
            ("http://localhost:1/", "localhost", 1, "/"),
            ("http://localhost:65535/", "localhost", 65535, "/"),
            ("http://localhost:5173/../..", "localhost", 5173, "/../.."),
            (
                "http://localhost:5173#@evil.com",
                "localhost",
                5173,
                "#@evil.com",
            ),
            (
                "http://localhost:5173?to=@evil.com",
                "localhost",
                5173,
                "?to=@evil.com",
            ),
        ] {
            let parsed =
                parse_loopback_url(url).unwrap_or_else(|err| panic!("{url:?} rejected: {err}"));
            assert_eq!(parsed.host, host, "{url:?}");
            assert_eq!(parsed.port, port, "{url:?}");
            assert_eq!(parsed.rest, rest, "{url:?}");
        }
    }

    #[test]
    fn surrounding_whitespace_does_not_change_the_verdict() {
        let parsed = parse_loopback_url("  http://localhost:5173/  ").expect("trimmed");
        assert_eq!(parsed.port, 5173);
    }

    #[test]
    fn a_rewritten_url_keeps_everything_but_the_port() {
        let parsed = parse_loopback_url("http://localhost:5173/app?x=1#f").expect("valid");
        assert_eq!(parsed.with_port(15173), "http://localhost:15173/app?x=1#f");
        assert_eq!(parsed.with_port(5173), "http://localhost:5173/app?x=1#f");
    }

    // -- the local port probe -------------------------------------------------
    //
    // `SystemPorts` is the sole enforcement point for invariant 1, and the one
    // genuinely OS-touching seam every other test injects a fake for. These
    // bind loopback sockets in this process: nothing leaves the machine, no
    // host is reached, and port 0 lets the kernel pick a number so the result
    // does not depend on what else the machine has bound.

    #[test]
    fn a_port_this_process_is_listening_on_reads_as_in_use() {
        let held = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .expect("binding an ephemeral loopback port");
        let port = held.local_addr().expect("the bound address").port();

        assert!(
            SystemPorts.in_use(port),
            "port {port} is held by this test, so forwarding onto it would collide"
        );
    }

    #[test]
    fn a_listener_on_ipv6_loopback_alone_is_still_seen() {
        // A dev server bound only to ::1 is invisible to a v4 probe, and a
        // forward onto that port fails to bind with the record already written.
        let Ok(held) = TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, 0))) else {
            // A machine with IPv6 disabled has nothing to say about this.
            return;
        };
        let port = held.local_addr().expect("the bound address").port();

        assert!(
            SystemPorts.in_use(port),
            "port {port} is held on ::1 only, and the probe missed it"
        );
    }

    #[test]
    fn a_port_nobody_holds_reads_as_free() {
        // Bound to learn a number the kernel considers free, then released. A
        // listener with no connections leaves no TIME_WAIT behind.
        let port = {
            let probe = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .expect("binding an ephemeral loopback port");
            probe.local_addr().expect("the bound address").port()
        };

        assert!(
            !SystemPorts.in_use(port),
            "port {port} was released, so a forward must be allowed onto it"
        );
    }

    #[test]
    fn port_zero_is_never_reported_as_taken() {
        // `bind(0)` means "pick one", so it always succeeds and proves nothing
        // about port 0 itself. Answering from that bind would be an answer to a
        // different question.
        assert!(!SystemPorts.in_use(0));
    }

    #[test]
    fn a_bind_that_failed_for_any_other_reason_does_not_read_as_in_use() {
        // The v6 probe fails with EAFNOSUPPORT on a host without IPv6. Treating
        // any error as "taken" would make `add` refuse every port on such a
        // machine.
        let not_in_use = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(!addr_in_use(&Err(not_in_use)));
        assert!(!addr_in_use(&Err(io::Error::from(
            io::ErrorKind::AddrNotAvailable
        ))));
        assert!(addr_in_use(&Err(io::Error::from(io::ErrorKind::AddrInUse))));
    }

    // -- the desktop handoff --------------------------------------------------

    #[test]
    fn the_url_reaches_the_desktop_as_one_argv_word_and_never_through_a_shell() {
        // Invariant 9, at the crate's only handoff to a program that renders a
        // URL. `parse_loopback_url` validates the scheme, host and port, then
        // copies the path, query and fragment verbatim, so every one of these
        // survives validation and arrives here — as bytes through an argv, as
        // command injection from a Ctrl+click through `sh -c`.
        for hostile in [
            "http://localhost:5173/x;curl evil.example.com/sh|sh",
            "http://localhost:5173/$(id)",
            "http://localhost:5173/`id`",
            "http://localhost:5173/a b&c>d",
            "http://localhost:5173/x'y\"z",
        ] {
            let (program, args) = opener_argv(hostile);
            assert!(
                !["sh", "bash", "zsh", "/bin/sh", "env"].contains(&program),
                "the opener became a shell: {program}"
            );
            assert!(
                !args.iter().any(|arg| arg == "-c"),
                "a command string, not an argv: {args:?}"
            );
            assert_eq!(
                args,
                vec![hostile.to_string()],
                "the whole URL has to be exactly one word"
            );
        }
    }

    #[test]
    fn the_opener_this_platform_uses_is_the_one_that_exists_on_it() {
        let (program, _) = opener_argv("http://localhost:5173/");
        let expected = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        assert_eq!(program, expected);
    }

    #[test]
    fn an_opener_that_failed_is_reported_rather_than_read_as_a_window() {
        use std::os::unix::process::ExitStatusExt;

        assert_eq!(
            opener_outcome("open", std::process::ExitStatus::from_raw(0)),
            Ok(())
        );
        // A wait status of 1 << 8 is exit code 1. An `open` that could not
        // find a browser exits non-zero and opens nothing; reported as success
        // it leaves the user waiting for a window that never appears.
        let failed = opener_outcome("open", std::process::ExitStatus::from_raw(1 << 8));
        let Err(message) = failed else {
            panic!("a non-zero exit must not read as an opened URL");
        };
        assert!(message.contains("open"), "{message}");
    }

    // -- placement -----------------------------------------------------------

    #[test]
    fn only_the_two_spelled_modes_override_placement() {
        assert_eq!(placement_override(Some("local")), Some(Placement::Local));
        assert_eq!(
            placement_override(Some(" REMOTE ")),
            Some(Placement::Remote)
        );
        assert_eq!(placement_override(Some("Local")), Some(Placement::Local));
        assert_eq!(placement_override(None), None);
        assert_eq!(placement_override(Some("")), None);
        assert_eq!(placement_override(Some("yes")), None);
        assert_eq!(placement_override(Some("localhost")), None);
    }

    #[test]
    fn a_mirror_workspace_label_places_us_on_the_users_machine() {
        let harness = Harness::simple();
        let mut env = env(vec![host("buildbox", Some("build"))]);
        env.placement = None;
        env.context.workspace_label = Some("build: scholion".into());
        let actions = harness.actions(env);
        let placed = actions.locate(&host("buildbox", Some("build")), true);
        assert_eq!(placed.placement, Placement::Local);
        assert_eq!(placed.reason, PlacementReason::MirrorWorkspace);
        assert!(
            harness.ssh.calls().is_empty(),
            "the free rule must not spawn ssh"
        );
    }

    #[test]
    fn a_live_control_master_places_us_on_the_users_machine() {
        let harness = Harness::new(
            FakeSsh::new().with_master("buildbox.example"),
            FakeHerdr::default(),
            FakePorts::default(),
        );
        let mut env = env(vec![host("buildbox", None)]);
        env.placement = None;
        let actions = harness.actions(env);
        let placed = actions.locate(&host("buildbox", None), false);
        assert_eq!(placed.placement, Placement::Local);
        assert_eq!(placed.reason, PlacementReason::LiveMaster);
    }

    #[test]
    fn a_mirrored_workspace_anywhere_on_this_server_places_us_locally() {
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::with_panes(vec![pane("w1:p1", "w1", Some("build: scholion"))]),
            FakePorts::default(),
        );
        let mut env = env(vec![host("buildbox", Some("build"))]);
        env.placement = None;
        let actions = harness.actions(env);
        let placed = actions.locate(&host("buildbox", Some("build")), false);
        assert_eq!(placed.placement, Placement::Local);
        assert_eq!(placed.reason, PlacementReason::MirroredPane);
    }

    #[test]
    fn no_evidence_at_all_assumes_the_far_side() {
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::with_panes(vec![pane("w1:p1", "w1", Some("scholion"))]),
            FakePorts::default(),
        );
        let mut env = env(vec![host("buildbox", Some("build"))]);
        env.placement = None;
        let actions = harness.actions(env);
        let placed = actions.locate(&host("buildbox", Some("build")), false);
        assert_eq!(placed.placement, Placement::Remote);
        assert_eq!(placed.reason, PlacementReason::Assumed);
    }

    #[test]
    fn an_unreachable_ssh_is_not_evidence_of_placement() {
        let harness = Harness::new(
            FakeSsh {
                check_fails: true,
                ..FakeSsh::new()
            },
            FakeHerdr::default(),
            FakePorts::default(),
        );
        let mut env = env(vec![host("buildbox", None)]);
        env.placement = None;
        let actions = harness.actions(env);
        assert_eq!(
            actions.locate(&host("buildbox", None), false).placement,
            Placement::Remote
        );
    }

    #[test]
    fn a_forced_mode_beats_every_other_signal() {
        let harness = Harness::new(
            FakeSsh::new().with_master("buildbox.example"),
            FakeHerdr::with_panes(vec![pane("w1:p1", "w1", Some("build: scholion"))]),
            FakePorts::default(),
        );
        let mut env = env(vec![host("buildbox", Some("build"))]);
        env.placement = Some(Placement::Remote);
        env.context.workspace_label = Some("build: scholion".into());
        let actions = harness.actions(env);
        let placed = actions.locate(&host("buildbox", Some("build")), true);
        assert_eq!(placed.placement, Placement::Remote);
        assert_eq!(placed.reason, PlacementReason::Forced);
    }

    // -- add -----------------------------------------------------------------

    #[test]
    fn add_forwards_the_same_port_on_both_ends_and_records_it() {
        let harness = Harness::simple();
        let actions = harness.actions(env(vec![host("buildbox", None)]));
        let outcome = actions.add(Some(5173), None).expect("add");

        assert_eq!(outcome.applied, Applied::Forwarded);
        assert_eq!(outcome.spec.local_port, 5173);
        assert_eq!(outcome.spec.remote_port, 5173);
        assert_eq!(outcome.spec.remote_bind, LOOPBACK);
        assert_eq!(outcome.spec.origin, Origin::Explicit);
        assert_eq!(
            harness.ssh.calls(),
            vec![
                SshCall::EnsureMaster("buildbox.example".into()),
                SshCall::AddForward(
                    "buildbox.example".into(),
                    "localhost:5173:127.0.0.1:5173".into()
                ),
            ]
        );
        let recorded = harness.store.list().expect("list");
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].created_unix_ms, NOW);
    }

    #[test]
    fn add_carries_the_invoking_pane_so_reconcile_can_release_it() {
        let harness = Harness::simple();
        let mut e = env(vec![host("buildbox", None)]);
        e.context.pane_id = Some("w1:p2".into());
        let outcome = harness.actions(e).add(Some(5173), None).expect("add");
        assert_eq!(outcome.spec.pane_id.as_deref(), Some("w1:p2"));
    }

    #[test]
    fn add_without_a_port_asks_for_one_instead_of_guessing() {
        let harness = Harness::simple();
        let err = harness
            .actions(env(vec![host("buildbox", None)]))
            .add(None, None)
            .expect_err("no port");
        assert!(matches!(err, ActionError::NoPort { command: "add" }));
        assert!(harness.ssh.calls().is_empty());
    }

    #[test]
    fn each_command_missing_a_port_suggests_its_own_verb() {
        // A `stop` that suggests `add` reads as a correction, and sends the
        // user to the opposite of what they asked for.
        let harness = Harness::simple();
        let actions = harness.actions(env(vec![host("buildbox", None)]));

        let add = actions.add(None, None).expect_err("no port").to_string();
        assert!(add.contains("herdr-portfwd add 5173"), "{add}");

        let stop = actions.stop(None).expect_err("no port").to_string();
        assert!(stop.contains("herdr-portfwd stop 5173"), "{stop}");
        assert!(
            !stop.contains("add"),
            "the stop hint must not name another command: {stop}"
        );
        assert!(harness.ssh.calls().is_empty(), "nothing dialled either way");
    }

    #[test]
    fn no_action_ever_remaps_a_port_on_its_own() {
        // Invariant: the local port equals the remote port unless the caller
        // named a different one. Sweep the whole port range's edges plus the
        // action that takes a URL, which has no way to ask for a remap at all.
        for port in [1u16, 80, 5173, 8080, 65535] {
            let harness = Harness::simple();
            let outcome = harness
                .actions(env(vec![host("buildbox", None)]))
                .add(Some(port), None)
                .expect("add");
            assert!(
                !outcome.spec.is_remapped(),
                "add({port}) produced a remap on its own"
            );

            let harness = Harness::simple();
            let mut e = env(vec![host("buildbox", None)]);
            e.context.clicked_url = Some(format!("http://localhost:{port}/"));
            let opened = harness.actions(e).open_url(None).expect("open");
            assert!(
                !opened.add.spec.is_remapped(),
                "open_url({port}) produced a remap on its own"
            );
        }
    }

    #[test]
    fn a_remap_happens_only_when_the_caller_names_the_local_port() {
        let harness = Harness::simple();
        let outcome = harness
            .actions(env(vec![host("buildbox", None)]))
            .add(Some(5173), Some(15173))
            .expect("add");
        assert!(outcome.spec.is_remapped());
        assert_eq!(
            harness.ssh.calls().last(),
            Some(&SshCall::AddForward(
                "buildbox.example".into(),
                "localhost:15173:127.0.0.1:5173".into()
            ))
        );
    }

    #[test]
    fn a_local_port_held_by_another_process_is_reported_not_taken() {
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::default(),
            FakePorts::taken(&[5173]),
        );
        let err = harness
            .actions(env(vec![host("buildbox", None)]))
            .add(Some(5173), None)
            .expect_err("conflict");

        match &err {
            ActionError::LocalPortInUse {
                local_port,
                remote_port,
                holder,
                ..
            } => {
                assert_eq!(*local_port, 5173);
                assert_eq!(*remote_port, 5173);
                assert!(holder.contains("another process"));
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        let rendered = err.to_string();
        assert!(rendered.contains("5173"), "both ports must be named");
        assert!(
            rendered.contains("Vite HMR") && rendered.contains("redirect_uri"),
            "the report must say what a remap breaks: {rendered}"
        );
        assert!(
            harness.ssh.calls().is_empty(),
            "nothing may be bound on a conflict"
        );
        assert!(harness.store.list().expect("list").is_empty());
    }

    #[test]
    fn a_local_port_held_by_one_of_our_forwards_names_that_forward() {
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::default(),
            FakePorts::taken(&[5173]),
        );
        harness
            .store
            .upsert(forward("buildbox", 3000, 5173, Origin::Explicit, None))
            .expect("seed");
        let err = harness
            .actions(env(vec![host("buildbox", None)]))
            .add(Some(5173), None)
            .expect_err("conflict");
        assert!(
            err.to_string()
                .contains("the forward of buildbox port 3000"),
            "{err}"
        );
    }

    #[test]
    fn an_explicit_remap_onto_a_busy_port_is_refused_too() {
        // "Explicit" licenses a different number, not a collision: binding an
        // occupied port cannot succeed, so reporting is the only useful answer.
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::default(),
            FakePorts::taken(&[15173]),
        );
        let err = harness
            .actions(env(vec![host("buildbox", None)]))
            .add(Some(5173), Some(15173))
            .expect_err("conflict");
        assert!(matches!(err, ActionError::LocalPortInUse { .. }));
        assert!(harness.ssh.calls().is_empty());
    }

    #[test]
    fn adding_a_forward_that_is_already_live_changes_nothing() {
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::default(),
            FakePorts::taken(&[5173]),
        );
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let outcome = harness
            .actions(env(vec![host("buildbox", None)]))
            .add(Some(5173), None)
            .expect("idempotent");
        assert_eq!(outcome.applied, Applied::AlreadyLive);
        assert!(harness.ssh.calls().is_empty());
    }

    #[test]
    fn adding_a_forward_the_agent_made_for_a_pane_pins_it() {
        // Idempotent is not the same as inert. The agent opened 5173 because a
        // pane asked for it; the user then said `add 5173`, or Ctrl+clicked the
        // URL. There is nothing to open, but the record has to record the
        // intent, or the pin lasts exactly as long as the pane does.
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::default(),
            FakePorts::taken(&[5173]),
        );
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Auto, Some("w1:p1")))
            .expect("seed the agent's own forward");

        let outcome = harness
            .actions(env(vec![host("buildbox", None)]))
            .add(Some(5173), None)
            .expect("idempotent");

        assert_eq!(outcome.applied, Applied::AlreadyLive);
        assert!(harness.ssh.calls().is_empty(), "nothing to open");
        assert_eq!(outcome.spec.origin, Origin::Explicit);
        let recorded = harness
            .store
            .get("buildbox", 5173)
            .expect("read")
            .expect("still one record");
        assert_eq!(recorded.spec.origin, Origin::Explicit);
    }

    #[test]
    fn a_pinned_forward_the_agent_had_opened_survives_the_pane_exiting() {
        // The consequence the upgrade exists for, through the path that killed
        // the port: `pane.exited` fires `reconcile --event`, which cancels every
        // pane-scoped forward whose pane is gone.
        let harness = reconcile_harness(&[5173], vec![]);
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Auto, Some("w1:p9")))
            .expect("seed the agent's own forward");
        let actions = harness.actions(env(vec![host("buildbox", None)]));

        actions.add(Some(5173), None).expect("the user pins it");
        let report = actions
            .reconcile(ReconcileTrigger::Event)
            .expect("the pane exits");

        assert!(
            report.dropped.is_empty(),
            "the user asked for the port, not for the pane: {:?}",
            report.dropped
        );
        assert_eq!(harness.store.list().expect("list").len(), 1);
    }

    #[test]
    fn a_clicked_url_pins_the_forward_it_lands_on() {
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::default(),
            FakePorts::taken(&[5173]),
        );
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Auto, Some("w1:p1")))
            .expect("seed");

        let outcome = harness
            .actions(env(vec![host("buildbox", None)]))
            .open_url(Some("http://localhost:5173/"))
            .expect("open");

        assert_eq!(outcome.add.applied, Applied::AlreadyLive);
        assert_eq!(
            harness
                .store
                .get("buildbox", 5173)
                .expect("read")
                .expect("a record")
                .spec
                .origin,
            Origin::Explicit
        );
    }

    #[test]
    fn a_stale_record_does_not_shadow_a_port_that_is_actually_free() {
        // Invariant 3: the record claims 5173, nothing is listening there, so
        // the record is wrong and the forward is made rather than refused.
        let harness = Harness::simple();
        harness
            .store
            .upsert(forward("buildbox", 3000, 5173, Origin::Explicit, None))
            .expect("seed");
        let outcome = harness
            .actions(env(vec![host("buildbox", None)]))
            .add(Some(5173), None)
            .expect("stale record replaced");
        assert_eq!(outcome.applied, Applied::Forwarded);
        let recorded = harness.store.list().expect("list");
        assert_eq!(recorded.len(), 1, "one record per (host, local port)");
        assert_eq!(recorded[0].spec.remote_port, 5173);
    }

    #[test]
    fn on_the_far_side_add_records_a_request_and_touches_no_socket() {
        let harness = Harness::simple();
        let mut e = env(vec![host("buildbox", None)]);
        e.placement = Some(Placement::Remote);
        let outcome = harness.actions(e).add(Some(5173), None).expect("add");

        assert_eq!(outcome.applied, Applied::Requested);
        assert!(outcome.request_id.is_some());
        assert!(harness.ssh.calls().is_empty(), "no ssh on the far side");
        assert!(
            harness.ports.asked().is_empty(),
            "local ports there mean nothing"
        );
        assert!(harness.store.list().expect("list").is_empty());

        let pending = harness.requests.pending().expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].local_port, 5173);
        assert_eq!(pending[0].remote_port, 5173);
        assert_eq!(pending[0].created_unix_ms, NOW);
    }

    #[test]
    fn add_refuses_when_no_host_can_be_chosen() {
        let harness = Harness::simple();
        let err = harness
            .actions(env(Vec::new()))
            .add(Some(5173), None)
            .expect_err("no hosts");
        assert!(matches!(err, ActionError::Context(_)), "{err}");
    }

    #[test]
    fn add_reports_an_ssh_refusal_without_recording_a_forward() {
        let harness = Harness::new(
            FakeSsh {
                add_fails: Some("cannot listen to port: 5173".into()),
                ..FakeSsh::new()
            },
            FakeHerdr::default(),
            FakePorts::default(),
        );
        let err = harness
            .actions(env(vec![host("buildbox", None)]))
            .add(Some(5173), None)
            .expect_err("ssh refused");
        assert!(matches!(err, ActionError::Ssh(_)), "{err}");
        assert!(
            harness.store.list().expect("list").is_empty(),
            "a failed forward must not leave a record"
        );
    }

    #[test]
    fn a_control_path_over_the_limit_is_refused_before_ssh_runs() {
        let harness = Harness::simple();
        let mut e = env(vec![host("buildbox", None)]);
        e.control_dir = PathBuf::from("/tmp").join("x".repeat(ssh::MAX_CONTROL_PATH_LEN));
        let err = harness
            .actions(e)
            .add(Some(5173), None)
            .expect_err("too long");
        assert!(
            matches!(err, ActionError::Ssh(SshError::ControlPathTooLong { .. })),
            "{err}"
        );
        assert!(harness.ssh.calls().is_empty());
    }

    // -- open-url ------------------------------------------------------------

    #[test]
    fn open_url_forwards_the_clicked_port_then_opens_it_here() {
        let harness = Harness::simple();
        let mut e = env(vec![host("buildbox", None)]);
        e.context.clicked_url = Some("http://localhost:5173/app".into());
        let outcome = harness.actions(e).open_url(None).expect("open");

        assert_eq!(outcome.add.applied, Applied::Forwarded);
        assert_eq!(outcome.url, "http://localhost:5173/app");
        assert!(outcome.opened);
        assert_eq!(harness.opener.opened(), vec!["http://localhost:5173/app"]);
    }

    #[test]
    fn an_argument_url_wins_over_the_clicked_one() {
        let harness = Harness::simple();
        let mut e = env(vec![host("buildbox", None)]);
        e.context.clicked_url = Some("http://localhost:1111/".into());
        let outcome = harness
            .actions(e)
            .open_url(Some("http://127.0.0.1:2222/"))
            .expect("open");
        assert_eq!(outcome.add.spec.remote_port, 2222);
        assert_eq!(harness.opener.opened(), vec!["http://127.0.0.1:2222/"]);
    }

    #[test]
    fn open_url_with_nothing_to_open_says_so() {
        let harness = Harness::simple();
        let err = harness
            .actions(env(vec![host("buildbox", None)]))
            .open_url(None)
            .expect_err("no url");
        assert!(matches!(err, ActionError::NoUrl));
    }

    #[test]
    fn a_hostile_url_is_rejected_before_a_host_is_even_resolved() {
        let harness = Harness::simple();
        let err = harness
            .actions(env(Vec::new()))
            .open_url(Some("http://localhost@evil.com:80/"))
            .expect_err("hostile");
        match err {
            ActionError::InvalidUrl { url, reason } => {
                assert_eq!(url, "http://localhost@evil.com:80/");
                assert!(reason.contains("userinfo"), "{reason}");
            }
            other => panic!("expected a URL rejection, got {other:?}"),
        }
        assert!(harness.ssh.calls().is_empty());
    }

    #[test]
    fn open_url_opens_the_port_the_forward_actually_bound() {
        // The record already remaps 5173 to 15173, so the browser has to be
        // sent to 15173 even though the clicked text says 5173.
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::default(),
            FakePorts::taken(&[5173]),
        );
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let outcome = harness
            .actions(env(vec![host("buildbox", None)]))
            .open_url(Some("http://localhost:5173/x"))
            .expect("open");
        assert_eq!(outcome.add.applied, Applied::AlreadyLive);
        assert_eq!(outcome.url, "http://localhost:5173/x");
    }

    #[test]
    fn on_the_far_side_open_url_asks_for_the_forward_and_notifies_instead() {
        let harness = Harness::simple();
        let mut e = env(vec![host("buildbox", None)]);
        e.placement = Some(Placement::Remote);
        let outcome = harness
            .actions(e)
            .open_url(Some("http://localhost:5173/"))
            .expect("open");

        assert_eq!(outcome.add.applied, Applied::Requested);
        assert!(!outcome.opened);
        assert!(
            harness.opener.opened().is_empty(),
            "the browser is on the other machine"
        );
        let notes = harness.herdr.notifications();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].1.contains("http://localhost:5173/"),
            "{:?}",
            notes[0]
        );

        // And the promise that notification makes — that the URL opens once the
        // local agent applies the forward — is carried in the request, because
        // the agent is the half of this tool on the machine with the browser.
        // Without it the notification was simply false: nothing ever opened.
        let pending = harness.requests.pending().expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].open_url.as_deref(),
            Some("http://localhost:5173/"),
            "the local agent was given nothing to open"
        );
    }

    #[test]
    fn a_request_carries_the_url_re_rendered_rather_than_the_click_text() {
        // What travels is built from the validated parts, at the port the
        // request asks for, so the applier's own re-validation is checking a
        // string this side constructed rather than one it merely copied.
        let harness = Harness::simple();
        let mut e = env(vec![host("buildbox", None)]);
        e.placement = Some(Placement::Remote);
        harness
            .actions(e)
            .open_url(Some("  HTTP://LocalHost:5173/app?x=1#f  "))
            .expect("open");

        let pending = harness.requests.pending().expect("pending");
        assert_eq!(
            pending[0].open_url.as_deref(),
            Some("http://localhost:5173/app?x=1#f")
        );
    }

    #[test]
    fn an_add_that_nobody_clicked_asks_for_no_browser() {
        // Only a clicked URL may put a window in front of the user. `add` is a
        // forward and nothing else.
        let harness = Harness::simple();
        let mut e = env(vec![host("buildbox", None)]);
        e.placement = Some(Placement::Remote);
        harness.actions(e).add(Some(5173), None).expect("add");

        let pending = harness.requests.pending().expect("pending");
        assert_eq!(pending[0].open_url, None);
    }

    #[test]
    fn a_browser_that_will_not_start_is_reported_after_the_forward_is_made() {
        let mut harness = Harness::simple();
        harness.opener.fails = true;
        let err = harness
            .actions(env(vec![host("buildbox", None)]))
            .open_url(Some("http://localhost:5173/"))
            .expect_err("opener failed");
        assert!(matches!(err, ActionError::Open { .. }), "{err}");
        assert_eq!(
            harness.store.list().expect("list").len(),
            1,
            "the forward itself succeeded and stays recorded"
        );
    }

    // -- the confirmation a click lands on -----------------------------------

    #[test]
    fn a_preview_describes_the_click_and_changes_nothing() {
        // The screen is drawn before anything happens, so building it must not
        // open a socket, write a record, or start a browser.
        let harness = Harness::simple();
        let mut e = env(vec![host("buildbox", None)]);
        e.context.clicked_url = Some("  HTTP://LocalHost:5173/app?x=1  ".into());
        e.context.workspace_label = Some("mini: scholion".into());
        e.context.agent = Some("claude".into());
        e.context.pane_id = Some("w1:p1".into());
        let state = harness.actions(e).preview_open_url(None).expect("preview");

        // Re-rendered from the validated parts, exactly as a request's URL is.
        assert_eq!(state.url, "http://localhost:5173/app?x=1");
        assert_eq!(state.remote_port, 5173);
        assert_eq!(state.local_port, 5173, "a click never remaps");
        assert_eq!(state.host.as_deref(), Some("buildbox"));
        assert_eq!(state.local_free, Some(true));
        assert_eq!(state.already_forwarded, Some(false));
        assert!(state.applies_here);
        assert_eq!(state.workspace.as_deref(), Some("mini: scholion"));
        assert_eq!(state.agent.as_deref(), Some("claude"));
        assert_eq!(state.pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(state.problem, None);

        assert!(harness.ssh.calls().is_empty(), "a preview forwards nothing");
        assert!(harness.store.list().expect("list").is_empty());
        assert!(harness.requests.pending().expect("pending").is_empty());
        assert!(harness.opener.opened().is_empty());
    }

    #[test]
    fn a_preview_of_a_port_already_forwarded_says_so() {
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::default(),
            FakePorts::taken(&[5173]),
        );
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let state = harness
            .actions(env(vec![host("buildbox", None)]))
            .preview_open_url(Some("http://localhost:5173/"))
            .expect("preview");
        assert_eq!(state.already_forwarded, Some(true));
        // The port reads as taken because our own forward holds it, which the
        // screen must not render as a collision.
        assert_eq!(state.local_free, Some(false));
        assert!(!state.port_taken());
    }

    #[test]
    fn a_preview_of_a_port_something_else_holds_reports_the_conflict() {
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::default(),
            FakePorts::taken(&[5173]),
        );
        let state = harness
            .actions(env(vec![host("buildbox", None)]))
            .preview_open_url(Some("http://localhost:5173/"))
            .expect("preview");
        assert_eq!(state.already_forwarded, Some(false));
        assert!(state.port_taken(), "invariant 1 has to be visible up front");
    }

    #[test]
    fn a_preview_on_the_far_side_answers_none_rather_than_guessing() {
        let harness = Harness::simple();
        // A record on *this* side, which under `herdr --remote` is the host and
        // not the machine the forward would be made on. Whatever it says is an
        // answer to a different question.
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let mut e = env(vec![host("buildbox", None)]);
        e.placement = Some(Placement::Remote);
        let state = harness
            .actions(e)
            .preview_open_url(Some("http://localhost:5173/"))
            .expect("preview");
        assert!(!state.applies_here);
        assert_eq!(
            state.local_free, None,
            "this side cannot know what the user's machine has bound"
        );
        assert_eq!(
            state.already_forwarded, None,
            "nor which forwards it already holds; the records that would answer \
             live beside the local agent, not here"
        );
        assert!(harness.ports.asked().is_empty());
    }

    #[test]
    fn a_click_for_a_host_that_cannot_be_resolved_is_a_screen_not_a_failure() {
        // The command runs headless with its output going to a plugin log, so
        // failing here means the user Ctrl+clicked and saw nothing at all. The
        // reason belongs on the screen the click was already opening.
        let harness = Harness::simple();
        let state = harness
            .actions(env(Vec::new()))
            .preview_open_url(Some("http://localhost:5173/"))
            .expect("an unresolvable host is a question, not an error");
        assert_eq!(state.host, None);
        assert!(!state.actionable());
        let problem = state.problem.expect("the reason has to be carried");
        assert!(problem.contains("no hosts are configured"), "{problem}");
    }

    #[test]
    fn a_click_this_tool_will_never_act_on_is_still_refused_outright() {
        // Nothing to confirm about a URL that can never be forwarded, and the
        // manifest pattern is not the only thing that has to reject it.
        let harness = Harness::simple();
        let err = harness
            .actions(env(vec![host("buildbox", None)]))
            .preview_open_url(Some("http://localhost@evil.com:80/"))
            .expect_err("hostile");
        assert!(matches!(err, ActionError::InvalidUrl { .. }), "{err}");
    }

    #[test]
    fn forward_only_makes_the_forward_and_opens_nothing() {
        let harness = Harness::simple();
        let outcome = harness
            .actions(env(vec![host("buildbox", None)]))
            .open_url_for("http://localhost:5173/app", Some("w1:p1".into()), false)
            .expect("forward only");
        assert_eq!(outcome.add.applied, Applied::Forwarded);
        assert!(!outcome.opened);
        assert!(
            harness.opener.opened().is_empty(),
            "`f` is the key that opens nothing"
        );
        // The forward itself is exactly the one `enter` would have made.
        let recorded = harness.store.list().expect("list");
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].spec.local_port, 5173);
        assert_eq!(recorded[0].spec.pane_id.as_deref(), Some("w1:p1"));
    }

    #[test]
    fn forward_only_on_the_far_side_hands_the_agent_no_url_to_open() {
        // Under `herdr --remote` the browser belongs to the agent, so "forward
        // only" survives the trip only by leaving the URL out of the record.
        // Carrying it and hoping the agent ignores it would open a window the
        // user pressed a key specifically to avoid.
        let harness = Harness::simple();
        let mut e = env(vec![host("buildbox", None)]);
        e.placement = Some(Placement::Remote);
        let outcome = harness
            .actions(e)
            .open_url_for("http://localhost:5173/", None, false)
            .expect("request");
        assert_eq!(outcome.add.applied, Applied::Requested);
        let pending = harness.requests.pending().expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].open_url, None);
        assert!(
            harness.herdr.notifications().is_empty(),
            "there is no page to promise"
        );
    }

    #[test]
    fn a_confirmed_click_is_attributed_to_the_pane_that_printed_the_url() {
        // The confirm pane runs in a popup Herdr launched without a pane
        // identity, and its context names whatever pane is focused when the
        // popup opens. Deriving attribution there would hand the forward to the
        // wrong pane, and `reconcile` releases a forward when its pane exits.
        let harness = Harness::simple();
        let mut e = env(vec![host("buildbox", None)]);
        e.context.pane_id = Some("the-popups-idea-of-focus".into());
        harness
            .actions(e)
            .open_url_for("http://localhost:5173/", Some("w1:clicked".into()), true)
            .expect("open");
        let recorded = harness.store.list().expect("list");
        assert_eq!(recorded[0].spec.pane_id.as_deref(), Some("w1:clicked"));
    }

    #[test]
    fn a_url_that_reached_the_pane_through_the_environment_is_validated_again() {
        // Invariant 11 at the third hop: under `--remote` this string was
        // written on the host, put in an environment variable there, and handed
        // back. The pane trusts none of that.
        let harness = Harness::simple();
        let err = harness
            .actions(env(vec![host("buildbox", None)]))
            .open_url_for("http://localhost@evil.com:80/", None, true)
            .expect_err("hostile");
        assert!(matches!(err, ActionError::InvalidUrl { .. }), "{err}");
        assert!(harness.ssh.calls().is_empty());
        assert!(harness.opener.opened().is_empty());
    }

    // -- stop / stop-all -----------------------------------------------------

    #[test]
    fn stop_cancels_with_the_recorded_spec_and_drops_the_record() {
        let harness = Harness::simple();
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let outcome = harness
            .actions(env(vec![host("buildbox", None)]))
            .stop(Some(5173))
            .expect("stop");

        assert_eq!(outcome.stopped.len(), 1);
        assert_eq!(
            harness.ssh.calls(),
            vec![SshCall::CancelForward(
                "buildbox.example".into(),
                "localhost:5173:127.0.0.1:5173".into()
            )]
        );
        assert!(harness.store.list().expect("list").is_empty());
    }

    #[test]
    fn stopping_a_forward_that_was_never_created_is_a_no_op() {
        // Invariant 4, at the action level: reconcile and a mistyped port must
        // both come back quietly.
        let harness = Harness::simple();
        let outcome = harness
            .actions(env(vec![host("buildbox", None)]))
            .stop(Some(9999))
            .expect("no-op, not an error");
        assert!(outcome.stopped.is_empty());
        assert!(harness.ssh.calls().is_empty());
    }

    #[test]
    fn stop_also_matches_the_local_port_a_remap_created() {
        let harness = Harness::simple();
        harness
            .store
            .upsert(forward("buildbox", 5173, 15173, Origin::Explicit, None))
            .expect("seed");
        let outcome = harness
            .actions(env(vec![host("buildbox", None)]))
            .stop(Some(15173))
            .expect("stop");
        assert_eq!(outcome.stopped[0].spec.remote_port, 5173);
    }

    #[test]
    fn stop_prefers_the_remote_port_when_both_could_match() {
        let harness = Harness::simple();
        // 8080 is one forward's remote port and another's local port.
        harness
            .store
            .upsert(forward("buildbox", 8080, 8080, Origin::Explicit, None))
            .expect("seed");
        harness
            .store
            .upsert(forward("buildbox", 3000, 3000, Origin::Explicit, None))
            .expect("seed");
        let outcome = harness
            .actions(env(vec![host("buildbox", None)]))
            .stop(Some(8080))
            .expect("stop");
        assert_eq!(outcome.stopped.len(), 1);
        assert_eq!(outcome.stopped[0].spec.remote_port, 8080);
    }

    #[test]
    fn stop_touches_only_the_selected_host() {
        let harness = Harness::simple();
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        harness
            .store
            .upsert(forward("other", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let mut e = env(vec![host("buildbox", None), host("other", None)]);
        e.explicit_host = Some("buildbox".into());
        harness.actions(e).stop(Some(5173)).expect("stop");

        let left = harness.store.list().expect("list");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].spec.host, "other");
    }

    #[test]
    fn stop_reaches_a_port_on_a_host_the_invocation_did_not_resolve_to() {
        // The other half of the bug: `stop 5178` narrowed to the host the
        // invocation resolved to, found nothing, and said "nothing to stop on
        // mac-mini-honkoma" while the port stayed up on the other machine.
        let harness = Harness::simple();
        harness
            .store
            .upsert(forward("mini", 8788, 8788, Origin::Explicit, None))
            .expect("seed");
        harness
            .store
            .upsert(forward("studio", 5178, 5178, Origin::Explicit, None))
            .expect("seed");

        let mut e = env(vec![host("mini", None), host("studio", None)]);
        e.hosts.default_host = Some("mini".into());
        let outcome = harness.actions(e).stop(Some(5178)).expect("stop");

        assert_eq!(outcome.stopped.len(), 1);
        assert_eq!(outcome.stopped[0].spec.host, "studio");
        assert_eq!(outcome.hosts, ["studio"]);
        assert_eq!(
            harness.ssh.calls(),
            vec![SshCall::CancelForward(
                "studio.example".into(),
                "localhost:5178:127.0.0.1:5178".into()
            )]
        );
        let left = harness.store.list().expect("list");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].spec.host, "mini");
    }

    #[test]
    fn a_port_forwarded_on_two_hosts_is_named_rather_than_guessed_at() {
        let harness = Harness::simple();
        for name in ["mini", "studio"] {
            harness
                .store
                .upsert(forward(name, 5173, 5173, Origin::Explicit, None))
                .expect("seed");
        }
        let mut e = env(vec![host("mini", None), host("studio", None)]);
        e.hosts.default_host = Some("mini".into());
        let err = harness
            .actions(e)
            .stop(Some(5173))
            .expect_err("two hosts, one port");
        match err {
            ActionError::PortOnSeveralHosts { port, hosts, .. } => {
                assert_eq!(port, 5173);
                assert!(
                    hosts.contains("mini") && hosts.contains("studio"),
                    "{hosts}"
                );
            }
            other => panic!("expected PortOnSeveralHosts, got {other:?}"),
        }
        assert!(
            harness.ssh.calls().is_empty(),
            "a refusal must cancel nothing"
        );
        assert_eq!(harness.store.list().expect("list").len(), 2);
    }

    #[test]
    fn a_narrowed_stop_that_finds_nothing_names_the_host_that_does_have_the_port() {
        let harness = Harness::simple();
        harness
            .store
            .upsert(forward("studio", 5178, 5178, Origin::Explicit, None))
            .expect("seed");
        let mut e = env(vec![host("mini", None), host("studio", None)]);
        e.explicit_host = Some("mini".into());
        let outcome = harness.actions(e).stop(Some(5178)).expect("no-op");

        assert!(outcome.stopped.is_empty());
        assert_eq!(outcome.hosts, ["mini"]);
        assert_eq!(
            outcome.elsewhere,
            ["studio"],
            "a stop that says nothing about where the port actually is sends \
             the user looking for a bug in ssh"
        );
    }

    #[test]
    fn stop_rows_cancels_exactly_the_rows_a_screen_was_showing() {
        let harness = Harness::simple();
        harness
            .store
            .upsert(forward("mini", 8788, 8788, Origin::Explicit, None))
            .expect("seed");
        harness
            .store
            .upsert(forward("studio", 5178, 5178, Origin::Explicit, None))
            .expect("seed");
        harness
            .store
            .upsert(forward("studio", 3000, 3000, Origin::Offered, None))
            .expect("seed");

        let mut e = env(vec![host("mini", None), host("studio", None)]);
        e.hosts.default_host = Some("mini".into());
        let outcome = harness
            .actions(e)
            .stop_rows(&[
                ForwardKey {
                    host: "studio".into(),
                    remote_port: 5178,
                    local_port: 5178,
                },
                ForwardKey {
                    host: "mini".into(),
                    remote_port: 8788,
                    local_port: 8788,
                },
            ])
            .expect("stop rows");

        assert_eq!(outcome.stopped.len(), 2);
        let left = harness.store.list().expect("list");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].spec.remote_port, 3000);
    }

    #[test]
    fn a_row_whose_record_changed_under_it_stops_nothing() {
        // The frame said 5173 -> 5173; by the time the key was pressed the
        // record on that local port belonged to a different remote port.
        // Cancelling it would stop a forward nobody pointed at.
        let harness = Harness::simple();
        harness
            .store
            .upsert(forward("mini", 9999, 5173, Origin::Explicit, None))
            .expect("seed");
        let outcome = harness
            .actions(env(vec![host("mini", None)]))
            .stop_rows(&[ForwardKey {
                host: "mini".into(),
                remote_port: 5173,
                local_port: 5173,
            }])
            .expect("no-op");
        assert!(outcome.stopped.is_empty());
        assert_eq!(outcome.hosts, ["mini"]);
        assert!(harness.ssh.calls().is_empty());
        assert_eq!(harness.store.list().expect("list").len(), 1);
    }

    #[test]
    fn stop_all_cancels_every_forward_for_the_host_and_leaves_the_master_up() {
        let harness = Harness::new(
            FakeSsh::new().with_master("buildbox.example"),
            FakeHerdr::default(),
            FakePorts::default(),
        );
        for port in [5173u16, 8787] {
            harness
                .store
                .upsert(forward("buildbox", port, port, Origin::Explicit, None))
                .expect("seed");
        }
        harness
            .store
            .upsert(forward("other", 3000, 3000, Origin::Offered, None))
            .expect("seed");

        let mut e = env(vec![host("buildbox", None), host("other", None)]);
        e.explicit_host = Some("buildbox".into());
        let outcome = harness.actions(e).stop_all().expect("stop-all");

        assert_eq!(outcome.stopped.len(), 2);
        let cancels: Vec<_> = harness
            .ssh
            .calls()
            .into_iter()
            .filter(|call| matches!(call, SshCall::CancelForward(..)))
            .collect();
        assert_eq!(cancels.len(), 2);
        assert!(
            !harness
                .ssh
                .calls()
                .iter()
                .any(|call| matches!(call, SshCall::ExitMaster(_))),
            "the master is shared; stop-all must not tear it down"
        );
        let left = harness.store.list().expect("list");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].spec.host, "other");
    }

    #[test]
    fn stop_all_with_no_host_named_covers_every_host() {
        // `stop-all` stops what `list` shows, and `list` shows every host. The
        // manifest description says so too.
        let harness = Harness::simple();
        harness
            .store
            .upsert(forward("mini", 8788, 8788, Origin::Explicit, None))
            .expect("seed");
        harness
            .store
            .upsert(forward("studio", 5178, 5178, Origin::Explicit, None))
            .expect("seed");

        let mut e = env(vec![host("mini", None), host("studio", None)]);
        e.hosts.default_host = Some("mini".into());
        let outcome = harness.actions(e).stop_all().expect("stop-all");

        assert_eq!(outcome.stopped.len(), 2);
        assert_eq!(outcome.hosts, ["mini", "studio"]);
        assert!(harness.store.list().expect("list").is_empty());
    }

    #[test]
    fn stop_all_with_nothing_recorded_is_a_no_op() {
        let harness = Harness::simple();
        let outcome = harness
            .actions(env(vec![host("buildbox", None)]))
            .stop_all()
            .expect("stop-all");
        assert!(outcome.stopped.is_empty());
        assert!(harness.ssh.calls().is_empty());
    }

    #[test]
    fn stopping_from_the_far_side_says_where_to_run_it_instead() {
        for oneshot in [true, false] {
            let harness = Harness::simple();
            let mut e = env(vec![host("buildbox", None)]);
            e.placement = Some(Placement::Remote);
            let actions = harness.actions(e);
            let err = if oneshot {
                actions.stop(Some(5173)).expect_err("far side")
            } else {
                actions.stop_all().expect_err("far side")
            };
            match err {
                ActionError::NotHere { host, .. } => assert_eq!(host, "buildbox"),
                other => panic!("expected NotHere, got {other:?}"),
            }
            assert!(harness.ssh.calls().is_empty());
        }
    }

    // -- list ----------------------------------------------------------------

    #[test]
    fn list_marks_a_record_whose_port_is_not_bound_as_missing() {
        let harness = Harness::new(
            FakeSsh::new().with_master("buildbox.example"),
            FakeHerdr::default(),
            FakePorts::taken(&[5173]),
        );
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        harness
            .store
            .upsert(forward("buildbox", 8787, 8787, Origin::Offered, None))
            .expect("seed");

        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .list()
            .expect("list");

        assert_eq!(report.hosts.len(), 1);
        assert_eq!(report.hosts[0].master_running, Some(true));
        let by_port: HashMap<u16, Liveness> = report.hosts[0]
            .forwards
            .iter()
            .map(|f| (f.forward.spec.local_port, f.live))
            .collect();
        assert_eq!(by_port[&5173], Liveness::Live);
        assert_eq!(by_port[&8787], Liveness::Missing);
    }

    #[test]
    fn list_shows_every_host_that_has_a_forward_not_only_the_resolved_one() {
        // The bug, exactly as a user met it: two hosts, a forward on each, and
        // an invocation that resolves to one of them. Narrowing the listing to
        // that one made the other forward invisible — and `stop` narrowed the
        // same way, so it could not be stopped either.
        let harness = Harness::new(
            FakeSsh::new().with_master("mini.example"),
            FakeHerdr::default(),
            FakePorts::taken(&[8788, 5178]),
        );
        harness
            .store
            .upsert(forward("mini", 8788, 8788, Origin::Explicit, None))
            .expect("seed");
        harness
            .store
            .upsert(forward("studio", 5178, 5178, Origin::Explicit, None))
            .expect("seed");

        let mut e = env(vec![host("mini", None), host("studio", None)]);
        e.hosts.default_host = Some("mini".into());
        let report = harness.actions(e).list().expect("list");

        assert_eq!(report.scope, Scope::AllHosts);
        assert_eq!(report.total(), 2, "both forwards are live: {report:?}");
        assert_eq!(
            report
                .hosts
                .iter()
                .map(|section| section.host.as_str())
                .collect::<Vec<_>>(),
            ["mini", "studio"]
        );
    }

    #[test]
    fn list_narrows_to_one_host_only_when_it_was_asked_to() {
        let harness = Harness::simple();
        harness
            .store
            .upsert(forward("mini", 8788, 8788, Origin::Explicit, None))
            .expect("seed");
        harness
            .store
            .upsert(forward("studio", 5178, 5178, Origin::Explicit, None))
            .expect("seed");

        let mut e = env(vec![host("mini", None), host("studio", None)]);
        e.explicit_host = Some("studio".into());
        let report = harness.actions(e).list().expect("list");

        assert_eq!(report.scope, Scope::Host("studio".into()));
        assert_eq!(report.total(), 1);
        assert_eq!(report.hosts[0].host, "studio");
    }

    #[test]
    fn list_answers_across_hosts_even_when_no_single_one_can_be_resolved() {
        // Two configured hosts and nothing pointing at either is
        // `AmbiguousHost` for anything that has to *choose* one. Listing does
        // not have to choose, and refusing to show the forwards that are up is
        // the least useful moment to insist on it.
        let harness = Harness::simple();
        harness
            .store
            .upsert(forward("mini", 8788, 8788, Origin::Explicit, None))
            .expect("seed");

        let report = harness
            .actions(env(vec![host("mini", None), host("studio", None)]))
            .list()
            .expect("a listing needs no single host");
        assert_eq!(report.total(), 1);
    }

    #[test]
    fn a_record_whose_host_left_the_config_is_still_listed_rather_than_hidden() {
        // It is an open port until `reconcile` reaches it, and the listing is
        // the one screen that could do something about it.
        let harness = Harness::simple();
        harness
            .store
            .upsert(forward("retired", 9000, 9000, Origin::Explicit, None))
            .expect("seed");

        let report = harness
            .actions(env(vec![host("mini", None)]))
            .list()
            .expect("list");
        assert_eq!(report.total(), 1);
        assert_eq!(report.hosts[0].host, "retired");
    }

    #[test]
    fn on_the_far_side_list_shows_pending_requests_and_claims_no_liveness() {
        let harness = Harness::simple();
        harness
            .requests
            .enqueue(spec("buildbox", 5173, 5173, Origin::Explicit), NOW)
            .expect("enqueue");
        harness
            .requests
            .enqueue(spec("other", 3000, 3000, Origin::Explicit), NOW)
            .expect("enqueue");
        harness
            .store
            .upsert(forward("buildbox", 8787, 8787, Origin::Explicit, None))
            .expect("seed");

        let mut e = env(vec![host("buildbox", None), host("other", None)]);
        e.placement = Some(Placement::Remote);
        e.explicit_host = Some("buildbox".into());
        let report = harness.actions(e).list().expect("list");

        assert_eq!(report.hosts.len(), 1, "--host narrowed it");
        let section = &report.hosts[0];
        assert_eq!(section.master_running, None);
        assert_eq!(section.pending.len(), 1, "only this host's requests");
        assert_eq!(section.pending[0].remote_port, 5173);
        assert_eq!(section.forwards[0].live, Liveness::Unknown);
    }

    // -- scan ----------------------------------------------------------------

    const LSOF_FIELDS: &str = "p1234\ncnode\nn127.0.0.1:5173\np4321\ncsshd\nn*:22\n";
    const PS_PAIRS: &str = "1234 900\n4321 1\n900 1\n";

    fn scan_ssh() -> FakeSsh {
        FakeSsh::new()
            .with_master("buildbox.example")
            .with_exec(discover::LSOF_LISTENERS_COMMAND, LSOF_FIELDS)
            .with_exec(discover::PS_TREE_COMMAND, PS_PAIRS)
    }

    #[test]
    fn scan_offers_only_listeners_that_reach_a_pane() {
        let harness = Harness::new(
            scan_ssh(),
            FakeHerdr::with_pids(
                vec![pane("w1:p1", "w1", Some("build: scholion"))],
                &[("w1:p1", 900)],
            ),
            FakePorts::default(),
        );
        let report = harness
            .actions(env(vec![host("buildbox", Some("build"))]))
            .scan()
            .expect("scan");

        assert_eq!(report.candidates.len(), 1);
        let candidate = &report.candidates[0];
        assert_eq!(candidate.listener.port, 5173);
        assert_eq!(
            candidate.pane.as_ref().map(|p| p.pane_id.as_str()),
            Some("w1:p1")
        );
        assert_eq!(report.unattributed, 1, "sshd belongs to no pane");
        assert_eq!(candidate.local_port_free, Some(true));
        assert!(!candidate.already_forwarded);
    }

    #[test]
    fn scan_reads_nothing_but_the_fixed_discovery_commands() {
        let harness = Harness::new(scan_ssh(), FakeHerdr::default(), FakePorts::default());
        harness
            .actions(env(vec![host("buildbox", None)]))
            .scan()
            .expect("scan");
        let commands: Vec<String> = harness
            .ssh
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                SshCall::Exec(_, command) => Some(command),
                _ => None,
            })
            .collect();
        assert_eq!(
            commands,
            vec![
                discover::LSOF_LISTENERS_COMMAND.to_string(),
                discover::PS_TREE_COMMAND.to_string(),
            ]
        );
    }

    #[test]
    fn scan_falls_back_to_ss_when_the_host_has_no_lsof() {
        let ssh = FakeSsh::new()
            .with_master("buildbox.example")
            .with_exec_err(discover::LSOF_LISTENERS_COMMAND, "lsof: command not found")
            .with_exec(
                discover::SS_LISTENERS_COMMAND,
                "LISTEN 0 511 127.0.0.1:5173 0.0.0.0:* users:((\"node\",pid=1234,fd=23))\n",
            )
            .with_exec(discover::PS_TREE_COMMAND, PS_PAIRS);
        let harness = Harness::new(
            ssh,
            FakeHerdr::with_pids(vec![pane("w1:p1", "w1", None)], &[("w1:p1", 900)]),
            FakePorts::default(),
        );
        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .scan()
            .expect("scan");
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(report.candidates[0].listener.port, 5173);
    }

    #[test]
    fn scan_reports_a_host_that_answers_neither_tool() {
        let ssh = FakeSsh::new()
            .with_master("buildbox.example")
            .with_exec_err(discover::LSOF_LISTENERS_COMMAND, "no lsof")
            .with_exec_err(discover::SS_LISTENERS_COMMAND, "no ss");
        let harness = Harness::new(ssh, FakeHerdr::default(), FakePorts::default());
        let err = harness
            .actions(env(vec![host("buildbox", None)]))
            .scan()
            .expect_err("no discovery tool");
        match err {
            ActionError::Probe { message, .. } => {
                assert!(
                    message.contains("no lsof") && message.contains("no ss"),
                    "{message}"
                );
            }
            other => panic!("expected a probe failure, got {other:?}"),
        }
    }

    #[test]
    fn unattributed_listeners_appear_only_when_the_user_asked_for_them() {
        let harness = Harness::new(scan_ssh(), FakeHerdr::default(), FakePorts::default());
        let mut e = env(vec![host("buildbox", None)]);
        e.settings.require_pane_attribution = false;
        let report = harness.actions(e).scan().expect("scan");
        assert_eq!(report.candidates.len(), 2);
        assert!(
            report.candidates.iter().all(|c| c.pane.is_none()),
            "no panes were reported by the server"
        );
        assert_eq!(report.unattributed, 2);
    }

    #[test]
    fn the_picker_puts_an_attributed_listener_first_and_orders_by_port() {
        // The order is load-bearing: the picker highlights row 0, and Enter
        // forwards the highlighted row. With attribution off, an inverted sort
        // makes some infrastructure port the one-keystroke default instead of
        // the agent's dev server.
        let ssh = FakeSsh::new()
            .with_master("buildbox.example")
            .with_exec(
                discover::LSOF_LISTENERS_COMMAND,
                "p1234\ncnode\nn127.0.0.1:9000\n\
                 p1235\ncvite\nn127.0.0.1:3000\n\
                 p4321\ncsshd\nn*:22\n\
                 p4322\nccloudflared\nn127.0.0.1:8080\n",
            )
            .with_exec(
                discover::PS_TREE_COMMAND,
                "1234 900\n1235 900\n4321 1\n4322 1\n900 1\n",
            );
        let harness = Harness::new(
            ssh,
            FakeHerdr::with_pids(vec![pane("w1:p1", "w1", None)], &[("w1:p1", 900)]),
            FakePorts::default(),
        );
        let mut e = env(vec![host("buildbox", None)]);
        e.settings.require_pane_attribution = false;

        let report = harness.actions(e).scan().expect("scan");

        let ports: Vec<u16> = report.candidates.iter().map(|c| c.listener.port).collect();
        assert_eq!(
            ports,
            vec![3000, 9000, 22, 8080],
            "attributed first, each group by port"
        );
        assert!(
            report.candidates[0].pane.is_some(),
            "the top entry is the one a pane is responsible for"
        );
    }

    #[test]
    fn scan_flags_ports_the_allowlist_already_covers() {
        let harness = Harness::new(
            scan_ssh(),
            FakeHerdr::with_pids(
                vec![pane("w1:p1", "w1", Some("build: scholion"))],
                &[("w1:p1", 900)],
            ),
            FakePorts::default(),
        );
        let mut e = env(vec![host("buildbox", Some("build"))]);
        e.context.workspace_label = Some("build: scholion".into());
        e.settings.auto = vec![AutoRule {
            workspace: "scholion".into(),
            ports: vec![5173],
        }];
        let report = harness.actions(e).scan().expect("scan");
        assert!(report.candidates[0].auto_allowed);
    }

    #[test]
    fn scan_changes_nothing() {
        // Listing is not consent: invariant 7 means a candidate becomes a
        // forward only through `add`.
        let harness = Harness::new(
            scan_ssh(),
            FakeHerdr::with_pids(
                vec![pane("w1:p1", "w1", Some("build: scholion"))],
                &[("w1:p1", 900)],
            ),
            FakePorts::default(),
        );
        harness
            .actions(env(vec![host("buildbox", Some("build"))]))
            .scan()
            .expect("scan");
        assert!(harness.store.list().expect("list").is_empty());
        assert!(harness.requests.pending().expect("pending").is_empty());
        assert!(
            !harness
                .ssh
                .calls()
                .iter()
                .any(|call| matches!(call, SshCall::AddForward(..))),
            "scan must not forward anything"
        );
    }

    #[test]
    fn scan_marks_a_port_that_is_already_forwarded() {
        let harness = Harness::new(
            scan_ssh(),
            FakeHerdr::with_pids(vec![pane("w1:p1", "w1", None)], &[("w1:p1", 900)]),
            FakePorts::taken(&[5173]),
        );
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .scan()
            .expect("scan");
        assert!(report.candidates[0].already_forwarded);
        assert_eq!(report.candidates[0].local_port_free, Some(false));
    }

    #[test]
    fn a_listener_on_a_routable_address_is_not_offered() {
        let ssh = FakeSsh::new()
            .with_master("buildbox.example")
            .with_exec(
                discover::LSOF_LISTENERS_COMMAND,
                "p1234\ncnode\nn192.168.1.10:5173\n",
            )
            .with_exec(discover::PS_TREE_COMMAND, PS_PAIRS);
        let harness = Harness::new(
            ssh,
            FakeHerdr::with_pids(vec![pane("w1:p1", "w1", None)], &[("w1:p1", 900)]),
            FakePorts::default(),
        );
        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .scan()
            .expect("scan");
        assert!(report.candidates.is_empty());
        assert_eq!(report.unforwardable, 1);
    }

    #[test]
    fn on_the_far_side_scan_probes_this_machine_and_opens_no_connection() {
        let probe = FakeProbe::default()
            .with(discover::LSOF_LISTENERS_COMMAND, LSOF_FIELDS)
            .with(discover::PS_TREE_COMMAND, PS_PAIRS);
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::with_pids(vec![pane("w1:p1", "w1", None)], &[("w1:p1", 900)]),
            FakePorts::default(),
        );
        let mut e = env(vec![host("buildbox", None)]);
        e.placement = Some(Placement::Remote);
        let mut actions = harness.actions(e);
        actions.probe = Some(&probe);
        let report = actions.scan().expect("scan");

        assert_eq!(report.candidates.len(), 1);
        assert!(harness.ssh.calls().is_empty(), "no ssh on the far side");
    }

    #[test]
    fn on_the_far_side_the_local_port_is_reported_as_unknown_not_as_taken() {
        // Every candidate is a listener on *this* machine, so a bind probe here
        // answers "in use" for all of them — the opposite of the truth about
        // the user's machine, which is where the forward would be opened.
        let probe = FakeProbe::default()
            .with(discover::LSOF_LISTENERS_COMMAND, LSOF_FIELDS)
            .with(discover::PS_TREE_COMMAND, PS_PAIRS);
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::with_pids(vec![pane("w1:p1", "w1", None)], &[("w1:p1", 900)]),
            FakePorts::taken(&[5173]),
        );
        let mut e = env(vec![host("buildbox", None)]);
        e.placement = Some(Placement::Remote);
        let mut actions = harness.actions(e);
        actions.probe = Some(&probe);
        let report = actions.scan().expect("scan");

        assert_eq!(report.candidates[0].local_port_free, None);
        assert!(
            harness.ports.asked().is_empty(),
            "the far side must not bind a socket to answer a question about \
             another machine: probed {:?}",
            harness.ports.asked()
        );
    }

    #[test]
    fn the_discovery_commands_split_into_argv_without_a_shell() {
        for command in [
            discover::LSOF_LISTENERS_COMMAND,
            discover::SS_LISTENERS_COMMAND,
            discover::PS_TREE_COMMAND,
        ] {
            let argv = fixed_argv(command);
            assert!(!argv.is_empty(), "{command:?} produced no argv");
            assert_eq!(argv.join(" "), command, "argv must round-trip {command:?}");
            assert!(
                !command
                    .contains(['"', '\'', '`', '$', '\\', ';', '&', '|', '<', '>', '(', ')', '*']),
                "{command:?} would need a shell to mean what it says"
            );
        }
        assert_eq!(
            fixed_argv(discover::PS_TREE_COMMAND),
            vec!["ps", "-eo", "pid=,ppid="]
        );
    }

    // -- reconcile -----------------------------------------------------------

    fn reconcile_harness(ports: &[u16], panes: Vec<PaneRef>) -> Harness {
        Harness::new(
            FakeSsh::new().with_master("buildbox.example"),
            FakeHerdr::with_panes(panes),
            FakePorts::taken(ports),
        )
    }

    #[test]
    fn reconcile_with_nothing_recorded_does_nothing() {
        let harness = Harness::simple();
        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .reconcile(ReconcileTrigger::Startup)
            .expect("reconcile");
        assert!(report.kept.is_empty() && report.dropped.is_empty());
        assert!(harness.ssh.calls().is_empty());
    }

    #[test]
    fn reconcile_drops_records_for_a_host_that_left_the_config() {
        let harness = reconcile_harness(&[5173], vec![]);
        harness
            .store
            .upsert(forward("retired", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .reconcile(ReconcileTrigger::Event)
            .expect("reconcile");

        assert_eq!(report.dropped.len(), 1);
        assert_eq!(report.dropped[0].reason, DropReason::HostGone);
        assert!(harness.store.list().expect("list").is_empty());
    }

    #[test]
    fn reconcile_releases_a_forward_whose_pane_is_gone() {
        let harness = reconcile_harness(&[5173], vec![pane("w1:p1", "w1", None)]);
        harness
            .store
            .upsert(forward(
                "buildbox",
                5173,
                5173,
                Origin::Offered,
                Some("w1:p9"),
            ))
            .expect("seed");
        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .reconcile(ReconcileTrigger::Event)
            .expect("reconcile");

        assert_eq!(report.dropped[0].reason, DropReason::PaneGone);
        assert!(harness.ssh.calls().contains(&SshCall::CancelForward(
            "buildbox.example".into(),
            "localhost:5173:127.0.0.1:5173".into()
        )));
        assert!(harness.store.list().expect("list").is_empty());
    }

    #[test]
    fn a_user_pinned_forward_outlives_its_pane() {
        let harness = reconcile_harness(&[5173], vec![pane("w1:p1", "w1", None)]);
        harness
            .store
            .upsert(forward(
                "buildbox",
                5173,
                5173,
                Origin::Explicit,
                Some("w1:p9"),
            ))
            .expect("seed");
        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .reconcile(ReconcileTrigger::Event)
            .expect("reconcile");

        assert!(report.dropped.is_empty(), "{:?}", report.dropped);
        assert_eq!(report.kept.len(), 1);
        assert_eq!(harness.store.list().expect("list").len(), 1);
    }

    #[test]
    fn an_allowlisted_forward_is_released_with_its_pane_too() {
        let harness = reconcile_harness(&[5173], vec![]);
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Auto, Some("w1:p9")))
            .expect("seed");
        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .reconcile(ReconcileTrigger::Event)
            .expect("reconcile");
        assert_eq!(report.dropped[0].reason, DropReason::PaneGone);
    }

    #[test]
    fn a_forward_that_vanished_is_put_back() {
        // Nothing is listening on 5173, so the record is not describing a live
        // forward: invariant 3 says restore it or remove it, not leave it.
        let harness = reconcile_harness(&[], vec![pane("w1:p1", "w1", None)]);
        harness
            .store
            .upsert(forward(
                "buildbox",
                5173,
                5173,
                Origin::Offered,
                Some("w1:p1"),
            ))
            .expect("seed");
        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .reconcile(ReconcileTrigger::Event)
            .expect("reconcile");

        assert_eq!(report.readded.len(), 1);
        assert!(report.dropped.is_empty());
        assert!(harness.ssh.calls().contains(&SshCall::AddForward(
            "buildbox.example".into(),
            "localhost:5173:127.0.0.1:5173".into()
        )));
        assert_eq!(harness.store.list().expect("list").len(), 1);
    }

    #[test]
    fn a_forward_that_cannot_be_put_back_stops_being_a_record() {
        let harness = Harness::new(
            FakeSsh {
                add_fails: Some("cannot listen to port: 5173".into()),
                ..FakeSsh::new()
            }
            .with_master("buildbox.example"),
            FakeHerdr::default(),
            FakePorts::default(),
        );
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .reconcile(ReconcileTrigger::Startup)
            .expect("reconcile");

        assert!(matches!(
            report.dropped[0].reason,
            DropReason::ReAddFailed(_)
        ));
        assert!(
            harness.store.list().expect("list").is_empty(),
            "a record that cannot be made real must not shadow the port"
        );
    }

    #[test]
    fn an_event_hook_never_opens_a_connection() {
        let harness = reconcile_harness(&[], vec![]);
        harness
            .store
            .upsert(forward("other", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let mut e = env(vec![host("buildbox", None), host("other", None)]);
        e.explicit_host = Some("buildbox".into());
        let report = harness
            .actions(e)
            .reconcile(ReconcileTrigger::Event)
            .expect("reconcile");

        assert_eq!(report.dropped[0].reason, DropReason::MasterGone);
        assert!(
            !harness
                .ssh
                .calls()
                .iter()
                .any(|call| matches!(call, SshCall::EnsureMaster(_))),
            "a workspace focus must not dial anything"
        );
    }

    #[test]
    fn startup_restores_the_master_and_the_forwards_on_it() {
        let harness = Harness::new(FakeSsh::new(), FakeHerdr::default(), FakePorts::default());
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .reconcile(ReconcileTrigger::Startup)
            .expect("reconcile");

        assert_eq!(report.readded.len(), 1);
        assert!(harness
            .ssh
            .calls()
            .contains(&SshCall::EnsureMaster("buildbox.example".into())));
        assert_eq!(harness.store.list().expect("list").len(), 1);
    }

    #[test]
    fn a_host_that_cannot_be_reached_keeps_none_of_its_records() {
        let harness = Harness::new(
            FakeSsh {
                ensure_fails: true,
                ..FakeSsh::new()
            },
            FakeHerdr::default(),
            FakePorts::default(),
        );
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .reconcile(ReconcileTrigger::Startup)
            .expect("reconcile");
        assert!(matches!(
            report.dropped[0].reason,
            DropReason::Unreachable(_)
        ));
        assert!(harness.store.list().expect("list").is_empty());
    }

    #[test]
    fn a_herdr_server_that_cannot_be_reached_never_costs_a_forward() {
        // Not knowing which panes exist is not the same as the pane being gone.
        let harness = Harness::new(
            FakeSsh::new().with_master("buildbox.example"),
            FakeHerdr::unreachable(),
            FakePorts::taken(&[5173]),
        );
        harness
            .store
            .upsert(forward(
                "buildbox",
                5173,
                5173,
                Origin::Offered,
                Some("w1:p9"),
            ))
            .expect("seed");
        let report = harness
            .actions(env(vec![host("buildbox", None)]))
            .reconcile(ReconcileTrigger::Event)
            .expect("reconcile");
        assert!(report.dropped.is_empty(), "{:?}", report.dropped);
        assert_eq!(report.kept.len(), 1);
    }

    #[test]
    fn reconcile_handles_every_host_with_records_not_just_the_selected_one() {
        let harness = Harness::new(
            FakeSsh::new()
                .with_master("buildbox.example")
                .with_master("other.example"),
            FakeHerdr::default(),
            FakePorts::taken(&[5173, 8787]),
        );
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        harness
            .store
            .upsert(forward("other", 8787, 8787, Origin::Explicit, None))
            .expect("seed");
        harness
            .store
            .upsert(forward("gone", 3000, 3000, Origin::Explicit, None))
            .expect("seed");

        let mut e = env(vec![host("buildbox", None), host("other", None)]);
        e.explicit_host = Some("buildbox".into());
        let report = harness
            .actions(e)
            .reconcile(ReconcileTrigger::Startup)
            .expect("reconcile");

        assert_eq!(report.kept.len(), 2);
        assert_eq!(report.dropped.len(), 1);
        assert_eq!(report.dropped[0].forward.spec.host, "gone");
        let left: Vec<String> = harness
            .store
            .list()
            .expect("list")
            .into_iter()
            .map(|f| f.spec.host)
            .collect();
        assert_eq!(left, vec!["buildbox".to_string(), "other".to_string()]);
    }

    #[test]
    fn on_the_far_side_reconcile_prunes_records_without_touching_ssh() {
        let harness = Harness::new(
            FakeSsh::new(),
            FakeHerdr::with_panes(vec![pane("w1:p1", "w1", None)]),
            FakePorts::default(),
        );
        harness
            .store
            .upsert(forward(
                "buildbox",
                5173,
                5173,
                Origin::Offered,
                Some("w1:p9"),
            ))
            .expect("seed");
        harness
            .store
            .upsert(forward(
                "buildbox",
                8787,
                8787,
                Origin::Offered,
                Some("w1:p1"),
            ))
            .expect("seed");
        let mut e = env(vec![host("buildbox", None)]);
        e.placement = Some(Placement::Remote);
        let report = harness
            .actions(e)
            .reconcile(ReconcileTrigger::Event)
            .expect("reconcile");

        assert_eq!(report.dropped.len(), 1);
        assert_eq!(report.kept.len(), 1);
        assert!(harness.ssh.calls().is_empty());
    }

    /// Invariant 2, one level above `Store`. Reconcile reads, spends seconds on
    /// ssh, and writes; a forward another process recorded in that window is
    /// not this pass's to forget.
    #[test]
    fn a_forward_recorded_during_the_pass_survives_the_reconcile() {
        let harness = reconcile_harness(&[], vec![]);
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let ports = RacingPorts::new(
            harness.dir(),
            true,
            vec![Interleaved::Upsert(forward(
                "buildbox",
                4444,
                4444,
                Origin::Explicit,
                None,
            ))],
        );

        let report = harness
            .actions_with(env(vec![host("buildbox", None)]), &ports)
            .reconcile(ReconcileTrigger::Event)
            .expect("reconcile");

        assert!(report.dropped.is_empty(), "{:?}", report.dropped);
        let ports: Vec<u16> = harness
            .store
            .list()
            .expect("list")
            .iter()
            .map(|f| f.spec.local_port)
            .collect();
        assert_eq!(
            ports,
            vec![4444, 5173],
            "a forward recorded mid-pass was wiped, leaving it live and untracked"
        );
    }

    #[test]
    fn a_forward_stopped_during_the_pass_is_not_resurrected() {
        // The mirror image: the user ran `stop 5173` while reconcile was still
        // deciding from a snapshot that had it. Re-adding it would hand back a
        // forward the user just asked to be rid of.
        let harness = reconcile_harness(&[], vec![]);
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let ports = RacingPorts::new(
            harness.dir(),
            false,
            vec![Interleaved::Remove("buildbox".to_string(), 5173)],
        );

        let report = harness
            .actions_with(env(vec![host("buildbox", None)]), &ports)
            .reconcile(ReconcileTrigger::Event)
            .expect("reconcile");

        assert!(
            harness.store.list().expect("list").is_empty(),
            "the record came back from a stale snapshot"
        );
        assert!(report.kept.is_empty() && report.readded.is_empty());
        assert_eq!(report.dropped.len(), 1);
        assert_eq!(report.dropped[0].reason, DropReason::Superseded);
        assert!(
            harness.ssh.calls().contains(&SshCall::CancelForward(
                "buildbox.example".into(),
                "localhost:5173:127.0.0.1:5173".into()
            )),
            "a re-added forward with no record left behind is invariant 3's \
             failure: {:?}",
            harness.ssh.calls()
        );
    }

    #[test]
    fn a_record_rewritten_during_the_pass_keeps_the_newer_writers_version() {
        // Reconcile is carrying the snapshot's version of this record forward
        // while another process rewrites the same key — the local agent
        // re-recording a forward it just applied. The newer write knew more
        // about the port than the snapshot did.
        let harness = reconcile_harness(&[], vec![]);
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let mut newer = forward("buildbox", 5173, 5173, Origin::Offered, Some("w1:p1"));
        newer.created_unix_ms = NOW + 1;
        let ports = RacingPorts::new(harness.dir(), true, vec![Interleaved::Upsert(newer)]);

        harness
            .actions_with(env(vec![host("buildbox", None)]), &ports)
            .reconcile(ReconcileTrigger::Event)
            .expect("reconcile");

        let left = harness.store.list().expect("list");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].spec.origin, Origin::Offered);
        assert_eq!(left[0].created_unix_ms, NOW + 1);
    }

    #[test]
    fn the_merge_touches_only_the_records_the_pass_actually_read() {
        let snapshot = vec![
            forward("buildbox", 5173, 5173, Origin::Explicit, None),
            forward("buildbox", 8787, 8787, Origin::Offered, None),
        ];
        // Kept 5173, dropped 8787.
        let keep = vec![snapshot[0].clone()];

        let mut rewritten = forward("buildbox", 8787, 8787, Origin::Explicit, None);
        rewritten.created_unix_ms = NOW + 5;
        let mut current = vec![
            snapshot[0].clone(),
            rewritten.clone(),
            forward("buildbox", 4444, 4444, Origin::Explicit, None),
        ];

        let superseded = merge_reconciled(&snapshot, keep, &mut current);

        assert!(superseded.is_empty());
        assert_eq!(
            current,
            vec![
                forward("buildbox", 5173, 5173, Origin::Explicit, None),
                rewritten,
                forward("buildbox", 4444, 4444, Origin::Explicit, None),
            ],
            "only a record identical to the snapshot is this pass's to decide"
        );
    }

    #[test]
    fn the_merge_reports_a_record_that_left_instead_of_writing_it_back() {
        let snapshot = vec![forward("buildbox", 5173, 5173, Origin::Explicit, None)];
        let keep = snapshot.clone();
        let mut current: Vec<Forward> = Vec::new();

        let superseded = merge_reconciled(&snapshot, keep, &mut current);

        assert_eq!(superseded.len(), 1);
        assert_eq!(superseded[0].spec.local_port, 5173);
        assert!(current.is_empty());
    }

    #[test]
    fn the_merge_over_an_untouched_file_is_the_decision_it_was_given() {
        let snapshot = vec![
            forward("alpha", 5173, 5173, Origin::Explicit, None),
            forward("beta", 8787, 8787, Origin::Offered, None),
        ];
        let mut current = snapshot.clone();

        let superseded = merge_reconciled(&snapshot, vec![snapshot[1].clone()], &mut current);

        assert!(superseded.is_empty());
        assert_eq!(current, vec![snapshot[1].clone()]);
    }

    // -- status --------------------------------------------------------------

    #[test]
    fn status_reports_hosts_masters_and_forwards() {
        let harness = Harness::new(
            FakeSsh::new().with_master("buildbox.example"),
            FakeHerdr::default(),
            FakePorts::default(),
        );
        harness
            .store
            .upsert(forward("buildbox", 5173, 5173, Origin::Explicit, None))
            .expect("seed");
        let mut e = env(vec![host("buildbox", None), host("other", None)]);
        e.explicit_host = Some("buildbox".into());
        e.hosts.warnings = vec!["host \"typo\" has no target".into()];
        let report = harness.actions(e).status().expect("status");

        let selected = report.selected.expect("a host was selected");
        assert_eq!(selected.name, "buildbox");
        assert_eq!(selected.source, "explicit");
        assert_eq!(report.hosts.len(), 2);
        assert_eq!(report.hosts[0].master_running, Some(true));
        assert_eq!(report.hosts[1].master_running, Some(false));
        assert_eq!(report.hosts[0].forwards.as_deref().map(<[_]>::len), Some(1));
        assert_eq!(report.warnings.len(), 1);
        assert!(report.selection_error.is_none());
    }

    #[test]
    fn status_still_reports_when_no_host_can_be_chosen() {
        // status is what someone runs when the setup is already broken, so it
        // has to describe the breakage instead of becoming it.
        let harness = Harness::simple();
        let report = harness.actions(env(Vec::new())).status().expect("status");
        assert!(report.selected.is_none());
        assert!(report
            .selection_error
            .expect("a reason")
            .contains("no hosts are configured"));
        assert!(report.hosts.is_empty());
    }

    #[test]
    fn status_counts_requests_waiting_for_the_local_agent() {
        let harness = Harness::simple();
        harness
            .requests
            .enqueue(spec("buildbox", 5173, 5173, Origin::Explicit), NOW)
            .expect("enqueue");
        let mut e = env(vec![host("buildbox", None)]);
        e.placement = Some(Placement::Remote);
        let report = harness.actions(e).status().expect("status");
        assert_eq!(report.pending_requests, Some(1));
        assert_eq!(report.hosts[0].master_running, None);
    }

    #[test]
    fn the_reported_placement_explains_which_rule_chose_it() {
        let harness = Harness::simple();
        let mut e = env(vec![host("buildbox", Some("build"))]);
        e.placement = None;
        e.context.workspace_label = Some("build: scholion".into());
        let report = harness.actions(e).status().expect("status");
        let selected = report.selected.expect("selected");
        assert_eq!(selected.source, "workspace-prefix");
        assert_eq!(selected.placement.reason, PlacementReason::MirrorWorkspace);
    }

    #[test]
    fn scan_asks_for_a_shell_pid_only_for_panes_that_did_not_carry_one() {
        // `pane list` does not include shell_pid, so attribution needs a
        // process-info call per pane — but not for a pane that already has one.
        let mut carried = pane("w1:p2", "w1", None);
        carried.shell_pid = Some(4321);
        let harness = Harness::new(
            scan_ssh(),
            FakeHerdr::with_pids(vec![pane("w1:p1", "w1", None), carried], &[("w1:p1", 900)]),
            FakePorts::default(),
        );
        harness
            .actions(env(vec![host("buildbox", None)]))
            .scan()
            .expect("scan");
        assert_eq!(
            harness.herdr.calls(),
            vec!["pane_list".to_string(), "shell_pid:w1:p1".to_string()]
        );
    }
}
