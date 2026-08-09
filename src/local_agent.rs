//! The local half, for `herdr --remote`.
//!
//! `attach <host>` starts a control master, spawns this, and execs
//! `herdr --remote <host>` so the user's session looks normal. This polls the
//! remote for requests and for new listeners, applies forwards to *its own*
//! master, and offers detections through a Herdr notification.
//!
//! It must survive the remote being briefly unreachable, must not leak masters
//! or forwards when the attach ends, and must never apply a forward the user
//! did not ask for. Detection uses polling because Herdr's manifest event hooks
//! deliberately exclude terminal-output events.
//!
//! Two shapes make the rest of the module testable without a network, a clock,
//! or a process:
//!
//! - [`Agent::step`] is one pass of the loop. Every remote failure lands in
//!   [`StepReport::errors`] instead of returning, so the loop backs off rather
//!   than exiting on the first timeout, and a test can drive a thousand passes
//!   in no time at all through an injected [`Clock`].
//! - [`Attach::run`] supervises the session over injected [`Launcher`] and
//!   [`Interrupts`] seams, and every exit path from it — return, error, signal,
//!   panic — goes through one [`Cleanup`].
//!
//! `attach` does not literally `execve` the Herdr client, despite the shorthand
//! above: a replaced process cannot tear down the master it started. It spawns
//! the client on the inherited terminal and waits, which looks identical from
//! the user's seat and leaves someone alive to clean up.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::actions::{parse_loopback_url, LocalPorts, SystemOpener, SystemPorts, UrlOpener};
use crate::config::Settings;
use crate::discover;
use crate::herdr::{HerdrClient, Notification};
use crate::request::{Queue, Request, DEFAULT_TTL_MS};
use crate::ssh::{MasterState, SshError, SshRunner, MAX_CONTROL_PATH_LEN};
use crate::state::{stderr_reporter, Reporter, Store};
use crate::types::{AttributedListener, Forward, ForwardSpec, Host, Listener, Origin, PaneRef};

/// Interval between passes when discovery is switched off. Requests still have
/// to be picked up: a click on a URL should not wait for the next minute.
pub const DEFAULT_TICK_SECONDS: u64 = 5;

/// Ceiling on the backoff. Long enough that an unreachable host costs nothing,
/// short enough that a laptop coming back from sleep reconnects while the user
/// is still looking at the screen.
pub const MAX_BACKOFF_SECONDS: u64 = 60;

/// Doublings applied to the base interval. `2^6 * 5s` is already past the
/// ceiling; the shift is capped anyway so the multiply cannot overflow.
const MAX_BACKOFF_SHIFT: u32 = 6;

/// How often the attach supervisor checks on the session it is waiting for.
pub const SUPERVISE_TICK: Duration = Duration::from_millis(100);

/// Ticks a signalled session gets to exit before the attach stops waiting for
/// it. Without a bound, a client that ignores SIGTERM would keep the master
/// alive forever, which is the failure this module exists to prevent.
pub const TERMINATE_GRACE_TICKS: usize = 100;

/// v1 forwards to loopback only, and IPv4 loopback is what a remote dev server
/// answers on unless it bound v6 exclusively.
const LOOPBACK_V4: &str = "127.0.0.1";
const LOOPBACK_V6: &str = "::1";

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Ssh(#[from] SshError),
    #[error(transparent)]
    State(#[from] crate::state::StateError),
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Herdr(#[from] crate::herdr::HerdrError),
    #[error("could not start {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AgentError>;

// ---------------------------------------------------------------------------
// injected seams
// ---------------------------------------------------------------------------

/// Time, injected so a test can run a hundred backoff cycles instantly.
pub trait Clock {
    fn now_unix_ms(&self) -> u64;
    fn sleep(&self, duration: Duration);
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|since| u64::try_from(since.as_millis()).ok())
            .unwrap_or(0)
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Where pending requests come from, and where the fact that they were dealt
/// with goes back.
///
/// Under herdr-mirror this is a local file; under `herdr --remote` it is a file
/// on the far side of the SSH master, written by a plugin running on that
/// machine. Reading twice must be harmless: the agent de-duplicates, because
/// the transport cannot.
///
/// Only the applier knows a forward was really made, so only the applier can
/// settle a request — and it has to say so where the *writer* can see it, or
/// `list` and `status` over there go on reporting an applied request as waiting
/// for this agent, and this agent re-applies it after a restart. That is what
/// [`RequestSource::settled`] is for, and [`RequestSource::settled_mark`] is how
/// a fresh agent picks up where the last one left off.
pub trait RequestSource {
    fn pending(&self) -> Result<Vec<Request>>;

    /// Highest request id an applier has already finished with.
    fn settled_mark(&self) -> Result<u64>;

    /// Record that every request up to and including `id` has been applied,
    /// skipped, or expired. Must be idempotent and must never move the mark
    /// backwards.
    fn settled(&self, id: u64) -> Result<()>;
}

/// An [`SshRunner`] shared by the agent, its Herdr client, and its request
/// source without any of them owning it.
pub struct SharedSsh<S>(Arc<S>);

impl<S> SharedSsh<S> {
    pub fn new(ssh: S) -> Self {
        Self(Arc::new(ssh))
    }
}

impl<S> Clone for SharedSsh<S> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<S: SshRunner> SshRunner for SharedSsh<S> {
    fn ensure_master(&self, target: &str, control_path: &Path) -> crate::ssh::Result<()> {
        self.0.ensure_master(target, control_path)
    }

    fn check_master(&self, target: &str, control_path: &Path) -> crate::ssh::Result<MasterState> {
        self.0.check_master(target, control_path)
    }

    fn add_forward(
        &self,
        target: &str,
        control_path: &Path,
        spec: &ForwardSpec,
    ) -> crate::ssh::Result<()> {
        self.0.add_forward(target, control_path, spec)
    }

    fn cancel_forward(
        &self,
        target: &str,
        control_path: &Path,
        spec: &ForwardSpec,
    ) -> crate::ssh::Result<()> {
        self.0.cancel_forward(target, control_path, spec)
    }

    fn exit_master(&self, target: &str, control_path: &Path) -> crate::ssh::Result<()> {
        self.0.exit_master(target, control_path)
    }

    fn exec(&self, target: &str, control_path: &Path, command: &str) -> crate::ssh::Result<String> {
        self.0.exec(target, control_path, command)
    }
}

// ---------------------------------------------------------------------------
// request sources
// ---------------------------------------------------------------------------

/// The request log on this machine. What herdr-mirror mode and a local test
/// both use.
pub struct LocalRequests {
    log: crate::request::RequestLog,
}

impl LocalRequests {
    pub fn new(log: crate::request::RequestLog) -> Self {
        Self { log }
    }
}

impl RequestSource for LocalRequests {
    fn pending(&self) -> Result<Vec<Request>> {
        Ok(self.log.pending()?)
    }

    fn settled_mark(&self) -> Result<u64> {
        Ok(self.log.last_consumed_id()?)
    }

    /// Sharing the filesystem with the writer, this settles a request the
    /// direct way: under the log's own lock, dropping exactly what was named.
    fn settled(&self, id: u64) -> Result<()> {
        self.log.consume_through(id)?;
        Ok(())
    }
}

/// Reads the remote request file over the agent's own control master, and
/// writes back the mark saying how far it has got.
///
/// The request file itself stays untouched: rewriting it from a shell cannot be
/// made atomic against the plugin writing it at the same moment, and a lost
/// enqueue is a Ctrl+click that silently did nothing. The mark
/// ([`crate::request::ACK_FILE`]) is a different matter — one integer with a
/// single writer, replaced by a rename — so it can be written from a shell
/// safely, and the plugin over there folds it into everything it reports.
pub struct RemoteRequests<S> {
    ssh: S,
    target: String,
    control_path: PathBuf,
    reporter: Reporter,
}

/// Leaves the remote plugin state directory in `$p`, or `$p` empty.
///
/// First candidate is the one [`crate::state::state_dir_from`] resolves, since
/// that is the only place this version of the plugin writes — records and the
/// control socket share one directory precisely so both halves of the tool can
/// find them by one rule.
///
/// The rest are kept for version skew, which is real here: the agent is
/// installed on the user's machine and the plugin on the remote, and they are
/// updated separately. A remote still running a plugin that obeyed
/// `HERDR_PLUGIN_STATE_DIR` — Herdr's `<state>/plugins/<id>` under
/// `XDG_STATE_HOME` or `~/.local/state`, or the debug-build name — is still
/// reachable. A directory only counts when the request file is in it, so an
/// empty leftover never shadows the live one and the mark is always written
/// beside the requests it settles.
///
/// A macro rather than a `const` because `concat!` only joins literals, and the
/// three snippets below must share one copy of this list.
macro_rules! remote_state_dir {
    () => {
        concat!(
            "p=''; for d in ",
            "\"${XDG_STATE_HOME:-$HOME/.local/state}/herdr-portfwd\" ",
            "\"${HERDR_PLUGIN_STATE_DIR:-}\" ",
            "\"${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/portfwd\" ",
            "\"${XDG_STATE_HOME:-$HOME/.local/state}/herdr-dev/plugins/portfwd\"",
            "; do if [ -n \"$d\" ] && [ -f \"$d/requests.json\" ]; then p=\"$d\"; break; fi;",
            " done; "
        )
    };
}

/// Locates and prints the remote request file.
///
/// Fixed text with no interpolation: nothing user-controlled reaches the remote
/// shell (invariant 9).
pub const REMOTE_REQUESTS_COMMAND: &str = concat!(
    remote_state_dir!(),
    "if [ -n \"$p\" ]; then cat \"$p/requests.json\"; fi"
);

/// Prints the mark an applier left beside the remote request file, or nothing.
pub const REMOTE_MARK_COMMAND: &str = concat!(
    remote_state_dir!(),
    "if [ -n \"$p\" ] && [ -f \"$p/requests.acked\" ]; then cat \"$p/requests.acked\"; fi"
);

/// Records `id` as the highest request this agent has finished with.
///
/// The one snippet here that is not fixed text. What it interpolates is a `u64`
/// rendered by `format!`, which is decimal digits and nothing else — no caller
/// string reaches the remote shell, so invariant 9 still holds; the test below
/// pins that.
///
/// Written by a temp file and a rename, so a reader sees the old integer or the
/// new one and never a half-written line, and only ever forwards: a mark that
/// went backwards would re-offer a request the user has since stopped. The
/// comparison is guarded rather than trusted — a file somebody else planted
/// makes `[` fail, and failing to write the mark is the one outcome that leaves
/// this agent unable to settle anything.
pub fn remote_mark_command(id: u64) -> String {
    // Two `format!` calls rather than one: the directory list above is full of
    // `${...}` and would have to be brace-escaped to survive a format string,
    // which is a copy of it waiting to drift from the one the other two
    // snippets use.
    let write = format!(
        "if [ -n \"$p\" ]; then o=$(cat \"$p/requests.acked\" 2>/dev/null); \
         case \"$o\" in ''|*[!0-9]*) o=0;; esac; \
         if [ \"$o\" -ge {id} ] 2>/dev/null; then :; else \
         t=\"$p/requests.acked.$$\"; \
         printf '%s\\n' {id} > \"$t\" && mv \"$t\" \"$p/requests.acked\"; fi; fi"
    );
    format!("{}{write}", remote_state_dir!())
}

impl<S> RemoteRequests<S> {
    pub fn new(ssh: S, target: impl Into<String>, control_path: impl Into<PathBuf>) -> Self {
        Self {
            ssh,
            target: target.into(),
            control_path: control_path.into(),
            reporter: stderr_reporter(),
        }
    }

    pub fn with_reporter(mut self, reporter: Reporter) -> Self {
        self.reporter = reporter;
        self
    }
}

impl<S: SshRunner> RequestSource for RemoteRequests<S> {
    fn pending(&self) -> Result<Vec<Request>> {
        let stdout = self
            .ssh
            .exec(&self.target, &self.control_path, REMOTE_REQUESTS_COMMAND)?;
        Ok(parse_pending(&stdout).unwrap_or_else(|why| {
            // A document we cannot read is not a reason to stop polling: the
            // next write from the plugin may well be readable.
            (self.reporter)(&format!("could not read the remote request file: {why}"));
            Vec::new()
        }))
    }

    fn settled_mark(&self) -> Result<u64> {
        let stdout = self
            .ssh
            .exec(&self.target, &self.control_path, REMOTE_MARK_COMMAND)?;
        Ok(parse_mark(&stdout))
    }

    fn settled(&self, id: u64) -> Result<()> {
        self.ssh
            .exec(&self.target, &self.control_path, &remote_mark_command(id))?;
        Ok(())
    }
}

/// The applier's mark as printed by [`REMOTE_MARK_COMMAND`].
///
/// Pure and total. No file, an empty file, or anything that is not a plain
/// number all read as zero: the mark can only settle requests, so an unreadable
/// one has to mean "nothing has been settled" rather than "settle everything".
pub fn parse_mark(text: &str) -> u64 {
    text.trim().parse().unwrap_or(0)
}

/// Pending requests from a serialized [`Queue`], oldest id first.
///
/// Pure: the transport hands it a string and gets values back. Empty input is
/// "no requests", not an error — that is what a host with no request file, or
/// one whose file is mid-create, looks like.
pub fn parse_pending(text: &str) -> std::result::Result<Vec<Request>, serde_json::Error> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let queue: Queue = serde_json::from_str(text)?;
    let mut pending = queue.pending;
    // The document carries its own high-water mark; a request at or below it
    // has been settled by whoever wrote that mark and is not ours to apply.
    pending.retain(|request| request.id > queue.last_consumed_id);
    pending.sort_by_key(|request| request.id);
    Ok(pending)
}

// ---------------------------------------------------------------------------
// the polling agent
// ---------------------------------------------------------------------------

/// Which listener command this host answered. Remembered so a host without
/// `lsof` is not asked for it every pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Probe {
    #[default]
    Unknown,
    Lsof,
    Ss,
}

#[derive(Debug, Default)]
struct AgentState {
    /// Highest request id known to be settled. Everything at or below it has
    /// been applied, skipped, or expired.
    last_applied_id: u64,
    /// Whether [`AgentState::last_applied_id`] has been seeded from the mark
    /// the request source holds. Purely in-memory state cannot answer this: a
    /// restarted agent starts at zero and would re-apply every request still
    /// inside its TTL, including one whose port the user has since stopped.
    mark_read: bool,
    /// *Remote* ports already offered, auto-forwarded, or known to be
    /// forwarded. What keeps a long-lived dev server from producing a
    /// notification a minute. Keyed by the remote port because that is what
    /// discovery sees; a forward that was remapped is recorded under its local
    /// port and would otherwise look unforwarded here.
    handled: HashSet<u16>,
    /// Ports whose forward failed and whose failure the user has been told
    /// about once. Not pruned with `handled`: a port that failed to open here
    /// has nothing to do with what is listening on the far side, and forgetting
    /// it would produce the notification per poll this set exists to prevent.
    reported: HashSet<u16>,
    /// Requests whose URL has already been handed to a browser, so one the
    /// agent meets twice — a request the mark has not settled yet because an
    /// earlier one is still failing — does not become two windows.
    ///
    /// Keyed by request id and deliberately not by the URL. A second Ctrl+click
    /// on the same URL, after the user closed the tab, is a *new* request with a
    /// new id, and it asks for a window as much as the first one did; keying
    /// this by the string swallowed it for the life of the agent. Bounded, and
    /// it has to be: the ids come off requests written on the far side, so an
    /// unbounded set is memory a compromised remote can grow for free. Past the
    /// cap the oldest is forgotten, which at worst costs a duplicate window for
    /// a request nobody has met in a while.
    opened: Vec<u64>,
    /// Consecutive failing passes, the exponent behind [`Agent::delay`].
    failures: u32,
    probe: Probe,
}

/// How many opened requests [`AgentState::opened`] remembers.
const MAX_REMEMBERED_OPENS: usize = 64;

/// Everything the agent needs that is not a trait.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub host: Host,
    pub control_path: PathBuf,
    pub settings: Settings,
    /// Stop when the master is gone instead of waiting for it to come back.
    /// `attach` spawns the agent this way: the master dying *is* the session
    /// ending.
    pub oneshot: bool,
    pub ttl_ms: u64,
}

impl AgentConfig {
    pub fn new(host: Host, control_path: impl Into<PathBuf>) -> Self {
        Self {
            host,
            control_path: control_path.into(),
            settings: Settings::default(),
            oneshot: true,
            ttl_ms: DEFAULT_TTL_MS,
        }
    }
}

/// What one pass did. Errors are collected rather than returned so a pass can
/// half-succeed: applying a request must not be skipped because discovery is
/// broken, and neither is a reason to exit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepReport {
    /// Local ports forwarded because a request asked for them.
    pub applied: Vec<u16>,
    /// Local ports forwarded because an allowlist matched (L3).
    pub auto: Vec<u16>,
    /// Ports offered through a notification (L2). Never forwarded here.
    pub offered: Vec<u16>,
    /// URLs handed to a browser on this machine, after their forward was in
    /// place.
    pub opened: Vec<String>,
    /// Requests that timed out before anyone applied them.
    pub expired: Vec<u16>,
    pub errors: Vec<String>,
    /// The master is gone. Whether that ends the loop is [`AgentConfig::oneshot`].
    pub master_gone: bool,
}

impl StepReport {
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Why [`Agent::run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    MasterGone,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub reason: StopReason,
    pub steps: usize,
    pub applied: usize,
    pub offered: usize,
    pub auto: usize,
}

/// The polling loop, composed over the traits so no test needs a network.
pub struct Agent<S, H, R, C> {
    ssh: S,
    herdr: H,
    requests: R,
    clock: C,
    store: Store,
    config: AgentConfig,
    reporter: Reporter,
    /// Where a URL a request asked for goes. This agent is the half of the tool
    /// that runs on the machine with the browser, so it is the only place a
    /// Ctrl+click under `herdr --remote` can be honoured — and a trait object
    /// rather than a fifth type parameter because every existing caller and
    /// test names the other four, and a test must never launch a browser.
    opener: Box<dyn UrlOpener>,
    /// Whether a local port is bound here. A record is not a forward: one left
    /// by a master that has gone says a port is forwarded when nothing is
    /// listening on it, and this is how the agent tells the two apart. The same
    /// seam `Actions` asks the same question through, for the same reason — the
    /// real implementation binds a socket and a test must not.
    ports: Box<dyn LocalPorts>,
    shutdown: Arc<AtomicBool>,
    state: AgentState,
}

impl<S, H, R, C> Agent<S, H, R, C>
where
    S: SshRunner,
    H: HerdrClient,
    R: RequestSource,
    C: Clock,
{
    pub fn new(ssh: S, herdr: H, requests: R, clock: C, store: Store, config: AgentConfig) -> Self {
        Self {
            ssh,
            herdr,
            requests,
            clock,
            store,
            config,
            reporter: stderr_reporter(),
            opener: Box::new(SystemOpener),
            ports: Box::new(SystemPorts),
            shutdown: Arc::new(AtomicBool::new(false)),
            state: AgentState::default(),
        }
    }

    pub fn with_reporter(mut self, reporter: Reporter) -> Self {
        self.reporter = reporter;
        self
    }

    /// Replace the desktop opener. Tests install one that records instead of
    /// launching anything.
    pub fn with_opener(mut self, opener: Box<dyn UrlOpener>) -> Self {
        self.opener = opener;
        self
    }

    /// Replace the local-port probe. Tests install one that answers from their
    /// fake master's own bindings, so no socket is ever bound.
    pub fn with_ports(mut self, ports: Box<dyn LocalPorts>) -> Self {
        self.ports = ports;
        self
    }

    /// Set this to end the loop after the pass in flight. The attach supervisor
    /// does not use it — it kills the agent process — but a signal handler in a
    /// standalone agent does.
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }

    pub fn failures(&self) -> u32 {
        self.state.failures
    }

    /// How long to wait before the next pass: the base interval doubled once
    /// per consecutive failure, capped. Backing off is what keeps an
    /// unreachable host from turning into a spin loop over ssh connects.
    pub fn delay(&self) -> Duration {
        let base = if self.config.settings.discovery_enabled() {
            self.config.settings.poll_seconds
        } else {
            DEFAULT_TICK_SECONDS
        };
        let shift = self.state.failures.min(MAX_BACKOFF_SHIFT);
        let seconds = base
            .max(1)
            .saturating_mul(1u64 << shift)
            .min(MAX_BACKOFF_SECONDS);
        Duration::from_secs(seconds)
    }

    /// One pass: check the master, apply what was requested, then look for
    /// something to offer.
    pub fn step(&mut self) -> StepReport {
        let mut report = StepReport::default();
        match self
            .ssh
            .check_master(&self.config.host.target, &self.config.control_path)
        {
            Ok(MasterState::Running) => {}
            Ok(MasterState::Absent) => {
                report.master_gone = true;
                if self.config.oneshot {
                    return report;
                }
                report
                    .errors
                    .push("the control master is not running".to_string());
                self.bump();
                return report;
            }
            Err(err) => {
                report.errors.push(err.to_string());
                self.bump();
                return report;
            }
        }

        let now = self.clock.now_unix_ms();
        self.apply_requests(now, &mut report);
        if self.config.settings.discovery_enabled() {
            self.discover(now, &mut report);
        }

        if report.is_clean() {
            self.state.failures = 0;
        } else {
            self.bump();
        }
        report
    }

    /// Step, sleep, repeat, until the master goes or someone asks it to stop.
    pub fn run(&mut self) -> RunSummary {
        let mut summary = RunSummary {
            reason: StopReason::Shutdown,
            steps: 0,
            applied: 0,
            offered: 0,
            auto: 0,
        };
        loop {
            let report = self.step();
            summary.steps += 1;
            summary.applied += report.applied.len();
            summary.offered += report.offered.len();
            summary.auto += report.auto.len();
            for problem in &report.errors {
                (self.reporter)(problem);
            }
            if report.master_gone && self.config.oneshot {
                summary.reason = StopReason::MasterGone;
                return summary;
            }
            if self.stopping() {
                return summary;
            }
            self.clock.sleep(self.delay());
            if self.stopping() {
                return summary;
            }
        }
    }

    fn stopping(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    fn bump(&mut self) {
        self.state.failures = self.state.failures.saturating_add(1);
    }

    // -- requests ----------------------------------------------------------

    fn apply_requests(&mut self, now: u64, report: &mut StepReport) {
        // Nothing may be applied before the agent knows what a previous one
        // already applied. Guessing zero here is how a restart re-opens a port
        // the user stopped a minute ago; failing the pass instead costs one
        // poll interval.
        if !self.state.mark_read {
            match self.requests.settled_mark() {
                Ok(mark) => {
                    self.state.last_applied_id = self.state.last_applied_id.max(mark);
                    self.state.mark_read = true;
                }
                Err(err) => {
                    report
                        .errors
                        .push(format!("could not read the applied-request mark: {err}"));
                    return;
                }
            }
        }

        let pending = match self.requests.pending() {
            Ok(pending) => pending,
            Err(err) => {
                report
                    .errors
                    .push(format!("could not read requests: {err}"));
                return;
            }
        };

        // The cursor may only advance over an unbroken run of settled
        // requests: a forward that failed has to be retried next pass, and
        // moving past it would drop it silently.
        let mut cursor = self.state.last_applied_id;
        let mut stalled = false;
        for request in pending {
            if request.id <= self.state.last_applied_id {
                continue;
            }
            if self.handle_request(&request, now, report) {
                if !stalled {
                    cursor = request.id;
                }
            } else {
                stalled = true;
            }
        }
        if cursor > self.state.last_applied_id {
            self.state.last_applied_id = cursor;
            // Told to the writer, not just to this process: it is what stops
            // `list` and `status` on the other machine reporting an applied
            // request as still waiting, and what a restarted agent reads back.
            if let Err(err) = self.requests.settled(cursor) {
                report.errors.push(format!(
                    "could not record request {cursor} as applied: {err}"
                ));
            }
        }
    }

    /// Returns whether the request is settled — applied, ignored, or expired.
    /// `false` means "try again next pass".
    fn handle_request(&mut self, request: &Request, now: u64, report: &mut StepReport) -> bool {
        if !self.is_ours(request) {
            return true;
        }
        if request.is_expired(now, self.config.ttl_ms) {
            report.expired.push(request.local_port);
            return true;
        }

        let mut spec = request.to_spec();
        // The remote knows the machine by whatever name its own config uses;
        // records on this side are keyed by ours.
        spec.host = self.config.host.name.clone();
        // The request document is written on the far side, so its `remote_bind`
        // is the remote's choice of where our tunnel terminates. Anything but
        // loopback would let a compromised remote point 127.0.0.1:<port> on this
        // machine at a third host — a cloud metadata service, another server on
        // its network. v1 forwards to loopback and nothing else, so the far end
        // is decided here rather than read off the wire.
        if let Some(clamped) = clamp_remote_bind(&spec.remote_bind) {
            (self.reporter)(&format!(
                "request {} asked to forward to {:?} on {}; \
                 v1 forwards to loopback only, using {clamped}",
                request.id, spec.remote_bind, self.config.host.name
            ));
            spec.remote_bind = clamped.to_string();
        }

        // A record is keyed by its local port, so one at `spec.local_port` is
        // not necessarily this request's: `add 3000 --local-port 5173` collides
        // with a forward of remote 5173. Only a record describing the same
        // forward answers the request — the same test `Actions::apply_or_request`
        // applies when it meets this collision on this machine.
        match self.store.get(&spec.host, spec.local_port) {
            Ok(Some(existing))
                if existing.spec.remote_port == spec.remote_port
                    && existing.spec.remote_bind == spec.remote_bind =>
            {
                // A record is not a forward. One over a local port nothing is
                // listening on is stale by definition — the master it lived on
                // went away, or it was cancelled behind the tool's back — and
                // invariant 3 says it must not shadow the port. Believing it
                // here settles the request with no tunnel behind it and, worse,
                // hands the browser a URL over a dead port. So the record only
                // answers the request while the port it claims is really bound;
                // otherwise this falls through and the forward is made below,
                // exactly as `Actions::apply_or_request_opening` treats a record
                // over a free port on this side.
                if self.ports.in_use(spec.local_port) {
                    // The port is already forwarded, but the request may still
                    // say something the record does not: an explicit `add` over
                    // a forward this agent opened on its own is the user pinning
                    // it, and a record that goes on saying otherwise loses the
                    // port when the pane that happened to open it exits.
                    if let Some(promoted) = crate::state::promoted(&existing, &spec) {
                        if let Err(err) = self.store.upsert(promoted) {
                            report.errors.push(err.to_string());
                            return false;
                        }
                    }
                    self.state.handled.insert(spec.remote_port);
                    // The forward this request asked for is in place — made by
                    // this agent on this master, whether a moment ago or on an
                    // earlier pass — so the click that carried the URL has been
                    // honoured and the browser can have it. Not opening here
                    // would leave the second Ctrl+click on a port that is
                    // already forwarded doing nothing at all, which is the bug
                    // this whole path exists for.
                    self.open_requested_url(request, &spec, report);
                    return true;
                }
            }
            Ok(Some(existing)) => {
                // Someone else's port. Told to the user and left unsettled, so
                // it is retried while the other forward might still go away and
                // then expires like any other request nobody could apply —
                // rather than reported as forwarded, which is what matching on
                // the local port alone made of it.
                let problem = format!(
                    "local port {} already forwards {} port {}",
                    spec.local_port, existing.spec.host, existing.spec.remote_port
                );
                self.report_failure(spec.local_port, &problem);
                report.errors.push(problem);
                return false;
            }
            Ok(None) => {}
            Err(err) => {
                report.errors.push(err.to_string());
                return false;
            }
        }

        match self.forward(&spec, now) {
            Ok(()) => {
                report.applied.push(spec.local_port);
                self.state.handled.insert(spec.remote_port);
                self.state.reported.remove(&spec.local_port);
                self.open_requested_url(request, &spec, report);
                true
            }
            Err(problem) => {
                self.report_failure(spec.local_port, &problem);
                report.errors.push(problem);
                false
            }
        }
    }

    /// Hand the URL a request carried to a browser on this machine, now that
    /// its forward is live.
    ///
    /// Called only where the request was settled as *applied*: a request this
    /// agent skipped as somebody else's, expired, or could not apply has no
    /// forward behind it, and a URL pointing at a port nothing is listening on
    /// is a browser error page the user did not ask for. A record claiming the
    /// port is not enough either — both callers have established that the
    /// forward is really live before they get here.
    ///
    /// Nothing about the string is trusted. It was written by a plugin on the
    /// remote host, into a file on the remote host, and read back over `ssh`, so
    /// it is re-parsed here by the same loopback validator the plugin used and
    /// then has to name the very local port this agent bound. That is invariant
    /// 11's reasoning at the other end of the click: a remote decides *that* a
    /// forward is wanted, never where it goes — and never, through a URL, what
    /// this machine's browser visits. What reaches the opener is finally
    /// re-rendered from the validated parts rather than passed through, and goes
    /// as one argv word (invariant 9).
    fn open_requested_url(
        &mut self,
        request: &Request,
        spec: &ForwardSpec,
        report: &mut StepReport,
    ) {
        let Some(raw) = request.open_url.as_deref() else {
            return;
        };
        let parsed = match parse_loopback_url(raw) {
            Ok(parsed) => parsed,
            Err(why) => {
                report.errors.push(format!(
                    "request {} carried a URL this machine will not open: {why}",
                    request.id
                ));
                return;
            }
        };
        if parsed.port != spec.local_port {
            report.errors.push(format!(
                "request {} forwarded local port {} but asked to open port {}; \
                 not opening it",
                request.id, spec.local_port, parsed.port
            ));
            return;
        }
        let url = parsed.with_port(spec.local_port);
        // Once per *request*, not once per URL. The queue is re-read whole on
        // every pass and a request the mark has not settled yet — one held
        // behind an earlier request that keeps failing — comes back with it, so
        // without this a single click becomes a window a poll. A later request
        // carrying the same URL is a later click, and gets its own window.
        if self.state.opened.contains(&request.id) {
            return;
        }
        if let Err(problem) = self.opener.open(&url) {
            // Not recorded as opened: nothing was, and the next pass over this
            // request deserves another try.
            report
                .errors
                .push(format!("could not open {url}: {problem}"));
            return;
        }
        if self.state.opened.len() >= MAX_REMEMBERED_OPENS {
            self.state.opened.remove(0);
        }
        self.state.opened.push(request.id);
        report.opened.push(url);
    }

    /// A request names the host it is for. The remote plugin's name for the
    /// machine and ours can differ, so the target counts too, and an unnamed
    /// host means "wherever this is applied".
    ///
    /// One that is not ours counts as settled, and so gets marked applied along
    /// with the rest. That is not this agent stealing it: the queue belongs to
    /// the machine this agent polls, nothing else reads it, and a request in it
    /// naming a third host can only ever sit there. Leaving it unsettled would
    /// stall the mark behind it and keep every later request pending forever in
    /// the eyes of the process that wrote them.
    fn is_ours(&self, request: &Request) -> bool {
        request.host.is_empty()
            || request.host == self.config.host.name
            || request.host == self.config.host.target
    }

    /// Tell the user once that a port they asked for could not be opened.
    /// Silence after a click on a URL reads as the plugin being broken; a
    /// notification per poll reads as worse.
    fn report_failure(&mut self, port: u16, problem: &str) {
        (self.reporter)(problem);
        if !self.state.reported.insert(port) {
            return;
        }
        let notification = Notification {
            title: format!("Port {port} not forwarded"),
            body: format!("{}: {problem}", self.config.host.name),
        };
        if let Err(err) = self.herdr.notify(&notification) {
            (self.reporter)(&format!("could not report the failed forward: {err}"));
        }
    }

    fn forward(&self, spec: &ForwardSpec, now: u64) -> std::result::Result<(), String> {
        self.ssh
            .add_forward(&self.config.host.target, &self.config.control_path, spec)
            .map_err(|err| err.to_string())?;
        self.store
            .upsert(Forward {
                spec: spec.clone(),
                created_unix_ms: now,
            })
            .map_err(|err| err.to_string())?;
        Ok(())
    }

    // -- discovery ---------------------------------------------------------

    fn discover(&mut self, now: u64, report: &mut StepReport) {
        let listeners = match self.listeners() {
            Ok(listeners) => listeners,
            Err(problem) => {
                report.errors.push(problem);
                return;
            }
        };
        let tree = match self.exec(discover::PS_TREE_COMMAND) {
            Ok(stdout) => discover::parse_ps(&stdout),
            Err(problem) => {
                report.errors.push(problem);
                return;
            }
        };
        let panes = match self.panes() {
            Ok(panes) => panes,
            Err(problem) => {
                report.errors.push(problem);
                return;
            }
        };

        let candidates = discover::attribute(&listeners, &tree, &panes);
        // A port that went away may be a different server when it comes back,
        // so it becomes offerable again. A port that is still there stays
        // marked, which is what stops the notification spam.
        let live: HashSet<u16> = candidates
            .iter()
            .map(|candidate| candidate.listener.port)
            .collect();
        self.state.handled.retain(|port| live.contains(port));

        for candidate in &candidates {
            self.consider(candidate, now, report);
        }
    }

    /// Decide what to do about one attributed listener. Per invariant 7 the
    /// only path from here to a forward is an allowlist match; everything else
    /// is an offer the user has to accept.
    fn consider(&mut self, candidate: &AttributedListener, now: u64, report: &mut StepReport) {
        let port = candidate.listener.port;
        if !candidate.listener.is_forwardable() {
            return;
        }
        if self.config.settings.require_pane_attribution && candidate.pane.is_none() {
            return;
        }
        if self.state.handled.contains(&port) {
            return;
        }
        // By the *remote* port: `port` is what the far side is listening on,
        // and a record is keyed by the local port it was given here. Asking for
        // a record at the local port `port` misses every remapped forward, and
        // the listener then reads as unforwarded — offered again, and with an
        // `[[auto]]` rule opened a second time alongside the remap.
        match self.store.get_remote(&self.config.host.name, port) {
            Ok(Some(_)) => {
                self.state.handled.insert(port);
                return;
            }
            Ok(None) => {}
            Err(err) => {
                report.errors.push(err.to_string());
                return;
            }
        }

        if self.auto_allows(candidate, port) {
            let spec = ForwardSpec {
                host: self.config.host.name.clone(),
                remote_port: port,
                local_port: port,
                remote_bind: remote_bind_for(&candidate.listener),
                origin: Origin::Auto,
                pane_id: candidate.pane.as_ref().map(|pane| pane.pane_id.clone()),
            };
            match self.forward(&spec, now) {
                Ok(()) => {
                    report.auto.push(port);
                    self.state.handled.insert(port);
                    self.state.reported.remove(&port);
                }
                Err(problem) => {
                    self.report_failure(port, &problem);
                    report.errors.push(problem);
                }
            }
            return;
        }

        if !self.config.settings.offer_new_listeners {
            return;
        }
        let notification = offer_notification(&self.config.host.name, candidate);
        match self.herdr.notify(&notification) {
            Ok(()) => {
                report.offered.push(port);
                self.state.handled.insert(port);
            }
            Err(err) => report
                .errors
                .push(format!("could not offer port {port}: {err}")),
        }
    }

    /// An allowlist is declared per workspace, so a listener nobody can
    /// attribute can never match one.
    fn auto_allows(&self, candidate: &AttributedListener, port: u16) -> bool {
        let Some(pane) = candidate.pane.as_ref() else {
            return false;
        };
        if self.config.settings.auto_allows(&pane.workspace_id, port) {
            return true;
        }
        pane.workspace_label
            .as_deref()
            .map(|label| self.config.settings.auto_allows(label, port))
            .unwrap_or(false)
    }

    /// `lsof` on macOS, `ss` on Linux, decided by asking rather than by
    /// guessing from a `uname` this crate would then have to parse.
    fn listeners(&mut self) -> std::result::Result<Vec<Listener>, String> {
        let order: [Probe; 2] = match self.state.probe {
            Probe::Ss => [Probe::Ss, Probe::Lsof],
            _ => [Probe::Lsof, Probe::Ss],
        };
        let mut last = String::from("no listener command succeeded");
        for probe in order {
            let command = match probe {
                Probe::Lsof => discover::LSOF_LISTENERS_COMMAND,
                Probe::Ss => discover::SS_LISTENERS_COMMAND,
                Probe::Unknown => continue,
            };
            match self.exec(command) {
                Ok(stdout) => {
                    self.state.probe = probe;
                    return Ok(match probe {
                        Probe::Ss => discover::parse_ss(&stdout),
                        _ => discover::parse_lsof(&stdout),
                    });
                }
                Err(problem) => last = problem,
            }
        }
        Err(last)
    }

    fn panes(&self) -> std::result::Result<Vec<PaneRef>, String> {
        // Labelled: an allowlist is written per workspace, and the name a user
        // has in front of them is the label, not the id. The plain listing
        // carries no label, so matching one against it never fires.
        let mut panes = self
            .herdr
            .pane_list_labelled()
            .map_err(|err| format!("could not list panes: {err}"))?;
        for pane in &mut panes {
            if pane.shell_pid.is_some() {
                continue;
            }
            // One unreadable pane is not a broken pass: attribution simply
            // cannot reach that pane this time.
            match self.herdr.shell_pid(&pane.pane_id) {
                Ok(shell_pid) => pane.shell_pid = shell_pid,
                Err(err) => (self.reporter)(&format!(
                    "could not read the shell pid for pane {}: {err}",
                    pane.pane_id
                )),
            }
        }
        Ok(panes)
    }

    fn exec(&self, command: &str) -> std::result::Result<String, String> {
        self.ssh
            .exec(&self.config.host.target, &self.config.control_path, command)
            .map_err(|err| err.to_string())
    }
}

/// The loopback address to substitute for a remote-supplied `remote_bind`, or
/// `None` when the record already names loopback and can be used as written.
///
/// Pure and total: every value that is not exactly one of the two loopback
/// spellings — a routable address, a name, an empty string, a bracketed form
/// this crate never writes — comes back as v4 loopback rather than being
/// refused, because the user did ask for a forward and loopback is what v1
/// means by one.
pub fn clamp_remote_bind(remote_bind: &str) -> Option<&'static str> {
    match remote_bind {
        LOOPBACK_V4 | LOOPBACK_V6 => None,
        _ => Some(LOOPBACK_V4),
    }
}

/// Remote side of the forward for a detected listener.
///
/// A server that bound v6 loopback only does not answer on `127.0.0.1`, and a
/// forward to the wrong family fails at connect time with nothing useful in it.
fn remote_bind_for(listener: &Listener) -> String {
    if listener.bind == LOOPBACK_V6 {
        LOOPBACK_V6.to_string()
    } else {
        LOOPBACK_V4.to_string()
    }
}

/// The L2 offer. Pure, so the wording is testable and the notification carries
/// what the user needs to decide: which port, what opened it, and which pane it
/// belongs to.
pub fn offer_notification(host: &str, candidate: &AttributedListener) -> Notification {
    let port = candidate.listener.port;
    let owner = match candidate.pane.as_ref() {
        Some(pane) => match pane.agent.as_deref() {
            Some(agent) => format!("{agent} in pane {}", pane.pane_id),
            None => format!("pane {}", pane.pane_id),
        },
        None => "no pane".to_string(),
    };
    Notification {
        title: format!("Port {port} on {host}"),
        body: format!(
            "{} is listening ({owner}). Forward it with: herdr-portfwd add {port}",
            candidate.listener.process
        ),
    }
}

// ---------------------------------------------------------------------------
// cleanup
// ---------------------------------------------------------------------------

/// A teardown that runs at most once, however the attach ends.
///
/// Held behind an `Arc` because three different things have to be able to
/// trigger it — a scope guard, a signal, and the panic hook — and none of them
/// may run it twice.
pub struct Cleanup {
    action: Box<dyn Fn() + Send + Sync>,
    done: AtomicBool,
}

impl Cleanup {
    pub fn new(action: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            action: Box::new(action),
            done: AtomicBool::new(false),
        })
    }

    /// Run the teardown unless it already ran. Returns whether this call is the
    /// one that did the work.
    pub fn run(&self) -> bool {
        if self.done.swap(true, Ordering::SeqCst) {
            return false;
        }
        (self.action)();
        true
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
}

/// Runs a [`Cleanup`] when the scope ends, including while unwinding.
pub struct CleanupGuard {
    cleanup: Arc<Cleanup>,
}

impl CleanupGuard {
    pub fn new(cleanup: Arc<Cleanup>) -> Self {
        Self { cleanup }
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        self.cleanup.run();
    }
}

/// Run the cleanup from the panic hook as well as from the guard.
///
/// Not redundant: release builds are `panic = "abort"`, where no destructor
/// runs at all. The hook fires before the abort, and is the only thing standing
/// between a panic and a master left holding live forwards.
pub fn install_panic_cleanup(cleanup: Arc<Cleanup>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        cleanup.run();
        previous(info);
    }));
}

/// Signals the attach supervisor watches for.
pub trait Interrupts {
    /// The signal that arrived, if one has.
    fn pending(&self) -> Option<i32>;
}

static SIGNALLED: AtomicI32 = AtomicI32::new(0);

extern "C" fn note_signal(signal: libc::c_int) {
    // The only async-signal-safe thing worth doing here: record it and let the
    // supervisor loop notice.
    SIGNALLED.store(signal, Ordering::SeqCst);
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessSignals;

impl Interrupts for ProcessSignals {
    fn pending(&self) -> Option<i32> {
        match SIGNALLED.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }
}

/// Catch the signals that end a session so the master is torn down rather than
/// orphaned. SIGINT usually reaches the client too — it shares the terminal's
/// foreground group — and this is about the case where it does not.
pub fn install_signal_handlers() -> ProcessSignals {
    // Through a function pointer, not straight from the function item: a direct
    // item-to-integer cast is a hard error on current rustc, and the pointer is
    // what `sighandler_t` has always meant anyway.
    let handler = note_signal as extern "C" fn(libc::c_int);
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // Safe: the handler only stores into a static atomic.
        unsafe {
            libc::signal(signal, handler as libc::sighandler_t);
        }
    }
    ProcessSignals
}

// ---------------------------------------------------------------------------
// process supervision
// ---------------------------------------------------------------------------

pub trait ChildProcess {
    /// `None` while it is still running.
    fn try_wait(&mut self) -> std::io::Result<Option<i32>>;
    /// Ask it to stop. Politely: a terminal client needs to restore the screen.
    fn terminate(&mut self) -> std::io::Result<()>;
}

pub trait Launcher {
    type Child: ChildProcess;

    /// Spawn on the inherited terminal. This is the process the user sees.
    fn spawn_interactive(&self, program: &Path, argv: &[String]) -> std::io::Result<Self::Child>;

    /// Spawn with output redirected to `log`. A background process that wrote
    /// to the inherited terminal would draw over the Herdr TUI.
    fn spawn_background(
        &self,
        program: &Path,
        argv: &[String],
        log: &Path,
    ) -> std::io::Result<Self::Child>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemLauncher;

pub struct SystemChild(std::process::Child);

impl ChildProcess for SystemChild {
    fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        Ok(self.0.try_wait()?.map(exit_code))
    }

    fn terminate(&mut self) -> std::io::Result<()> {
        let pid = self.0.id();
        let pid = libc::pid_t::try_from(pid).unwrap_or(0);
        if pid > 0 {
            // Safe: kill on a pid this process spawned and has not reaped.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        Ok(())
    }
}

/// A signalled child has no exit code of its own; the shell convention makes
/// one out of the signal number so the status is still reportable.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

impl Launcher for SystemLauncher {
    type Child = SystemChild;

    fn spawn_interactive(&self, program: &Path, argv: &[String]) -> std::io::Result<Self::Child> {
        std::process::Command::new(program)
            .args(argv)
            .spawn()
            .map(SystemChild)
    }

    fn spawn_background(
        &self,
        program: &Path,
        argv: &[String],
        log: &Path,
    ) -> std::io::Result<Self::Child> {
        let file = std::fs::File::create(log)?;
        let errors = file.try_clone()?;
        std::process::Command::new(program)
            .args(argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(file))
            .stderr(std::process::Stdio::from(errors))
            .spawn()
            .map(SystemChild)
    }
}

// ---------------------------------------------------------------------------
// attach
// ---------------------------------------------------------------------------

/// Everything `attach` needs, resolved before anything is spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachPlan {
    pub host: Host,
    pub control_path: PathBuf,
    /// The Herdr client to hand the terminal to.
    pub herdr_bin: PathBuf,
    /// Arguments the user wants passed through to it.
    pub herdr_args: Vec<String>,
    /// This binary, re-invoked as the polling agent.
    pub agent_bin: PathBuf,
    pub agent_log: PathBuf,
}

/// `herdr --remote <target> <extra...>`.
pub fn herdr_remote_argv(target: &str, extra: &[String]) -> Vec<String> {
    let mut argv = vec!["--remote".to_string(), target.to_string()];
    argv.extend(extra.iter().cloned());
    argv
}

/// `herdr-portfwd local-agent --oneshot -- <host>`.
///
/// The flag comes first and the host after `--` so a host name that starts with
/// a dash is a host name, not an option.
pub fn local_agent_argv(host: &str) -> Vec<String> {
    vec![
        "local-agent".to_string(),
        "--oneshot".to_string(),
        "--".to_string(),
        host.to_string(),
    ]
}

/// Where the agent's own output goes: next to the control socket, one file per
/// host, so a session that misbehaves leaves something to read.
pub fn agent_log_path(control_path: &Path) -> PathBuf {
    let mut log = control_path.as_os_str().to_os_string();
    log.push(".agent.log");
    PathBuf::from(log)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachOutcome {
    /// Exit status of the Herdr client.
    pub status: i32,
    /// Signal that ended the attach, when one did.
    pub signal: Option<i32>,
    pub agent_started: bool,
}

/// Supervises one attach: master up, agent up, client on the terminal, and
/// everything torn down afterwards.
pub struct Attach<S, L, C, I> {
    ssh: S,
    launcher: L,
    clock: C,
    interrupts: I,
    tick: Duration,
    reporter: Reporter,
}

impl<S, L, C, I> Attach<S, L, C, I>
where
    S: SshRunner,
    L: Launcher,
    C: Clock,
    I: Interrupts,
{
    pub fn new(ssh: S, launcher: L, clock: C, interrupts: I) -> Self {
        Self {
            ssh,
            launcher,
            clock,
            interrupts,
            tick: SUPERVISE_TICK,
            reporter: stderr_reporter(),
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    pub fn with_reporter(mut self, reporter: Reporter) -> Self {
        self.reporter = reporter;
        self
    }

    pub fn run(&self, plan: &AttachPlan, cleanup: &Arc<Cleanup>) -> Result<AttachOutcome> {
        // Invariant 8: check the socket path before anything can try to bind it.
        check_control_path(&plan.control_path)?;
        self.ssh
            .ensure_master(&plan.host.target, &plan.control_path)?;

        // From here the master exists, so every exit from this function has to
        // go through the cleanup.
        let _guard = CleanupGuard::new(Arc::clone(cleanup));

        let mut agent = match self.launcher.spawn_background(
            &plan.agent_bin,
            &local_agent_argv(&plan.host.name),
            &plan.agent_log,
        ) {
            Ok(child) => Some(child),
            Err(err) => {
                // An agent that will not start costs the user automatic
                // forwarding, not their session.
                (self.reporter)(&format!("could not start the port-forward agent: {err}"));
                None
            }
        };
        let agent_started = agent.is_some();

        let outcome = self.supervise(plan);

        // Stop the agent before the master goes: an agent still polling could
        // otherwise add a forward to a master that is being torn down.
        if let Some(child) = agent.as_mut() {
            if let Err(err) = child.terminate() {
                (self.reporter)(&format!("could not stop the port-forward agent: {err}"));
            }
        }

        outcome.map(|(status, signal)| AttachOutcome {
            status,
            signal,
            agent_started,
        })
    }

    /// Run the client and wait for it, watching for a signal in between.
    fn supervise(&self, plan: &AttachPlan) -> Result<(i32, Option<i32>)> {
        let argv = herdr_remote_argv(&plan.host.target, &plan.herdr_args);
        let mut session = self
            .launcher
            .spawn_interactive(&plan.herdr_bin, &argv)
            .map_err(|source| AgentError::Spawn {
                program: plan.herdr_bin.display().to_string(),
                source,
            })?;

        let mut signal = None;
        let mut grace = TERMINATE_GRACE_TICKS;
        loop {
            match session.try_wait() {
                Ok(Some(status)) => return Ok((status, signal)),
                Ok(None) => {}
                Err(source) => {
                    return Err(AgentError::Spawn {
                        program: plan.herdr_bin.display().to_string(),
                        source,
                    })
                }
            }
            if signal.is_none() {
                if let Some(caught) = self.interrupts.pending() {
                    signal = Some(caught);
                    if let Err(err) = session.terminate() {
                        (self.reporter)(&format!("could not stop the herdr client: {err}"));
                    }
                }
            } else {
                // A client that ignores the signal must not keep the master
                // alive indefinitely: stop waiting and tear down anyway.
                grace = grace.saturating_sub(1);
                if grace == 0 {
                    return Ok((128 + signal.unwrap_or(0), signal));
                }
            }
            self.clock.sleep(self.tick);
        }
    }
}

fn check_control_path(path: &Path) -> Result<()> {
    let len = path.as_os_str().len();
    if len > MAX_CONTROL_PATH_LEN {
        return Err(SshError::ControlPathTooLong {
            path: path.to_string_lossy().into_owned(),
            len,
            max: MAX_CONTROL_PATH_LEN,
        }
        .into());
    }
    Ok(())
}

/// Cancel every recorded forward for `host`, stop the master, and forget the
/// records that are provably gone.
///
/// A record is only dropped once its port is provably not forwarded any more —
/// either its own cancel succeeded or the whole master exited. Anything else
/// stays, so `reconcile` can find a forward this teardown failed to remove
/// instead of it becoming an untracked leak.
pub fn tear_down<S: SshRunner>(
    ssh: &S,
    host: &Host,
    control_path: &Path,
    store: &Store,
) -> Vec<String> {
    let mut problems = Vec::new();
    let records = match store.list_host(&host.name) {
        Ok(records) => records,
        Err(err) => {
            problems.push(err.to_string());
            Vec::new()
        }
    };

    let mut cancelled = Vec::new();
    for record in &records {
        match ssh.cancel_forward(&host.target, control_path, &record.spec) {
            Ok(()) => cancelled.push(record.spec.local_port),
            Err(err) => problems.push(err.to_string()),
        }
    }

    let master_gone = match ssh.exit_master(&host.target, control_path) {
        Ok(()) => true,
        Err(err) => {
            problems.push(err.to_string());
            false
        }
    };

    if master_gone {
        if let Err(err) = store.remove_host(&host.name) {
            problems.push(err.to_string());
        }
        return problems;
    }
    for port in cancelled {
        if let Err(err) = store.remove(&host.name, port) {
            problems.push(err.to_string());
        }
    }
    problems
}

/// The teardown as a [`Cleanup`], ready to hand to [`Attach::run`] and to
/// [`install_panic_cleanup`].
pub fn teardown_cleanup<S>(
    ssh: Arc<S>,
    host: Host,
    control_path: PathBuf,
    store: Arc<Store>,
    reporter: Reporter,
) -> Arc<Cleanup>
where
    S: SshRunner + Send + Sync + 'static,
{
    Cleanup::new(move || {
        for problem in tear_down(ssh.as_ref(), &host, &control_path, store.as_ref()) {
            reporter(&problem);
        }
    })
}

// ---------------------------------------------------------------------------
// wiring
// ---------------------------------------------------------------------------

/// Build the plan `attach` runs from. Separated from [`Attach::run`] so the
/// paths are decided — and can be inspected — before a process exists.
pub fn attach_plan(host: Host, herdr_args: Vec<String>) -> Result<AttachPlan> {
    let control_path =
        crate::ssh::control_socket_path(&crate::ssh::control_socket_dir()?, &host.name)?;
    let agent_bin = std::env::current_exe().map_err(|source| AgentError::Spawn {
        program: "herdr-portfwd".to_string(),
        source,
    })?;
    Ok(AttachPlan {
        agent_log: agent_log_path(&control_path),
        host,
        control_path,
        herdr_bin: PathBuf::from(crate::herdr::herdr_bin()),
        herdr_args,
        agent_bin,
    })
}

/// `herdr-portfwd attach <host> [...]`, wired to the real world.
///
/// The cleanup is installed in the panic hook as well as guarded by scope,
/// because a release build aborts on panic and runs no destructors.
pub fn attach_main(host: Host, herdr_args: Vec<String>) -> Result<AttachOutcome> {
    let plan = attach_plan(host, herdr_args)?;
    let ssh = Arc::new(crate::ssh_process::SshProcess::new());
    let store = Arc::new(Store::from_env()?);
    let cleanup = teardown_cleanup(
        Arc::clone(&ssh),
        plan.host.clone(),
        plan.control_path.clone(),
        Arc::clone(&store),
        stderr_reporter(),
    );
    install_panic_cleanup(Arc::clone(&cleanup));
    let interrupts = install_signal_handlers();
    let attach = Attach::new(SharedSsh(ssh), SystemLauncher, SystemClock, interrupts);
    attach.run(&plan, &cleanup)
}

/// `herdr-portfwd local-agent <host>`, wired to the real world.
///
/// Both the Herdr client and the request source go over the same master this
/// agent's forwards live on: under `--remote` the server the user is looking at
/// is the remote one, so that is where a notification has to be shown.
pub fn local_agent_main(host: Host, oneshot: bool) -> Result<RunSummary> {
    let control_path =
        crate::ssh::control_socket_path(&crate::ssh::control_socket_dir()?, &host.name)?;
    let ssh = SharedSsh::new(crate::ssh_process::SshProcess::new());
    let herdr =
        crate::herdr_cli::HerdrCli::remote(ssh.clone(), host.target.clone(), control_path.clone());
    let requests = RemoteRequests::new(ssh.clone(), host.target.clone(), control_path.clone());
    let settings = crate::config::load_settings(&crate::config::settings_file())?;
    let config = AgentConfig {
        host,
        control_path,
        settings,
        oneshot,
        ttl_ms: DEFAULT_TTL_MS,
    };
    let mut agent = Agent::new(
        ssh,
        herdr,
        requests,
        SystemClock,
        Store::from_env()?,
        config,
    );
    let shutdown = agent.shutdown_flag();
    let signals = install_signal_handlers();
    // The loop checks this between passes, so a signal ends the agent after the
    // pass in flight rather than in the middle of applying a forward.
    let watcher = std::thread::spawn(move || {
        while signals.pending().is_none() {
            std::thread::sleep(SUPERVISE_TICK);
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
        }
        shutdown.store(true, Ordering::SeqCst);
    });
    let summary = agent.run();
    agent.shutdown_flag().store(true, Ordering::SeqCst);
    let _ = watcher.join();
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Mutex, MutexGuard};

    use crate::config::AutoRule;

    // -- fakes -------------------------------------------------------------

    fn ssh_failure(operation: &'static str, why: &str) -> SshError {
        SshError::Command {
            operation,
            target: "buildbox.example".to_string(),
            code: 255,
            stderr: why.to_string(),
        }
    }

    struct SshState {
        master: MasterState,
        check_error: Option<String>,
        ensure_error: Option<String>,
        forward_error: Option<String>,
        cancel_error: Option<String>,
        exit_error: Option<String>,
        ensured: Vec<String>,
        forwards: Vec<String>,
        cancels: Vec<String>,
        /// Local ports this master currently holds. A real `ssh -L` binds the
        /// port, so the fake master is also where a local-port probe gets its
        /// answer; nothing in the suite may bind a socket for real.
        bound: HashSet<u16>,
        exits: usize,
        execs: Vec<String>,
        responses: HashMap<String, std::result::Result<String, String>>,
    }

    impl Default for SshState {
        fn default() -> Self {
            Self {
                master: MasterState::Running,
                check_error: None,
                ensure_error: None,
                forward_error: None,
                cancel_error: None,
                exit_error: None,
                ensured: Vec::new(),
                forwards: Vec::new(),
                cancels: Vec::new(),
                bound: HashSet::new(),
                exits: 0,
                execs: Vec::new(),
                responses: HashMap::new(),
            }
        }
    }

    #[derive(Clone, Default)]
    struct FakeSsh {
        state: Arc<Mutex<SshState>>,
    }

    impl FakeSsh {
        fn lock(&self) -> MutexGuard<'_, SshState> {
            self.state.lock().expect("ssh state")
        }

        fn answer(&self, command: &str, stdout: &str) {
            self.lock()
                .responses
                .insert(command.to_string(), Ok(stdout.to_string()));
        }

        fn refuse(&self, command: &str, why: &str) {
            self.lock()
                .responses
                .insert(command.to_string(), Err(why.to_string()));
        }

        /// Hold a local port without a forward having been asked for here: what
        /// a test seeding a record of a forward that really is live needs, since
        /// the probe answers from this master's own bindings.
        fn bind(&self, port: u16) {
            self.lock().bound.insert(port);
        }
    }

    /// The probe the agent asks whether a local port is bound. Answering from
    /// the fake master keeps the two in step: a forward binds its local port and
    /// a cancel releases it, so a record with no forward behind it — the stale
    /// record invariant 3 is about — is exactly a record over a free port.
    impl LocalPorts for FakeSsh {
        fn in_use(&self, port: u16) -> bool {
            self.lock().bound.contains(&port)
        }
    }

    impl SshRunner for FakeSsh {
        fn ensure_master(&self, target: &str, _path: &Path) -> crate::ssh::Result<()> {
            let mut state = self.lock();
            state.ensured.push(target.to_string());
            match state.ensure_error.clone() {
                Some(why) => Err(ssh_failure("start the master", &why)),
                None => Ok(()),
            }
        }

        fn check_master(&self, _target: &str, _path: &Path) -> crate::ssh::Result<MasterState> {
            let state = self.lock();
            match state.check_error.clone() {
                Some(why) => Err(ssh_failure("check", &why)),
                None => Ok(state.master),
            }
        }

        fn add_forward(
            &self,
            _target: &str,
            _path: &Path,
            spec: &ForwardSpec,
        ) -> crate::ssh::Result<()> {
            let mut state = self.lock();
            match state.forward_error.clone() {
                Some(why) => Err(ssh_failure("forward", &why)),
                None => {
                    state.forwards.push(spec.forward_arg());
                    state.bound.insert(spec.local_port);
                    Ok(())
                }
            }
        }

        fn cancel_forward(
            &self,
            _target: &str,
            _path: &Path,
            spec: &ForwardSpec,
        ) -> crate::ssh::Result<()> {
            let mut state = self.lock();
            match state.cancel_error.clone() {
                Some(why) => Err(ssh_failure("cancel", &why)),
                None => {
                    state.cancels.push(spec.forward_arg());
                    state.bound.remove(&spec.local_port);
                    Ok(())
                }
            }
        }

        fn exit_master(&self, _target: &str, _path: &Path) -> crate::ssh::Result<()> {
            let mut state = self.lock();
            state.exits += 1;
            match state.exit_error.clone() {
                Some(why) => Err(ssh_failure("exit", &why)),
                None => Ok(()),
            }
        }

        fn exec(&self, _target: &str, _path: &Path, command: &str) -> crate::ssh::Result<String> {
            let response = {
                let mut state = self.lock();
                state.execs.push(command.to_string());
                state.responses.get(command).cloned()
            };
            match response {
                Some(Ok(stdout)) => Ok(stdout),
                Some(Err(why)) => Err(ssh_failure("exec", &why)),
                None => Err(ssh_failure("exec", "command not found")),
            }
        }
    }

    #[derive(Default)]
    struct HerdrState {
        panes: Vec<PaneRef>,
        pids: HashMap<String, u32>,
        notifications: Vec<Notification>,
        pane_list_error: Option<String>,
        notify_error: Option<String>,
    }

    #[derive(Clone, Default)]
    struct FakeHerdr {
        state: Arc<Mutex<HerdrState>>,
    }

    impl FakeHerdr {
        fn lock(&self) -> MutexGuard<'_, HerdrState> {
            self.state.lock().expect("herdr state")
        }

        fn titles(&self) -> Vec<String> {
            self.lock()
                .notifications
                .iter()
                .map(|n| n.title.clone())
                .collect()
        }
    }

    fn herdr_failure(command: &str, why: &str) -> crate::herdr::HerdrError {
        crate::herdr::HerdrError::Command {
            command: command.to_string(),
            code: 1,
            stderr: why.to_string(),
        }
    }

    impl HerdrClient for FakeHerdr {
        fn pane_list(&self) -> crate::herdr::Result<Vec<PaneRef>> {
            let state = self.lock();
            match state.pane_list_error.clone() {
                Some(why) => Err(herdr_failure("pane list", &why)),
                // Stripped, as the real client's is: a label reaches a caller
                // only through the labelled listing.
                None => Ok(state
                    .panes
                    .iter()
                    .cloned()
                    .map(|pane| PaneRef {
                        workspace_label: None,
                        ..pane
                    })
                    .collect()),
            }
        }

        fn pane_list_labelled(&self) -> crate::herdr::Result<Vec<PaneRef>> {
            let state = self.lock();
            match state.pane_list_error.clone() {
                Some(why) => Err(herdr_failure("workspace list", &why)),
                None => Ok(state.panes.clone()),
            }
        }

        fn shell_pid(&self, pane_id: &str) -> crate::herdr::Result<Option<u32>> {
            Ok(self.lock().pids.get(pane_id).copied())
        }

        fn notify(&self, notification: &Notification) -> crate::herdr::Result<()> {
            let mut state = self.lock();
            match state.notify_error.clone() {
                Some(why) => Err(herdr_failure("notification show", &why)),
                None => {
                    state.notifications.push(notification.clone());
                    Ok(())
                }
            }
        }
    }

    #[derive(Default)]
    struct RequestState {
        pending: Vec<Request>,
        error: Option<String>,
        mark_error: Option<String>,
        reads: usize,
        /// Every id the agent said it had finished with, in order.
        settled: Vec<u64>,
        mark: u64,
    }

    #[derive(Clone, Default)]
    struct FakeRequests {
        state: Arc<Mutex<RequestState>>,
    }

    impl FakeRequests {
        fn lock(&self) -> MutexGuard<'_, RequestState> {
            self.state.lock().expect("request state")
        }

        fn set(&self, pending: Vec<Request>) {
            self.lock().pending = pending;
        }

        fn settled(&self) -> Vec<u64> {
            self.lock().settled.clone()
        }
    }

    /// Deliberately modelled on the *remote* source, the one production uses:
    /// settling records a mark and leaves the document alone, because the agent
    /// cannot rewrite a file on the far side. So a request stays in `pending`
    /// after it has been applied, and only the mark keeps it from being applied
    /// again.
    impl RequestSource for FakeRequests {
        fn pending(&self) -> Result<Vec<Request>> {
            let mut state = self.lock();
            state.reads += 1;
            match state.error.clone() {
                Some(why) => Err(AgentError::Other(why)),
                None => Ok(state.pending.clone()),
            }
        }

        fn settled_mark(&self) -> Result<u64> {
            let state = self.lock();
            match state.mark_error.clone() {
                Some(why) => Err(AgentError::Other(why)),
                None => Ok(state.mark),
            }
        }

        fn settled(&self, id: u64) -> Result<()> {
            let mut state = self.lock();
            state.settled.push(id);
            state.mark = state.mark.max(id);
            Ok(())
        }
    }

    #[derive(Default)]
    struct ClockState {
        now_ms: u64,
        sleeps: Vec<Duration>,
        stop_after: Option<usize>,
        flag: Option<Arc<AtomicBool>>,
    }

    #[derive(Clone, Default)]
    struct FakeClock {
        state: Arc<Mutex<ClockState>>,
    }

    impl FakeClock {
        fn lock(&self) -> MutexGuard<'_, ClockState> {
            self.state.lock().expect("clock state")
        }

        fn set_now(&self, now_ms: u64) {
            self.lock().now_ms = now_ms;
        }

        fn sleeps(&self) -> Vec<Duration> {
            self.lock().sleeps.clone()
        }

        /// Raise `flag` once the loop has slept `count` times, which is how a
        /// test ends a loop that would otherwise run forever.
        fn stop_after(&self, count: usize, flag: Arc<AtomicBool>) {
            let mut state = self.lock();
            state.stop_after = Some(count);
            state.flag = Some(flag);
        }
    }

    impl Clock for FakeClock {
        fn now_unix_ms(&self) -> u64 {
            self.lock().now_ms
        }

        fn sleep(&self, duration: Duration) {
            let mut state = self.lock();
            state.sleeps.push(duration);
            state.now_ms = state
                .now_ms
                .saturating_add(u64::try_from(duration.as_millis()).unwrap_or(0));
            if state.stop_after == Some(state.sleeps.len()) {
                if let Some(flag) = state.flag.as_ref() {
                    flag.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    /// A desktop opener that records instead of launching a browser. No test
    /// may ever put a window in front of whoever is running the suite.
    #[derive(Clone, Default)]
    struct FakeOpener {
        opened: Arc<Mutex<Vec<String>>>,
        error: Arc<Mutex<Option<String>>>,
    }

    impl FakeOpener {
        fn opened(&self) -> Vec<String> {
            self.opened.lock().expect("opened urls").clone()
        }

        fn refuse(&self, why: &str) {
            *self.error.lock().expect("opener error") = Some(why.to_string());
        }
    }

    impl UrlOpener for FakeOpener {
        fn open(&self, url: &str) -> std::result::Result<(), String> {
            if let Some(why) = self.error.lock().expect("opener error").clone() {
                return Err(why);
            }
            self.opened
                .lock()
                .expect("opened urls")
                .push(url.to_string());
            Ok(())
        }
    }

    fn recorder() -> (Arc<Mutex<Vec<String>>>, Reporter) {
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        (
            log,
            Arc::new(move |message: &str| {
                sink.lock().expect("warning log").push(message.to_string())
            }),
        )
    }

    // -- fixtures ----------------------------------------------------------

    fn host() -> Host {
        Host {
            name: "buildbox".to_string(),
            target: "buildbox.example".to_string(),
            prefix: Some("build".to_string()),
        }
    }

    fn spec(port: u16) -> ForwardSpec {
        ForwardSpec {
            host: "buildbox".to_string(),
            remote_port: port,
            local_port: port,
            remote_bind: LOOPBACK_V4.to_string(),
            origin: Origin::Explicit,
            pane_id: Some("w1:p1".to_string()),
        }
    }

    fn request(id: u64, port: u16) -> Request {
        Request::new(id, 1_000, spec(port))
    }

    fn pane(pane_id: &str, shell_pid: u32) -> PaneRef {
        PaneRef {
            pane_id: pane_id.to_string(),
            workspace_id: "w1".to_string(),
            workspace_label: Some("build: scholion".to_string()),
            agent: Some("claude".to_string()),
            shell_pid: Some(shell_pid),
        }
    }

    /// One listening node under pid 900, whose parent chain reaches pid 700.
    const LSOF: &str = "p900\ncnode\nn*:5173\n";
    const PS: &str = "900 800\n800 700\n700 1\n";
    const SS: &str = "LISTEN 0 511 127.0.0.1:5173 0.0.0.0:* users:((\"node\",pid=900,fd=20))\n";

    struct Harness {
        dir: tempfile::TempDir,
        ssh: FakeSsh,
        herdr: FakeHerdr,
        requests: FakeRequests,
        clock: FakeClock,
        warnings: Arc<Mutex<Vec<String>>>,
        reporter: Reporter,
        opener: FakeOpener,
        config: AgentConfig,
        agent: Agent<FakeSsh, FakeHerdr, FakeRequests, FakeClock>,
        store: Store,
    }

    impl Harness {
        /// Discovery off: most tests are about the request path, and a
        /// discovery pass with no canned command output would add noise.
        fn new() -> Self {
            Self::with(Settings {
                poll_seconds: 0,
                ..Settings::default()
            })
        }

        /// Discovery on, with one node listener to find.
        fn discovering() -> Self {
            Self::with(Settings::default()).with_listener()
        }

        fn with(settings: Settings) -> Self {
            let dir = tempfile::tempdir().expect("temp dir");
            let ssh = FakeSsh::default();
            let herdr = FakeHerdr::default();
            let requests = FakeRequests::default();
            let clock = FakeClock::default();
            let (warnings, reporter) = recorder();
            let opener = FakeOpener::default();
            let mut config = AgentConfig::new(host(), dir.path().join("buildbox.ctl"));
            config.settings = settings;
            let agent = Agent::new(
                ssh.clone(),
                herdr.clone(),
                requests.clone(),
                clock.clone(),
                Store::open(dir.path()).expect("store"),
                config.clone(),
            )
            .with_reporter(Arc::clone(&reporter))
            .with_opener(Box::new(opener.clone()))
            .with_ports(Box::new(ssh.clone()));
            Self {
                store: Store::open(dir.path()).expect("second store handle"),
                ssh,
                herdr,
                requests,
                clock,
                warnings,
                reporter,
                opener,
                config,
                agent,
                dir,
            }
        }

        /// Replace the agent with a fresh one over the same fakes and the same
        /// files: what the next `attach` gets. Everything the old agent knew
        /// only in memory is gone, which is the whole point.
        fn restart(&mut self) {
            self.agent = Agent::new(
                self.ssh.clone(),
                self.herdr.clone(),
                self.requests.clone(),
                self.clock.clone(),
                Store::open(self.dir.path()).expect("store"),
                self.config.clone(),
            )
            .with_reporter(Arc::clone(&self.reporter))
            .with_opener(Box::new(self.opener.clone()))
            .with_ports(Box::new(self.ssh.clone()));
        }

        /// Answer every discovery command with a single node listener.
        fn with_listener(self) -> Self {
            self.ssh.answer(discover::LSOF_LISTENERS_COMMAND, LSOF);
            self.ssh.answer(discover::PS_TREE_COMMAND, PS);
            self.herdr.lock().panes = vec![pane("w1:p1", 700)];
            self
        }

        fn warnings(&self) -> Vec<String> {
            self.warnings.lock().expect("warnings").clone()
        }
    }

    // -- requests ----------------------------------------------------------

    #[test]
    fn a_request_is_applied_exactly_once_even_when_the_log_is_read_twice() {
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173)]);

        let first = h.agent.step();
        // The source keeps returning the same document: nothing consumed it.
        let second = h.agent.step();

        assert_eq!(first.applied, vec![5173]);
        assert_eq!(second.applied, Vec::<u16>::new());
        assert_eq!(h.ssh.lock().forwards, vec!["localhost:5173:127.0.0.1:5173"]);
        assert_eq!(h.requests.lock().reads, 2, "the log really was read twice");
    }

    // -- the clicked URL ---------------------------------------------------
    //
    // Under `herdr --remote` the plugin that handles a Ctrl+click runs on the
    // remote host and the browser is here. The plugin told the user the URL
    // "opens once the local agent applies the forward" and then nobody opened
    // anything, because the opener was wired only into the process the plugin
    // runs in. It has to be wired here, where the browser is.

    fn clicked(id: u64, port: u16, url: &str) -> Request {
        request(id, port).opening(Some(url.to_string()))
    }

    #[test]
    fn a_ctrl_click_on_the_remote_opens_a_browser_on_this_machine() {
        let mut h = Harness::new();
        h.requests
            .set(vec![clicked(1, 5173, "http://localhost:5173/app?x=1")]);

        let report = h.agent.step();

        assert_eq!(report.applied, vec![5173], "the forward comes first");
        assert_eq!(
            h.opener.opened(),
            vec!["http://localhost:5173/app?x=1".to_string()],
            "the click reached no browser: {:?}",
            report.errors
        );
        assert_eq!(report.opened, vec!["http://localhost:5173/app?x=1"]);
    }

    #[test]
    fn a_request_that_carries_no_url_opens_nothing() {
        // `add 5173` from the remote is not a click, and must not put a window
        // in front of anybody.
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173)]);
        h.agent.step();
        assert!(h.opener.opened().is_empty());
    }

    #[test]
    fn one_click_opens_one_window() {
        // The same request comes back on every poll until the mark settles it,
        // and the mark cannot advance past a request that is still failing. So
        // the agent meets this click again on every pass while 3000 keeps
        // colliding, and one click may not become a window a poll.
        let mut h = Harness::new();
        // A forward of remote 9000 that landed on local 3000. The request for
        // remote 3000 collides with it, so it can never be applied and never
        // settles, and the mark stays behind the click that follows it.
        h.store
            .upsert(Forward {
                spec: ForwardSpec {
                    remote_port: 9000,
                    local_port: 3000,
                    ..spec(3000)
                },
                created_unix_ms: 1,
            })
            .expect("seed the forward local 3000 already belongs to");
        h.ssh.bind(3000);
        h.requests.set(vec![
            request(1, 3000),
            clicked(2, 5173, "http://localhost:5173/"),
        ]);

        h.agent.step();
        h.agent.step();
        h.agent.step();

        assert!(
            h.requests.settled().is_empty(),
            "the click is met again every pass, which is the point of this test"
        );
        assert_eq!(h.opener.opened().len(), 1, "{:?}", h.opener.opened());
    }

    #[test]
    fn a_second_click_on_a_url_already_opened_opens_it_again() {
        // The user closed the tab and Ctrl+clicked the same URL again. That is a
        // new request, with a new id, asking for a window as much as the first
        // one did. Remembering URL strings for the life of the agent swallowed
        // it and left the click doing nothing at all.
        let mut h = Harness::new();
        h.requests
            .set(vec![clicked(1, 5173, "http://localhost:5173/app")]);
        h.agent.step();

        h.requests
            .set(vec![clicked(2, 5173, "http://localhost:5173/app")]);
        let report = h.agent.step();

        assert!(report.applied.is_empty(), "the forward is already there");
        assert_eq!(
            h.opener.opened(),
            vec![
                "http://localhost:5173/app".to_string(),
                "http://localhost:5173/app".to_string()
            ],
            "the second click reached no browser: {:?}",
            report.errors
        );
    }

    #[test]
    fn a_url_this_machine_would_not_forward_is_not_a_url_it_will_open() {
        // The string arrived in a file on the remote, so the manifest pattern
        // that first matched it is on the wrong side of the trust boundary. The
        // agent re-validates rather than believing the record — invariant 11's
        // reasoning at the other end of the click.
        let mut h = Harness::new();
        h.requests.set(vec![
            clicked(1, 5173, "http://evil.example.com:5173/"),
            clicked(2, 3000, "file:///etc/passwd"),
            clicked(3, 8080, "http://localhost:8080@evil.example.com/"),
            clicked(4, 9000, "http://127.0.0.2:9000/"),
        ]);

        let report = h.agent.step();

        assert!(
            h.opener.opened().is_empty(),
            "a remote pointed this machine's browser somewhere: {:?}",
            h.opener.opened()
        );
        assert_eq!(
            report.applied,
            vec![5173, 3000, 8080, 9000],
            "the forward the user asked for still happens"
        );
        assert_eq!(report.errors.len(), 4, "each refusal is said out loud");
    }

    #[test]
    fn a_url_naming_a_port_other_than_the_one_that_was_forwarded_is_refused() {
        // Loopback, so the validator passes it — and pointed at whatever else
        // this machine happens to have listening. Only the port this pass
        // actually bound is opened.
        let mut h = Harness::new();
        h.requests
            .set(vec![clicked(1, 5173, "http://localhost:22/")]);

        let report = h.agent.step();

        assert!(h.opener.opened().is_empty());
        assert_eq!(report.applied, vec![5173]);
        assert!(
            report.errors.iter().any(|e| e.contains("22")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn a_url_on_a_request_that_was_never_applied_is_never_opened() {
        // Expired, and somebody else's. Neither has a forward behind it, so the
        // URL would resolve to whatever else holds that port here.
        let mut h = Harness::new();
        let mut expired = clicked(1, 5173, "http://localhost:5173/");
        expired.created_unix_ms = 0;
        let mut elsewhere = clicked(2, 3000, "http://localhost:3000/");
        elsewhere.host = "another-host".to_string();
        h.requests.set(vec![expired, elsewhere]);
        h.clock.set_now(DEFAULT_TTL_MS + 10);

        let report = h.agent.step();

        assert_eq!(report.expired, vec![5173]);
        assert!(report.applied.is_empty());
        assert!(h.opener.opened().is_empty());
    }

    #[test]
    fn a_url_whose_forward_failed_is_not_opened_either() {
        let mut h = Harness::new();
        h.ssh.lock().forward_error = Some("port already bound".to_string());
        h.requests
            .set(vec![clicked(1, 5173, "http://localhost:5173/")]);

        let report = h.agent.step();

        assert!(report.applied.is_empty());
        assert!(h.opener.opened().is_empty());
    }

    #[test]
    fn a_second_click_on_a_port_that_is_already_forwarded_still_opens_it() {
        // The record answers the request, so no new forward is made — but the
        // user clicked, and the port they clicked is forwarded. Doing nothing
        // here is the same silence this whole path exists to end.
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173)]);
        h.agent.step();
        assert!(h.opener.opened().is_empty());

        h.requests
            .set(vec![clicked(2, 5173, "http://localhost:5173/app")]);
        let report = h.agent.step();

        assert!(report.applied.is_empty(), "nothing new was forwarded");
        assert_eq!(h.opener.opened(), vec!["http://localhost:5173/app"]);
    }

    /// A record for 5173 with nothing bound behind it: what an agent finds
    /// after the master its forwards lived on went away, or after one was
    /// cancelled behind the tool's back. Invariant 3 — a stale record never
    /// shadows a real port.
    fn stale_record(h: &Harness) {
        h.store
            .upsert(Forward {
                spec: spec(5173),
                created_unix_ms: 1,
            })
            .expect("seed a record with no forward behind it");
    }

    #[test]
    fn a_url_over_a_stale_record_is_not_opened_at_a_port_nothing_is_listening_on() {
        // The record says the click's port is forwarded and it is not. Believing
        // it opens a browser on a dead port — an error page the user did not ask
        // for — and settles the request, so nobody ever forwards it either.
        let mut h = Harness::new();
        stale_record(&h);
        h.ssh.lock().forward_error = Some("cannot listen to port: 5173".to_string());
        h.requests
            .set(vec![clicked(1, 5173, "http://localhost:5173/app")]);

        let report = h.agent.step();

        assert!(
            h.opener.opened().is_empty(),
            "a browser was pointed at a port with no tunnel behind it: {:?}",
            h.opener.opened()
        );
        assert!(report.applied.is_empty());
        assert!(
            h.requests.settled().is_empty(),
            "a request nobody could apply is not settled"
        );
    }

    #[test]
    fn a_stale_record_is_replaced_by_the_forward_the_click_asked_for() {
        // The other half: the record is not obeyed, so the request is applied
        // for real — and only then does the URL reach a browser.
        let mut h = Harness::new();
        stale_record(&h);
        h.requests
            .set(vec![clicked(1, 5173, "http://localhost:5173/app")]);

        let report = h.agent.step();

        assert_eq!(report.applied, vec![5173], "{:?}", report.errors);
        assert_eq!(h.ssh.lock().forwards, vec!["localhost:5173:127.0.0.1:5173"]);
        assert_eq!(h.opener.opened(), vec!["http://localhost:5173/app"]);
    }

    #[test]
    fn an_opener_that_refuses_is_reported_rather_than_silently_swallowed() {
        let mut h = Harness::new();
        h.opener.refuse("xdg-open exited with 1");
        h.requests
            .set(vec![clicked(1, 5173, "http://localhost:5173/")]);

        let report = h.agent.step();

        assert_eq!(report.applied, vec![5173], "the forward is still made");
        assert!(report.opened.is_empty());
        assert!(
            report.errors.iter().any(|e| e.contains("xdg-open")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn only_the_last_urls_are_remembered_so_a_remote_cannot_grow_this_forever() {
        // The ids come off a file on the far side. Remembering every one would
        // be memory a compromised remote can grow for free.
        let mut h = Harness::new();
        let mut id = 0u64;
        for n in 0..(MAX_REMEMBERED_OPENS + 8) {
            id += 1;
            let port = 20_000 + u16::try_from(n).expect("fits");
            h.requests.set(vec![clicked(
                id,
                port,
                &format!("http://localhost:{port}/"),
            )]);
            h.agent.step();
        }
        assert_eq!(
            h.agent.state.opened.len(),
            MAX_REMEMBERED_OPENS,
            "the set of remembered opens is unbounded"
        );
    }

    #[test]
    fn a_request_cannot_aim_the_far_end_of_the_tunnel_at_a_third_host() {
        // The request document lives on the remote and is written by whatever
        // runs there. A `remote_bind` of its choosing would make
        // 127.0.0.1:8080 here a tunnel to that host's cloud metadata service,
        // or to SSH on a neighbour of its choosing.
        let mut h = Harness::new();
        let mut hostile = request(1, 8080);
        hostile.host = String::new();
        hostile.remote_bind = "169.254.169.254".to_string();
        let mut also_hostile = request(2, 2222);
        also_hostile.host = String::new();
        also_hostile.remote_bind = "10.0.0.7".to_string();
        h.requests.set(vec![hostile, also_hostile]);

        let report = h.agent.step();

        assert_eq!(report.applied, vec![8080, 2222]);
        assert_eq!(
            h.ssh.lock().forwards,
            vec![
                "localhost:8080:127.0.0.1:8080",
                "localhost:2222:127.0.0.1:2222"
            ],
            "the far end of a forward is decided here, never by the remote"
        );
        for port in [8080u16, 2222] {
            let recorded = h
                .store
                .get("buildbox", port)
                .expect("store read")
                .expect("a record");
            assert_eq!(recorded.spec.remote_bind, "127.0.0.1");
        }
        assert!(
            h.warnings()
                .iter()
                .any(|line| line.contains("169.254.169.254")),
            "the substitution has to be visible in the log: {:?}",
            h.warnings()
        );
    }

    #[test]
    fn a_request_naming_v6_loopback_keeps_it() {
        // A remote dev server that bound `::1` exclusively does not answer on
        // 127.0.0.1, so the one non-v4 bind v1 allows must survive.
        let mut h = Harness::new();
        let mut v6 = request(1, 8501);
        v6.remote_bind = "::1".to_string();
        h.requests.set(vec![v6]);

        h.agent.step();

        assert_eq!(h.ssh.lock().forwards, vec!["localhost:8501:[::1]:8501"]);
        assert!(h.warnings().is_empty(), "{:?}", h.warnings());
    }

    #[test]
    fn only_the_two_loopback_spellings_survive_the_clamp() {
        assert_eq!(clamp_remote_bind("127.0.0.1"), None);
        assert_eq!(clamp_remote_bind("::1"), None);
        for hostile in [
            "169.254.169.254",
            "10.0.0.7",
            "",
            "localhost",
            "[::1]",
            "0.0.0.0",
            "127.0.0.2",
            "127.0.0.1 ",
            "evil.example.com",
        ] {
            assert_eq!(
                clamp_remote_bind(hostile),
                Some("127.0.0.1"),
                "{hostile:?} must not reach ssh as written"
            );
        }
    }

    #[test]
    fn an_applied_request_becomes_a_record_under_this_hosts_name() {
        let mut h = Harness::new();
        let mut queued = request(1, 5173);
        // The remote knows the machine by its own name for it.
        queued.host = "buildbox.example".to_string();
        h.requests.set(vec![queued]);
        h.clock.set_now(9_000);

        h.agent.step();

        let recorded = h
            .store
            .get("buildbox", 5173)
            .expect("store read")
            .expect("a record for the applied request");
        assert_eq!(recorded.spec.host, "buildbox");
        assert_eq!(recorded.created_unix_ms, 9_000);
    }

    #[test]
    fn a_request_naming_a_different_host_is_ignored() {
        let mut h = Harness::new();
        let mut queued = request(1, 5173);
        queued.host = "someone-elses-box".to_string();
        h.requests.set(vec![queued]);

        let report = h.agent.step();

        assert!(report.applied.is_empty());
        assert!(h.ssh.lock().forwards.is_empty());
        assert!(report.is_clean(), "an unrelated request is not an error");
    }

    #[test]
    fn an_expired_request_is_reported_and_never_forwarded() {
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173)]);
        h.clock.set_now(1_000 + DEFAULT_TTL_MS + 1);

        let report = h.agent.step();

        assert_eq!(report.expired, vec![5173]);
        assert!(h.ssh.lock().forwards.is_empty());
    }

    #[test]
    fn a_request_at_exactly_the_ttl_is_still_applied() {
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173)]);
        h.clock.set_now(1_000 + DEFAULT_TTL_MS);

        let report = h.agent.step();

        assert_eq!(report.applied, vec![5173]);
        assert!(report.expired.is_empty());
    }

    #[test]
    fn a_forward_that_fails_is_retried_on_the_next_poll() {
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173)]);
        h.ssh.lock().forward_error = Some("cannot listen to port: 5173".to_string());

        let first = h.agent.step();
        assert!(first.applied.is_empty());
        assert_eq!(first.errors.len(), 1);

        h.ssh.lock().forward_error = None;
        let second = h.agent.step();

        assert_eq!(second.applied, vec![5173], "the cursor did not skip it");
    }

    #[test]
    fn a_failed_request_does_not_let_a_later_one_advance_the_cursor_past_it() {
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173), request(2, 8080)]);
        // Every forward fails, so neither request settles.
        h.ssh.lock().forward_error = Some("cannot listen".to_string());
        h.agent.step();

        h.ssh.lock().forward_error = None;
        let report = h.agent.step();

        assert_eq!(report.applied, vec![5173, 8080]);
    }

    #[test]
    fn an_explicit_request_over_a_forward_this_agent_made_pins_the_record() {
        // The agent opened 5173 for a pane; the user then clicked the URL or
        // ran `add`. There is nothing to open, but the record has to stop
        // saying the forward belongs to that pane, or the next `reconcile`
        // after the pane exits cancels the port the user just pinned.
        let mut h = Harness::new();
        let mut auto = spec(5173);
        auto.origin = Origin::Auto;
        h.store
            .upsert(Forward {
                spec: auto,
                created_unix_ms: 1,
            })
            .expect("seed the agent's own forward");
        h.ssh.bind(5173);
        h.requests.set(vec![request(1, 5173)]);

        let report = h.agent.step();

        assert!(report.is_clean(), "{report:?}");
        assert!(
            h.ssh.lock().forwards.is_empty(),
            "the port is already forwarded; nothing to open"
        );
        let recorded = h
            .store
            .get("buildbox", 5173)
            .expect("store read")
            .expect("a record");
        assert_eq!(recorded.spec.origin, Origin::Explicit);
        assert_eq!(
            recorded.created_unix_ms, 1,
            "pinning does not restart the forward's life"
        );
    }

    #[test]
    fn a_detection_never_downgrades_a_pinned_forward() {
        let mut h = Harness::new();
        h.store
            .upsert(Forward {
                spec: spec(5173),
                created_unix_ms: 1,
            })
            .expect("seed a pinned forward");
        h.ssh.bind(5173);
        let mut offered = request(1, 5173);
        offered.origin = Origin::Offered;
        h.requests.set(vec![offered]);

        h.agent.step();

        let recorded = h
            .store
            .get("buildbox", 5173)
            .expect("store read")
            .expect("a record");
        assert_eq!(recorded.spec.origin, Origin::Explicit);
    }

    #[test]
    fn a_request_for_a_local_port_another_forward_holds_is_reported_not_swallowed() {
        // The two halves of a record: `add 3000 --local-port 5173` on the far
        // side asks for local 5173, which the agent has already given to remote
        // 5173. Matching only on the local port makes that record look like the
        // one the request asked for — it is answered "already forwarded", 3000
        // is never opened and nothing says so, and the pin the request carries
        // lands on the unrelated forward it collided with.
        let mut h = Harness::new();
        let mut auto = spec(5173);
        auto.origin = Origin::Auto;
        h.store
            .upsert(Forward {
                spec: auto,
                created_unix_ms: 1,
            })
            .expect("seed the agent's own forward of 5173");
        let mut remap = spec(3000);
        remap.local_port = 5173;
        remap.pane_id = Some("w2:p9".to_string());
        h.requests.set(vec![Request::new(1, 1_000, remap)]);

        let report = h.agent.step();

        assert!(
            !report.is_clean(),
            "a local port that is taken is a conflict, not a success: {report:?}"
        );
        assert!(report.applied.is_empty(), "{report:?}");
        let recorded = h
            .store
            .get("buildbox", 5173)
            .expect("store read")
            .expect("a record");
        assert_eq!(
            recorded.spec.remote_port, 5173,
            "the record still describes the forward it always described"
        );
        assert_eq!(
            recorded.spec.origin,
            Origin::Auto,
            "a request that was refused pins nothing"
        );
        assert_eq!(recorded.spec.pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(
            h.store.list().expect("store read").len(),
            1,
            "and nothing was opened beside it"
        );
    }

    #[test]
    fn a_conflicting_request_is_retried_and_finally_expires_rather_than_stalling() {
        // Invariant of the mark: an unsettled request holds every later one
        // behind it. A conflict is settled the way a failed forward is — by the
        // TTL — so the queue drains without the forward being dropped silently.
        let mut h = Harness::new();
        let mut auto = spec(5173);
        auto.origin = Origin::Auto;
        h.store
            .upsert(Forward {
                spec: auto,
                created_unix_ms: 1,
            })
            .expect("seed");
        let mut remap = spec(3000);
        remap.local_port = 5173;
        h.requests.set(vec![Request::new(1, 1_000, remap)]);

        h.agent.step();
        assert!(h.requests.settled().is_empty(), "nothing settled yet");

        h.clock.set_now(1_000 + DEFAULT_TTL_MS + 1);
        let report = h.agent.step();

        assert_eq!(report.expired, vec![5173]);
        assert_eq!(h.requests.settled(), vec![1]);
    }

    #[test]
    fn a_port_already_recorded_is_not_forwarded_again() {
        let mut h = Harness::new();
        h.store
            .upsert(Forward {
                spec: spec(5173),
                created_unix_ms: 1,
            })
            .expect("seed a live forward");
        h.ssh.bind(5173);
        h.requests.set(vec![request(1, 5173)]);

        let report = h.agent.step();

        assert!(report.applied.is_empty());
        assert!(
            h.ssh.lock().forwards.is_empty(),
            "ssh must not be asked for a forward that already exists"
        );
    }

    #[test]
    fn a_request_that_cannot_be_forwarded_tells_the_user_once() {
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173)]);
        h.ssh.lock().forward_error = Some("cannot listen to port: 5173".to_string());

        h.agent.step();
        h.agent.step();
        h.agent.step();

        assert_eq!(
            h.herdr.titles(),
            vec!["Port 5173 not forwarded".to_string()],
            "one notification, however many times the forward is retried"
        );
    }

    #[test]
    fn an_applied_request_is_settled_where_the_process_that_wrote_it_can_see_it() {
        // Only this agent knows the forward was made. Keeping that to itself is
        // what left `status` on the other machine reporting "pending: 1" for
        // the rest of the session.
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173), request(2, 8080)]);

        let report = h.agent.step();

        assert_eq!(report.applied, vec![5173, 8080]);
        assert_eq!(
            h.requests.settled(),
            vec![2],
            "the whole settled run is named, once"
        );
    }

    #[test]
    fn a_request_nothing_here_will_ever_apply_is_settled_too() {
        // Expired and not-ours both leave the request unapplicable forever.
        // Left unsettled they stall the mark, and everything queued behind them
        // stays pending in the writer's eyes for as long as the session lasts.
        let mut h = Harness::new();
        let mut elsewhere = request(1, 5173);
        elsewhere.host = "someone-elses-box".to_string();
        h.requests.set(vec![elsewhere, request(2, 8080)]);
        h.clock.set_now(1_000 + DEFAULT_TTL_MS + 1);

        let report = h.agent.step();

        assert_eq!(report.expired, vec![8080]);
        assert_eq!(h.requests.settled(), vec![2]);
    }

    #[test]
    fn a_failed_forward_leaves_the_mark_where_it_was() {
        // Settling a request the agent has not managed to apply is the one way
        // this mark can lose a forward: nothing ever offers it again.
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173), request(2, 8080)]);
        h.ssh.lock().forward_error = Some("cannot listen".to_string());

        h.agent.step();

        assert!(
            h.requests.settled().is_empty(),
            "nothing was applied, so nothing may be marked applied"
        );
    }

    #[test]
    fn a_restarted_agent_does_not_re_open_a_port_the_user_has_since_stopped() {
        // The agent's cursor lives in memory and an attach ends. On the next
        // one the request is still in the remote document and still inside its
        // ten-minute TTL, and the record that would have masked it is gone
        // because the user ran `stop`.
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173)]);
        assert_eq!(h.agent.step().applied, vec![5173]);

        h.store
            .remove("buildbox", 5173)
            .expect("the user stopped it");
        h.ssh.lock().forwards.clear();
        h.restart();

        let report = h.agent.step();

        assert!(report.applied.is_empty(), "{report:?}");
        assert!(
            h.ssh.lock().forwards.is_empty(),
            "a request applied by the previous agent is not a fresh intent"
        );
    }

    #[test]
    fn an_unreadable_mark_stops_the_pass_instead_of_guessing_zero() {
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173)]);
        h.requests.lock().mark_error = Some("ssh: connection closed".to_string());

        let report = h.agent.step();

        assert!(!report.is_clean());
        assert!(
            h.ssh.lock().forwards.is_empty(),
            "not knowing what was applied is not licence to apply it again"
        );

        h.requests.lock().mark_error = None;
        assert_eq!(h.agent.step().applied, vec![5173], "and it retries");
    }

    #[test]
    fn the_mark_is_read_once_and_not_on_every_pass() {
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173)]);
        h.agent.step();
        h.requests.lock().mark_error = Some("ssh: connection closed".to_string());

        assert!(
            h.agent.step().is_clean(),
            "the mark is this agent's own from here on"
        );
    }

    #[test]
    fn an_unreadable_request_source_is_an_error_not_an_exit() {
        let mut h = Harness::new();
        h.requests.lock().error = Some("ssh: connect timed out".to_string());

        let report = h.agent.step();

        assert!(!report.is_clean());
        assert!(!report.master_gone);
        assert_eq!(h.agent.failures(), 1);
    }

    // -- backoff and the loop ---------------------------------------------

    #[test]
    fn backoff_grows_while_the_remote_fails_and_resets_after_a_success() {
        let mut h = Harness::new();
        h.requests.lock().error = Some("ssh: connection refused".to_string());

        assert_eq!(h.agent.delay(), Duration::from_secs(5));
        h.agent.step();
        assert_eq!(h.agent.delay(), Duration::from_secs(10));
        h.agent.step();
        assert_eq!(h.agent.delay(), Duration::from_secs(20));

        h.requests.lock().error = None;
        h.agent.step();

        assert_eq!(h.agent.failures(), 0);
        assert_eq!(h.agent.delay(), Duration::from_secs(5));
    }

    #[test]
    fn backoff_stops_growing_at_the_ceiling() {
        let mut h = Harness::new();
        h.requests.lock().error = Some("ssh: no route to host".to_string());
        for _ in 0..50 {
            h.agent.step();
        }
        assert_eq!(h.agent.delay(), Duration::from_secs(MAX_BACKOFF_SECONDS));
    }

    #[test]
    fn a_long_poll_interval_is_still_capped_by_the_ceiling() {
        let mut h = Harness::with(Settings {
            poll_seconds: 45,
            ..Settings::default()
        });
        h.requests.lock().error = Some("down".to_string());
        h.agent.step();
        assert_eq!(h.agent.delay(), Duration::from_secs(MAX_BACKOFF_SECONDS));
    }

    #[test]
    fn discovery_disabled_still_polls_on_the_default_tick() {
        let h = Harness::with(Settings {
            poll_seconds: 0,
            ..Settings::default()
        });
        assert_eq!(h.agent.delay(), Duration::from_secs(DEFAULT_TICK_SECONDS));
    }

    // -- discovery and offers ---------------------------------------------

    #[test]
    fn a_new_listener_is_offered_and_not_forwarded() {
        let mut h = Harness::discovering();

        let report = h.agent.step();

        assert_eq!(report.offered, vec![5173]);
        assert!(
            h.ssh.lock().forwards.is_empty(),
            "invariant 7: a detection is an offer, never a forward"
        );
        assert!(
            h.store.list().expect("store read").is_empty(),
            "an offer records nothing"
        );
    }

    #[test]
    fn a_listener_is_offered_exactly_once_across_repeated_polls() {
        let mut h = Harness::discovering();

        for _ in 0..10 {
            h.agent.step();
        }

        assert_eq!(h.herdr.lock().notifications.len(), 1);
    }

    #[test]
    fn a_listener_that_goes_away_and_comes_back_is_offered_again() {
        let mut h = Harness::discovering();
        h.agent.step();

        h.ssh.answer(discover::LSOF_LISTENERS_COMMAND, "");
        h.agent.step();
        h.ssh.answer(discover::LSOF_LISTENERS_COMMAND, LSOF);
        let report = h.agent.step();

        assert_eq!(report.offered, vec![5173], "a new server on that port");
        assert_eq!(h.herdr.lock().notifications.len(), 2);
    }

    #[test]
    fn a_listener_that_reaches_no_pane_is_not_offered_by_default() {
        let mut h = Harness::discovering();
        // The listener's ancestry no longer reaches the pane's shell.
        h.ssh.answer(discover::PS_TREE_COMMAND, "900 1\n");

        let report = h.agent.step();

        assert!(report.offered.is_empty());
        assert!(h.herdr.lock().notifications.is_empty());
    }

    #[test]
    fn an_unattributed_listener_is_offered_when_attribution_is_not_required() {
        let mut h = Harness::with(Settings {
            require_pane_attribution: false,
            ..Settings::default()
        })
        .with_listener();
        h.ssh.answer(discover::PS_TREE_COMMAND, "900 1\n");

        let report = h.agent.step();

        assert_eq!(report.offered, vec![5173]);
    }

    #[test]
    fn a_listener_bound_to_a_routable_address_is_never_offered() {
        let mut h = Harness::discovering();
        h.ssh.answer(
            discover::LSOF_LISTENERS_COMMAND,
            "p900\ncnode\nn10.0.0.4:5173\n",
        );

        let report = h.agent.step();

        assert!(report.offered.is_empty());
    }

    #[test]
    fn offers_can_be_switched_off_without_switching_off_discovery() {
        let mut h = Harness::with(Settings {
            offer_new_listeners: false,
            ..Settings::default()
        })
        .with_listener();

        let report = h.agent.step();

        assert!(report.offered.is_empty());
        assert!(h.herdr.lock().notifications.is_empty());
        assert!(report.is_clean(), "discovery itself still succeeded");
    }

    #[test]
    fn a_port_already_forwarded_is_neither_offered_nor_forwarded_again() {
        let mut h = Harness::discovering();
        h.store
            .upsert(Forward {
                spec: spec(5173),
                created_unix_ms: 1,
            })
            .expect("seed a live forward");

        let report = h.agent.step();

        assert!(report.offered.is_empty());
        assert!(h.ssh.lock().forwards.is_empty());
    }

    /// A record for remote 5173 that landed on local 15173, which is what
    /// `add --local-port` leaves behind once the agent has applied it.
    fn remapped(host: &str, remote_port: u16, local_port: u16) -> Forward {
        Forward {
            spec: ForwardSpec {
                host: host.to_string(),
                remote_port,
                local_port,
                remote_bind: LOOPBACK_V4.to_string(),
                origin: Origin::Explicit,
                pane_id: Some("w1:p1".to_string()),
            },
            created_unix_ms: 1,
        }
    }

    #[test]
    fn a_remapped_forward_is_not_offered_again_under_the_port_it_forwards() {
        // Discovery sees the port the *remote* is listening on. A record is
        // keyed by the local port it was given, so a remapped forward is
        // invisible to a lookup by remote port unless one is asked for.
        let mut h = Harness::discovering();
        h.store
            .upsert(remapped("buildbox", 5173, 15173))
            .expect("seed the remap");

        let report = h.agent.step();

        assert!(
            report.offered.is_empty(),
            "5173 is already forwarded, at 15173"
        );
        assert!(h.herdr.lock().notifications.is_empty());
    }

    #[test]
    fn an_allowlist_does_not_open_a_second_forward_beside_a_remap() {
        // The worse half of the same lookup: with a rule for this workspace the
        // listener is not merely re-offered, it is forwarded again — a second
        // tunnel to the same server, on the port the user moved it off.
        let mut h = Harness::with(Settings {
            auto: vec![AutoRule {
                workspace: "w1".to_string(),
                ports: vec![5173],
            }],
            ..Settings::default()
        })
        .with_listener();
        h.store
            .upsert(remapped("buildbox", 5173, 15173))
            .expect("seed the remap");

        let report = h.agent.step();

        assert!(report.auto.is_empty(), "{report:?}");
        assert!(
            h.ssh.lock().forwards.is_empty(),
            "the remote port is forwarded already"
        );
        assert_eq!(
            h.store.list().expect("store read").len(),
            1,
            "one forward per remote port"
        );
    }

    #[test]
    fn an_allowlisted_port_is_forwarded_without_asking() {
        let mut h = Harness::with(Settings {
            auto: vec![AutoRule {
                workspace: "w1".to_string(),
                ports: vec![5173],
            }],
            ..Settings::default()
        })
        .with_listener();

        let report = h.agent.step();

        assert_eq!(report.auto, vec![5173]);
        assert!(report.offered.is_empty());
        assert_eq!(h.ssh.lock().forwards, vec!["localhost:5173:127.0.0.1:5173"]);
        let recorded = h
            .store
            .get("buildbox", 5173)
            .expect("store read")
            .expect("an auto forward is recorded");
        assert_eq!(recorded.spec.origin, Origin::Auto);
        assert_eq!(recorded.spec.pane_id.as_deref(), Some("w1:p1"));
    }

    #[test]
    fn an_allowlist_matches_the_workspace_label_as_well_as_its_id() {
        let mut h = Harness::with(Settings {
            auto: vec![AutoRule {
                workspace: "build: scholion".to_string(),
                ports: vec![5173],
            }],
            ..Settings::default()
        })
        .with_listener();

        assert_eq!(h.agent.step().auto, vec![5173]);
    }

    #[test]
    fn an_allowlist_for_another_workspace_only_offers() {
        let mut h = Harness::with(Settings {
            auto: vec![AutoRule {
                workspace: "w9".to_string(),
                ports: vec![5173],
            }],
            ..Settings::default()
        })
        .with_listener();

        let report = h.agent.step();

        assert!(report.auto.is_empty());
        assert_eq!(report.offered, vec![5173]);
    }

    #[test]
    fn an_allowlist_cannot_match_a_listener_that_reaches_no_pane() {
        let mut h = Harness::with(Settings {
            require_pane_attribution: false,
            auto: vec![AutoRule {
                workspace: "w1".to_string(),
                ports: vec![5173],
            }],
            ..Settings::default()
        })
        .with_listener();
        h.ssh.answer(discover::PS_TREE_COMMAND, "900 1\n");

        let report = h.agent.step();

        assert!(
            report.auto.is_empty(),
            "an allowlist is scoped to a workspace, and this listener has none"
        );
        assert_eq!(report.offered, vec![5173]);
    }

    #[test]
    fn a_v6_only_listener_is_forwarded_to_v6_loopback() {
        let mut h = Harness::with(Settings {
            auto: vec![AutoRule {
                workspace: "w1".to_string(),
                ports: vec![8501],
            }],
            ..Settings::default()
        })
        .with_listener();
        h.ssh.answer(
            discover::LSOF_LISTENERS_COMMAND,
            "p900\ncpython\nn[::1]:8501\n",
        );

        h.agent.step();

        // Bracketed: `localhost:8501:::1:8501` is four colons of ambiguity, and
        // ssh would read the destination as something other than v6 loopback.
        assert_eq!(h.ssh.lock().forwards, vec!["localhost:8501:[::1]:8501"]);
    }

    #[test]
    fn discovery_falls_back_to_ss_when_the_host_has_no_lsof() {
        let mut h = Harness::discovering();
        h.ssh
            .refuse(discover::LSOF_LISTENERS_COMMAND, "lsof: command not found");
        h.ssh.answer(discover::SS_LISTENERS_COMMAND, SS);

        let report = h.agent.step();

        assert_eq!(report.offered, vec![5173]);
        assert!(report.is_clean(), "a missing lsof is not a failed pass");
    }

    #[test]
    fn the_working_listener_command_is_remembered_across_passes() {
        let mut h = Harness::discovering();
        h.ssh
            .refuse(discover::LSOF_LISTENERS_COMMAND, "lsof: command not found");
        h.ssh.answer(discover::SS_LISTENERS_COMMAND, SS);

        h.agent.step();
        h.ssh.lock().execs.clear();
        h.agent.step();

        let execs = h.ssh.lock().execs.clone();
        assert_eq!(
            execs.first().map(String::as_str),
            Some(discover::SS_LISTENERS_COMMAND),
            "the second pass asks the command that worked first"
        );
    }

    #[test]
    fn a_host_with_neither_lsof_nor_ss_backs_off_instead_of_spinning() {
        let mut h = Harness::discovering();
        h.ssh.refuse(discover::LSOF_LISTENERS_COMMAND, "not found");
        h.ssh.refuse(discover::SS_LISTENERS_COMMAND, "not found");

        let report = h.agent.step();

        assert!(!report.is_clean());
        assert_eq!(h.agent.failures(), 1);
        assert!(h.agent.delay() > Duration::from_secs(5));
    }

    #[test]
    fn a_pane_whose_shell_pid_needs_a_second_call_still_attributes() {
        let mut h = Harness::discovering();
        {
            let mut state = h.herdr.lock();
            state.panes = vec![PaneRef {
                shell_pid: None,
                ..pane("w1:p1", 0)
            }];
            state.pids.insert("w1:p1".to_string(), 700);
        }

        let report = h.agent.step();

        assert_eq!(report.offered, vec![5173]);
    }

    #[test]
    fn a_pane_list_that_fails_is_an_error_not_an_exit() {
        let mut h = Harness::discovering();
        h.herdr.lock().pane_list_error = Some("herdr: no server".to_string());

        let report = h.agent.step();

        assert!(!report.is_clean());
        assert!(!report.master_gone);
    }

    #[test]
    fn a_notification_that_fails_leaves_the_port_offerable() {
        let mut h = Harness::discovering();
        h.herdr.lock().notify_error = Some("herdr: no server".to_string());
        h.agent.step();

        h.herdr.lock().notify_error = None;
        let report = h.agent.step();

        assert_eq!(report.offered, vec![5173]);
    }

    #[test]
    fn offer_notification_names_the_port_the_process_and_the_pane() {
        let candidate = AttributedListener {
            listener: Listener {
                port: 5173,
                bind: "*".to_string(),
                pid: 900,
                process: "node".to_string(),
            },
            pane: Some(pane("w1:p1", 700)),
        };

        let notification = offer_notification("buildbox", &candidate);

        assert_eq!(notification.title, "Port 5173 on buildbox");
        assert!(notification.body.contains("node is listening"));
        assert!(notification.body.contains("claude in pane w1:p1"));
        assert!(notification.body.contains("herdr-portfwd add 5173"));
    }

    #[test]
    fn offer_notification_says_so_when_nothing_owns_the_port() {
        let candidate = AttributedListener {
            listener: Listener {
                port: 9000,
                bind: "*".to_string(),
                pid: 900,
                process: "caddy".to_string(),
            },
            pane: None,
        };

        assert!(offer_notification("buildbox", &candidate)
            .body
            .contains("no pane"));
    }

    #[test]
    fn a_disabled_poll_interval_runs_no_discovery_at_all() {
        let mut h = Harness::new();
        h.ssh.answer(discover::LSOF_LISTENERS_COMMAND, LSOF);
        h.ssh.answer(discover::PS_TREE_COMMAND, PS);
        h.herdr.lock().panes = vec![pane("w1:p1", 700)];

        let report = h.agent.step();

        assert!(report.offered.is_empty());
        assert!(report.is_clean());
        assert!(
            h.ssh.lock().execs.is_empty(),
            "nothing is asked of the remote when discovery is off"
        );
        assert!(h.herdr.lock().notifications.is_empty());
    }

    // -- the master and the loop ------------------------------------------

    #[test]
    fn an_absent_master_stops_a_oneshot_agent_cleanly() {
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173)]);
        h.ssh.lock().master = MasterState::Absent;

        let summary = h.agent.run();

        assert_eq!(summary.reason, StopReason::MasterGone);
        assert_eq!(summary.steps, 1);
        assert!(
            h.ssh.lock().forwards.is_empty(),
            "a dead master gets no forwards"
        );
        assert!(h.clock.sleeps().is_empty(), "stopping does not sleep first");
    }

    #[test]
    fn an_absent_master_only_backs_off_when_the_agent_is_not_oneshot() {
        let mut h = Harness::new();
        h.agent.config.oneshot = false;
        h.ssh.lock().master = MasterState::Absent;
        let flag = h.agent.shutdown_flag();
        h.clock.stop_after(3, flag);

        let summary = h.agent.run();

        assert_eq!(summary.reason, StopReason::Shutdown);
        assert_eq!(summary.steps, 3);
        assert_eq!(
            h.clock.sleeps(),
            vec![
                Duration::from_secs(10),
                Duration::from_secs(20),
                Duration::from_secs(40)
            ],
            "waiting for a master to come back backs off like any other failure"
        );
    }

    #[test]
    fn a_check_that_cannot_run_is_a_failure_not_a_dead_master() {
        let mut h = Harness::new();
        h.ssh.lock().check_error = Some("ssh: broken pipe".to_string());

        let report = h.agent.step();

        assert!(!report.master_gone, "an unanswerable check proves nothing");
        assert!(!report.is_clean());
    }

    #[test]
    fn the_loop_stops_when_the_shutdown_flag_is_set() {
        let mut h = Harness::new();
        let flag = h.agent.shutdown_flag();
        h.clock.stop_after(2, flag);

        let summary = h.agent.run();

        assert_eq!(summary.reason, StopReason::Shutdown);
        assert_eq!(summary.steps, 2);
    }

    #[test]
    fn a_shutdown_between_the_pass_and_the_sleep_is_noticed_without_sleeping() {
        let mut h = Harness::new();
        h.agent.shutdown_flag().store(true, Ordering::SeqCst);

        let summary = h.agent.run();

        assert_eq!(summary.steps, 1);
        assert!(h.clock.sleeps().is_empty());
    }

    #[test]
    fn the_loop_counts_what_it_did() {
        let mut h = Harness::discovering();
        h.requests.set(vec![request(1, 8080)]);
        let flag = h.agent.shutdown_flag();
        h.clock.stop_after(1, flag);

        let summary = h.agent.run();

        assert_eq!(summary.applied, 1);
        assert_eq!(summary.offered, 1);
        assert_eq!(summary.auto, 0);
    }

    #[test]
    fn a_step_that_fails_reports_its_problems_to_the_warning_sink() {
        let mut h = Harness::new();
        h.requests.lock().error = Some("ssh: connect timed out".to_string());
        h.agent.shutdown_flag().store(true, Ordering::SeqCst);

        h.agent.run();

        assert!(h
            .warnings()
            .iter()
            .any(|line| line.contains("connect timed out")));
    }

    // -- the remote request document --------------------------------------

    #[test]
    fn pending_requests_come_back_oldest_first() {
        let text = r#"{
            "version": 1,
            "last_consumed_id": 3,
            "pending": [
              {"id": 9, "host": "buildbox", "remote_port": 8080, "local_port": 8080,
               "remote_bind": "127.0.0.1", "origin": "explicit", "created_unix_ms": 20},
              {"id": 4, "host": "buildbox", "remote_port": 5173, "local_port": 5173,
               "remote_bind": "127.0.0.1", "origin": "offered", "created_unix_ms": 10}
            ]
        }"#;

        let pending = parse_pending(text).expect("a well-formed queue parses");

        assert_eq!(pending.iter().map(|r| r.id).collect::<Vec<_>>(), vec![4, 9]);
    }

    #[test]
    fn an_empty_remote_request_file_is_no_requests_rather_than_an_error() {
        for text in ["", "   ", "\n\n"] {
            assert!(parse_pending(text).expect("empty input parses").is_empty());
        }
    }

    #[test]
    fn a_queue_with_no_pending_section_yields_no_requests() {
        assert!(parse_pending("{}").expect("defaults apply").is_empty());
        assert!(parse_pending(r#"{"version":1}"#)
            .expect("defaults apply")
            .is_empty());
    }

    #[test]
    fn a_truncated_remote_request_file_is_reported_not_parsed() {
        let truncated = r#"{"version":1,"pending":[{"id":1,"host":"buildbox""#;
        assert!(parse_pending(truncated).is_err());
    }

    #[test]
    fn unknown_fields_in_the_remote_queue_do_not_hide_the_requests() {
        let text = r#"{
            "version": 99,
            "unheard_of_section": {"nested": [1, 2, 3]},
            "pending": [
              {"id": 1, "host": "buildbox", "remote_port": 5173, "local_port": 5173,
               "origin": "explicit", "created_unix_ms": 1, "future_field": true}
            ]
        }"#;

        let pending = parse_pending(text).expect("unknown fields are ignored");

        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].remote_bind, "127.0.0.1",
            "a missing bind falls back to loopback"
        );
    }

    #[test]
    fn a_remote_request_read_goes_over_the_master_and_parses_what_it_finds() {
        let ssh = FakeSsh::default();
        ssh.answer(
            REMOTE_REQUESTS_COMMAND,
            r#"{"pending":[{"id":1,"host":"buildbox","remote_port":5173,"local_port":5173,
                "origin":"explicit","created_unix_ms":1}]}"#,
        );
        let source = RemoteRequests::new(ssh.clone(), "buildbox.example", "/tmp/x.ctl");

        let pending = source.pending().expect("a readable file");

        assert_eq!(pending.len(), 1);
        assert_eq!(ssh.lock().execs, vec![REMOTE_REQUESTS_COMMAND.to_string()]);
    }

    #[test]
    fn an_unreadable_remote_request_file_warns_and_yields_nothing() {
        let ssh = FakeSsh::default();
        ssh.answer(REMOTE_REQUESTS_COMMAND, "{not json");
        let (warnings, reporter) = recorder();
        let source =
            RemoteRequests::new(ssh, "buildbox.example", "/tmp/x.ctl").with_reporter(reporter);

        assert!(source
            .pending()
            .expect("a bad file is not a poll failure")
            .is_empty());
        assert_eq!(warnings.lock().expect("warnings").len(), 1);
    }

    #[test]
    fn a_remote_read_that_cannot_run_is_an_error() {
        let ssh = FakeSsh::default();
        ssh.refuse(REMOTE_REQUESTS_COMMAND, "ssh: connection closed");
        let source = RemoteRequests::new(ssh, "buildbox.example", "/tmp/x.ctl");

        assert!(source.pending().is_err());
    }

    /// Run [`REMOTE_REQUESTS_COMMAND`] through a shell with a hand-built
    /// environment — what it does on the far side of the master, minus the ssh
    /// no test is allowed to reach for.
    fn run_remote_requests_command(env: &[(&str, &Path)]) -> String {
        let mut command = std::process::Command::new("/bin/sh");
        command.env_clear().arg("-c").arg(REMOTE_REQUESTS_COMMAND);
        for (key, value) in env {
            command.env(key, value);
        }
        let out = command.output().expect("run the snippet");
        assert!(
            out.status.success(),
            "the snippet exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn plant_request_file(dir: &Path, contents: &str) {
        std::fs::create_dir_all(dir).expect("create the candidate directory");
        std::fs::write(dir.join("requests.json"), contents).expect("plant a request file");
    }

    /// Plant a request file at `relative` under a fresh root and assert the
    /// snippet prints it back, with `env` values resolved against that root.
    fn find_through_the_snippet(relative: &str, env: &[(&str, &str)]) {
        let root = tempfile::tempdir().expect("temp dir");
        let marker = format!("{{\"pending\":[\"{relative}\"]}}");
        plant_request_file(&root.path().join(relative), &marker);
        let owned: Vec<(&str, PathBuf)> = env
            .iter()
            .map(|(key, value)| (*key, root.path().join(value)))
            .collect();
        let borrowed: Vec<(&str, &Path)> = owned
            .iter()
            .map(|(key, value)| (*key, value.as_path()))
            .collect();

        assert_eq!(
            run_remote_requests_command(&borrowed).trim(),
            marker,
            "the snippet never found {relative}"
        );
    }

    #[test]
    fn the_remote_request_command_finds_the_file_in_every_directory_it_names() {
        // This snippet is the only transport for a request under `--remote`. A
        // wrong variable name or a broken loop makes it print nothing, which
        // parses as "no requests": every Ctrl+click silently dropped, with a
        // clean pass reported forever after.
        find_through_the_snippet(
            "injected",
            &[("HERDR_PLUGIN_STATE_DIR", "injected"), ("HOME", "home")],
        );
        for candidate in [
            "xdg/herdr/plugins/portfwd",
            "xdg/herdr-dev/plugins/portfwd",
            "xdg/herdr-portfwd",
        ] {
            find_through_the_snippet(candidate, &[("XDG_STATE_HOME", "xdg"), ("HOME", "home")]);
        }
        // No XDG_STATE_HOME: the `$HOME/.local/state` default has to apply.
        find_through_the_snippet(
            "home/.local/state/herdr/plugins/portfwd",
            &[("HOME", "home")],
        );
    }

    #[test]
    fn the_remote_request_command_prefers_the_directory_this_plugin_writes() {
        // The others are there for a remote running an older plugin. With a
        // current one on the far side its own directory has to win, or the
        // agent polls a file nothing writes any more and every Ctrl+click is
        // dropped.
        let root = tempfile::tempdir().expect("temp dir");
        let injected = root.path().join("injected");
        let xdg = root.path().join("xdg");
        plant_request_file(&injected, "{\"pending\":[\"injected\"]}");
        plant_request_file(
            &xdg.join("herdr/plugins/portfwd"),
            "{\"pending\":[\"xdg\"]}",
        );
        plant_request_file(&xdg.join("herdr-portfwd"), "{\"pending\":[\"own\"]}");

        let stdout = run_remote_requests_command(&[
            ("HERDR_PLUGIN_STATE_DIR", &injected),
            ("XDG_STATE_HOME", &xdg),
            ("HOME", root.path()),
        ]);

        assert_eq!(stdout.trim(), "{\"pending\":[\"own\"]}");
    }

    #[test]
    fn a_host_with_no_request_file_anywhere_prints_nothing_and_succeeds() {
        // Every host that has never run the plugin looks like this, on every
        // poll: silence and exit 0, not an error to back off on.
        let root = tempfile::tempdir().expect("temp dir");
        let stdout = run_remote_requests_command(&[("HOME", root.path())]);

        assert_eq!(stdout, "");
        assert!(parse_pending(&stdout).expect("no output parses").is_empty());
    }

    #[test]
    fn the_remote_request_command_survives_an_environment_with_nothing_in_it() {
        // A non-interactive shell over ssh can arrive with neither HOME nor
        // XDG_STATE_HOME set; an unset variable must not make the snippet fail
        // or start reading paths that are not there.
        assert_eq!(run_remote_requests_command(&[]), "");
    }

    #[test]
    fn a_directory_that_is_not_readable_does_not_stop_the_search() {
        // A `HERDR_PLUGIN_STATE_DIR` pointing at something unreadable — a
        // stale export, a directory from another user — must not hide the
        // request file that is really there.
        let root = tempfile::tempdir().expect("temp dir");
        let home = root.path().join("home");
        plant_request_file(
            &home.join(".local/state/herdr/plugins/portfwd"),
            "{\"pending\":[\"found anyway\"]}",
        );

        let stdout = run_remote_requests_command(&[
            ("HERDR_PLUGIN_STATE_DIR", &root.path().join("nowhere")),
            ("HOME", &home),
        ]);

        assert_eq!(stdout.trim(), "{\"pending\":[\"found anyway\"]}");
    }

    #[test]
    fn the_remote_request_command_interpolates_nothing() {
        // Invariant 9: this is the one place a command string reaches a remote
        // shell, so it has to be a constant with no caller input in it.
        assert!(!REMOTE_REQUESTS_COMMAND.contains("{}"));
        assert!(REMOTE_REQUESTS_COMMAND.contains("requests.json"));
        assert!(
            REMOTE_REQUESTS_COMMAND.contains("herdr/plugins/portfwd"),
            "the plugin state directory Herdr itself would use"
        );
    }

    /// Run one of the remote snippets through a shell with a hand-built
    /// environment, as [`run_remote_requests_command`] does.
    fn run_snippet(command: &str, state_dir: &Path) -> String {
        let out = std::process::Command::new("/bin/sh")
            .env_clear()
            .env("HERDR_PLUGIN_STATE_DIR", state_dir)
            .arg("-c")
            .arg(command)
            .output()
            .expect("run the snippet");
        assert!(
            out.status.success(),
            "the snippet exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn an_applied_request_stops_being_pending_for_the_process_that_wrote_it() {
        // End to end for the reported symptom, minus the ssh no test may
        // reach for: the plugin queues a request, the agent settles it the only
        // way it can from the far side of a master, and the plugin's own view
        // of the queue has to agree.
        let dir = tempfile::tempdir().expect("temp dir");
        let log = crate::request::RequestLog::open(dir.path()).expect("request log");
        let request = log.enqueue(spec(5173), 1).expect("enqueue");
        assert_eq!(log.pending().expect("pending").len(), 1);

        run_snippet(&remote_mark_command(request.id), dir.path());

        assert!(
            log.pending().expect("pending").is_empty(),
            "the agent applied it; nothing is waiting for the agent"
        );
    }

    #[test]
    fn the_mark_the_agent_writes_is_the_mark_it_reads_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = crate::request::RequestLog::open(dir.path()).expect("request log");
        log.enqueue(spec(5173), 1).expect("enqueue");

        assert_eq!(
            parse_mark(&run_snippet(REMOTE_MARK_COMMAND, dir.path())),
            0,
            "a host nothing has applied on yet"
        );
        run_snippet(&remote_mark_command(4), dir.path());
        assert_eq!(parse_mark(&run_snippet(REMOTE_MARK_COMMAND, dir.path())), 4);
    }

    #[test]
    fn the_mark_only_ever_moves_forward() {
        // Two laptops attached to one host both settle what they applied. A
        // mark that went backwards would re-offer a request the other one has
        // already dealt with, and the user stopped.
        let dir = tempfile::tempdir().expect("temp dir");
        let log = crate::request::RequestLog::open(dir.path()).expect("request log");
        log.enqueue(spec(5173), 1).expect("enqueue");

        run_snippet(&remote_mark_command(9), dir.path());
        run_snippet(&remote_mark_command(3), dir.path());

        assert_eq!(parse_mark(&run_snippet(REMOTE_MARK_COMMAND, dir.path())), 9);
    }

    #[test]
    fn a_junk_mark_file_does_not_wedge_the_agent_out_of_settling_anything() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = crate::request::RequestLog::open(dir.path()).expect("request log");
        log.enqueue(spec(5173), 1).expect("enqueue");
        std::fs::write(log.ack_path(), "not a number\nat all\n").expect("plant junk");

        run_snippet(&remote_mark_command(2), dir.path());

        assert_eq!(parse_mark(&run_snippet(REMOTE_MARK_COMMAND, dir.path())), 2);
    }

    #[test]
    fn a_host_with_no_request_file_has_nowhere_to_put_a_mark_and_says_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        run_snippet(&remote_mark_command(7), dir.path());
        assert_eq!(run_snippet(REMOTE_MARK_COMMAND, dir.path()), "");
        assert!(!dir.path().join(crate::request::ACK_FILE).exists());
    }

    #[test]
    fn the_mark_snippet_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = crate::request::RequestLog::open(dir.path()).expect("request log");
        log.enqueue(spec(5173), 1).expect("enqueue");
        run_snippet(&remote_mark_command(1), dir.path());

        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("requests.acked."))
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[test]
    fn the_mark_command_interpolates_a_number_and_nothing_else() {
        // Invariant 9: the only snippet here that is not a constant, and the
        // only thing it interpolates is a `u64`.
        for id in [7u64, 1_234_567, u64::MAX] {
            let command = remote_mark_command(id);
            assert!(
                command.starts_with(remote_state_dir!()),
                "the same directory list as the other two snippets: {command}"
            );
            assert_eq!(
                command.matches(&id.to_string()).count(),
                2,
                "the id appears where it is compared and where it is written, \
                 and nowhere else: {command}"
            );
        }
    }

    #[test]
    fn a_mark_read_over_the_master_parses_what_it_finds() {
        let ssh = FakeSsh::default();
        ssh.answer(REMOTE_MARK_COMMAND, "12\n");
        let source = RemoteRequests::new(ssh.clone(), "buildbox.example", "/tmp/x.ctl");

        assert_eq!(source.settled_mark().expect("a readable mark"), 12);

        ssh.answer(REMOTE_MARK_COMMAND, "");
        assert_eq!(
            source.settled_mark().expect("no mark is not a failure"),
            0,
            "a host nothing has applied on yet"
        );
    }

    #[test]
    fn a_mark_that_cannot_be_written_is_an_error_the_pass_reports() {
        let ssh = FakeSsh::default();
        ssh.refuse(&remote_mark_command(3), "ssh: connection closed");
        let source = RemoteRequests::new(ssh, "buildbox.example", "/tmp/x.ctl");

        assert!(source.settled(3).is_err());
    }

    #[test]
    fn the_local_request_log_settles_a_request_through_its_own_lock() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = crate::request::RequestLog::open(dir.path()).expect("request log");
        log.enqueue(spec(5173), 1).expect("enqueue");
        log.enqueue(spec(8080), 1).expect("enqueue");
        let source = LocalRequests::new(log);

        source.settled(1).expect("settle");

        assert_eq!(source.settled_mark().expect("mark"), 1);
        let pending = source.pending().expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, 2);
    }

    #[test]
    fn the_local_request_log_is_a_request_source() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = crate::request::RequestLog::open(dir.path()).expect("request log");
        log.enqueue(spec(5173), 1).expect("enqueue");
        let source = LocalRequests::new(log);

        assert_eq!(source.pending().expect("pending").len(), 1);
    }

    // -- attach ------------------------------------------------------------

    #[derive(Default)]
    struct LauncherState {
        spawns: Vec<(String, Vec<String>, bool)>,
        terminated: Vec<String>,
        session_ticks: usize,
        session_status: i32,
        session_ignores_signals: bool,
        session_error: Option<String>,
        agent_error: Option<String>,
        logs: Vec<PathBuf>,
    }

    #[derive(Clone, Default)]
    struct FakeLauncher {
        state: Arc<Mutex<LauncherState>>,
    }

    impl FakeLauncher {
        fn lock(&self) -> MutexGuard<'_, LauncherState> {
            self.state.lock().expect("launcher state")
        }

        fn programs(&self) -> Vec<String> {
            self.lock()
                .spawns
                .iter()
                .map(|(program, _, _)| program.clone())
                .collect()
        }
    }

    struct FakeChild {
        name: String,
        shared: Arc<Mutex<LauncherState>>,
        ticks: usize,
        status: i32,
        terminated: bool,
        ignores_signals: bool,
        forever: bool,
    }

    impl ChildProcess for FakeChild {
        fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
            if self.terminated && !self.ignores_signals {
                return Ok(Some(self.status));
            }
            if self.forever {
                return Ok(None);
            }
            if self.ticks == 0 {
                return Ok(Some(self.status));
            }
            self.ticks -= 1;
            Ok(None)
        }

        fn terminate(&mut self) -> std::io::Result<()> {
            self.terminated = true;
            self.shared
                .lock()
                .expect("launcher state")
                .terminated
                .push(self.name.clone());
            Ok(())
        }
    }

    impl Launcher for FakeLauncher {
        type Child = FakeChild;

        fn spawn_interactive(
            &self,
            program: &Path,
            argv: &[String],
        ) -> std::io::Result<Self::Child> {
            let mut state = self.lock();
            state
                .spawns
                .push((program.display().to_string(), argv.to_vec(), false));
            if let Some(why) = state.session_error.clone() {
                return Err(std::io::Error::other(why));
            }
            Ok(FakeChild {
                name: "session".to_string(),
                shared: Arc::clone(&self.state),
                ticks: state.session_ticks,
                status: state.session_status,
                terminated: false,
                ignores_signals: state.session_ignores_signals,
                forever: false,
            })
        }

        fn spawn_background(
            &self,
            program: &Path,
            argv: &[String],
            log: &Path,
        ) -> std::io::Result<Self::Child> {
            let mut state = self.lock();
            state
                .spawns
                .push((program.display().to_string(), argv.to_vec(), true));
            state.logs.push(log.to_path_buf());
            if let Some(why) = state.agent_error.clone() {
                return Err(std::io::Error::other(why));
            }
            Ok(FakeChild {
                name: "agent".to_string(),
                shared: Arc::clone(&self.state),
                ticks: 0,
                status: 0,
                terminated: false,
                ignores_signals: false,
                // An agent runs until it is stopped.
                forever: true,
            })
        }
    }

    struct FakeInterrupts {
        signal: Option<i32>,
        after: usize,
        calls: AtomicUsize,
    }

    impl FakeInterrupts {
        fn none() -> Self {
            Self {
                signal: None,
                after: 0,
                calls: AtomicUsize::new(0),
            }
        }

        fn after(signal: i32, calls: usize) -> Self {
            Self {
                signal: Some(signal),
                after: calls,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl Interrupts for FakeInterrupts {
        fn pending(&self) -> Option<i32> {
            let seen = self.calls.fetch_add(1, Ordering::SeqCst);
            self.signal.filter(|_| seen >= self.after)
        }
    }

    fn plan(dir: &Path) -> AttachPlan {
        let control_path = dir.join("buildbox.ctl");
        AttachPlan {
            agent_log: agent_log_path(&control_path),
            host: host(),
            control_path,
            herdr_bin: PathBuf::from("/usr/local/bin/herdr"),
            herdr_args: vec!["--session".to_string(), "work".to_string()],
            agent_bin: PathBuf::from("/usr/local/bin/herdr-portfwd"),
        }
    }

    fn counter() -> (Arc<AtomicUsize>, Arc<Cleanup>) {
        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::clone(&count);
        (
            count,
            Cleanup::new(move || {
                sink.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    #[test]
    fn the_herdr_argv_puts_the_users_arguments_after_the_target() {
        let argv = herdr_remote_argv(
            "buildbox.example",
            &["--session".to_string(), "work".to_string()],
        );
        assert_eq!(argv, ["--remote", "buildbox.example", "--session", "work"]);
    }

    #[test]
    fn the_agent_argv_keeps_a_dash_leading_host_a_host() {
        assert_eq!(
            local_agent_argv("-weird-name"),
            ["local-agent", "--oneshot", "--", "-weird-name"]
        );
    }

    #[test]
    fn the_agent_log_sits_beside_the_control_socket() {
        assert_eq!(
            agent_log_path(Path::new("/state/herdr-portfwd/buildbox.ctl")),
            PathBuf::from("/state/herdr-portfwd/buildbox.ctl.agent.log")
        );
    }

    #[test]
    fn attach_starts_the_master_then_the_agent_then_the_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ssh = FakeSsh::default();
        let launcher = FakeLauncher::default();
        let (count, cleanup) = counter();
        let attach = Attach::new(
            ssh.clone(),
            launcher.clone(),
            FakeClock::default(),
            FakeInterrupts::none(),
        );

        let outcome = attach
            .run(&plan(dir.path()), &cleanup)
            .expect("the session runs");

        assert_eq!(ssh.lock().ensured, vec!["buildbox.example".to_string()]);
        assert_eq!(
            launcher.programs(),
            vec![
                "/usr/local/bin/herdr-portfwd".to_string(),
                "/usr/local/bin/herdr".to_string()
            ]
        );
        assert!(outcome.agent_started);
        assert_eq!(outcome.status, 0);
        assert_eq!(count.load(Ordering::SeqCst), 1, "cleanup ran once");
    }

    #[test]
    fn attach_gives_the_agent_its_own_log_instead_of_the_users_terminal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let launcher = FakeLauncher::default();
        let (_count, cleanup) = counter();
        let plan = plan(dir.path());
        Attach::new(
            FakeSsh::default(),
            launcher.clone(),
            FakeClock::default(),
            FakeInterrupts::none(),
        )
        .run(&plan, &cleanup)
        .expect("the session runs");

        assert_eq!(launcher.lock().logs, vec![plan.agent_log.clone()]);
        let (_, argv, background) = launcher.lock().spawns[0].clone();
        assert!(background);
        assert_eq!(argv, local_agent_argv("buildbox"));
    }

    #[test]
    fn attach_stops_the_agent_when_the_session_ends() {
        let dir = tempfile::tempdir().expect("temp dir");
        let launcher = FakeLauncher::default();
        let (_count, cleanup) = counter();

        Attach::new(
            FakeSsh::default(),
            launcher.clone(),
            FakeClock::default(),
            FakeInterrupts::none(),
        )
        .run(&plan(dir.path()), &cleanup)
        .expect("the session runs");

        assert_eq!(launcher.lock().terminated, vec!["agent".to_string()]);
    }

    #[test]
    fn attach_cleans_up_when_the_session_cannot_be_started() {
        let dir = tempfile::tempdir().expect("temp dir");
        let launcher = FakeLauncher::default();
        launcher.lock().session_error = Some("no such file".to_string());
        let (count, cleanup) = counter();

        let err = Attach::new(
            FakeSsh::default(),
            launcher.clone(),
            FakeClock::default(),
            FakeInterrupts::none(),
        )
        .run(&plan(dir.path()), &cleanup)
        .expect_err("a client that will not start is an error");

        assert!(matches!(err, AgentError::Spawn { .. }));
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "the master is still torn down"
        );
        assert_eq!(launcher.lock().terminated, vec!["agent".to_string()]);
    }

    #[test]
    fn an_agent_that_will_not_start_does_not_stop_the_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let launcher = FakeLauncher::default();
        launcher.lock().agent_error = Some("permission denied".to_string());
        let (warnings, reporter) = recorder();
        let (_count, cleanup) = counter();

        let outcome = Attach::new(
            FakeSsh::default(),
            launcher,
            FakeClock::default(),
            FakeInterrupts::none(),
        )
        .with_reporter(reporter)
        .run(&plan(dir.path()), &cleanup)
        .expect("the session still runs");

        assert!(!outcome.agent_started);
        assert!(warnings
            .lock()
            .expect("warnings")
            .iter()
            .any(|line| line.contains("permission denied")));
    }

    #[test]
    fn a_master_that_will_not_start_spawns_nothing_and_needs_no_cleanup() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ssh = FakeSsh::default();
        ssh.lock().ensure_error = Some("permission denied".to_string());
        let launcher = FakeLauncher::default();
        let (count, cleanup) = counter();

        let err = Attach::new(
            ssh,
            launcher.clone(),
            FakeClock::default(),
            FakeInterrupts::none(),
        )
        .run(&plan(dir.path()), &cleanup)
        .expect_err("no master, no attach");

        assert!(matches!(err, AgentError::Ssh(_)));
        assert!(launcher.lock().spawns.is_empty());
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "nothing was created to clean up"
        );
    }

    #[test]
    fn an_over_long_control_path_is_refused_before_anything_is_spawned() {
        let deep = PathBuf::from("/tmp").join("x".repeat(MAX_CONTROL_PATH_LEN));
        let mut plan = plan(Path::new("/tmp"));
        plan.control_path = deep;
        let ssh = FakeSsh::default();
        let launcher = FakeLauncher::default();
        let (_count, cleanup) = counter();

        let err = Attach::new(
            ssh.clone(),
            launcher.clone(),
            FakeClock::default(),
            FakeInterrupts::none(),
        )
        .run(&plan, &cleanup)
        .expect_err("invariant 8: the path is checked first");

        assert!(matches!(
            err,
            AgentError::Ssh(SshError::ControlPathTooLong { .. })
        ));
        assert!(ssh.lock().ensured.is_empty());
        assert!(launcher.lock().spawns.is_empty());
    }

    #[test]
    fn a_signal_ends_the_session_and_still_cleans_up() {
        let dir = tempfile::tempdir().expect("temp dir");
        let launcher = FakeLauncher::default();
        launcher.lock().session_ticks = 1_000;
        let (count, cleanup) = counter();

        let outcome = Attach::new(
            FakeSsh::default(),
            launcher.clone(),
            FakeClock::default(),
            FakeInterrupts::after(libc::SIGTERM, 2),
        )
        .run(&plan(dir.path()), &cleanup)
        .expect("a signalled session is not an error");

        assert_eq!(outcome.signal, Some(libc::SIGTERM));
        assert!(launcher.lock().terminated.contains(&"session".to_string()));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_session_that_ignores_the_signal_is_given_up_on_rather_than_waited_for() {
        let dir = tempfile::tempdir().expect("temp dir");
        let launcher = FakeLauncher::default();
        // Bounded, not `usize::MAX`: with the grace cap gone this loop has to
        // end and fail, not hang. `cargo test` has no per-test timeout, and a
        // hang in CI reads as broken infrastructure rather than as this bug.
        launcher.lock().session_ticks = 10_000;
        launcher.lock().session_ignores_signals = true;
        let clock = FakeClock::default();
        let (count, cleanup) = counter();

        let outcome = Attach::new(
            FakeSsh::default(),
            launcher,
            clock.clone(),
            FakeInterrupts::after(libc::SIGINT, 0),
        )
        .run(&plan(dir.path()), &cleanup)
        .expect("the attach ends anyway");

        assert_eq!(outcome.signal, Some(libc::SIGINT));
        assert_eq!(outcome.status, 128 + libc::SIGINT);
        // A literal budget, not `TERMINATE_GRACE_TICKS`: asserting against the
        // constant the code counts down means both sides move together, and
        // raising the cap to something absurd would still pass.
        let slept = clock.sleeps().len();
        assert!(slept <= 101, "waited {slept} ticks for a stuck client");
        let waited: Duration = clock.sleeps().iter().sum();
        assert!(
            waited <= Duration::from_secs(15),
            "a client that ignores the signal held the master for {waited:?}"
        );
        assert_eq!(count.load(Ordering::SeqCst), 1, "the master never leaks");
    }

    #[test]
    fn the_session_exit_status_is_what_attach_reports() {
        let dir = tempfile::tempdir().expect("temp dir");
        let launcher = FakeLauncher::default();
        launcher.lock().session_status = 17;
        let (_count, cleanup) = counter();

        let outcome = Attach::new(
            FakeSsh::default(),
            launcher,
            FakeClock::default(),
            FakeInterrupts::none(),
        )
        .run(&plan(dir.path()), &cleanup)
        .expect("a non-zero exit is still an exit");

        assert_eq!(outcome.status, 17);
    }

    // -- cleanup and teardown ---------------------------------------------

    #[test]
    fn a_cleanup_runs_once_however_many_times_it_is_asked_to() {
        let (count, cleanup) = counter();

        assert!(cleanup.run(), "the first call does the work");
        assert!(!cleanup.run());
        assert!(!cleanup.run());

        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(cleanup.is_done());
    }

    #[test]
    fn a_cleanup_runs_when_its_guard_leaves_scope() {
        let (count, cleanup) = counter();
        {
            let _guard = CleanupGuard::new(Arc::clone(&cleanup));
            assert_eq!(count.load(Ordering::SeqCst), 0);
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_cleanup_already_run_is_not_run_again_by_its_guard() {
        let (count, cleanup) = counter();
        {
            let _guard = CleanupGuard::new(Arc::clone(&cleanup));
            cleanup.run();
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_cleanup_runs_while_a_panic_unwinds_past_its_guard() {
        let (count, cleanup) = counter();
        let guarded = Arc::clone(&cleanup);

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = CleanupGuard::new(guarded);
            panic!("the session blew up");
        }));

        assert!(panicked.is_err());
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "a leaked master with live forwards is the worst outcome there is"
        );
    }

    #[test]
    fn two_threads_racing_on_one_cleanup_still_run_it_once() {
        let (count, cleanup) = counter();
        let mut threads = Vec::new();
        for _ in 0..8 {
            let cleanup = Arc::clone(&cleanup);
            threads.push(std::thread::spawn(move || cleanup.run()));
        }
        let winners = threads
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .filter(|won| *won)
            .count();

        assert_eq!(winners, 1);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn teardown_cancels_every_recorded_forward_then_exits_the_master() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(dir.path()).expect("store");
        for port in [5173, 8080] {
            store
                .upsert(Forward {
                    spec: spec(port),
                    created_unix_ms: 1,
                })
                .expect("seed");
        }
        let ssh = FakeSsh::default();

        let problems = tear_down(&ssh, &host(), Path::new("/tmp/x.ctl"), &store);

        assert!(problems.is_empty());
        assert_eq!(
            ssh.lock().cancels,
            vec![
                "localhost:5173:127.0.0.1:5173",
                "localhost:8080:127.0.0.1:8080"
            ]
        );
        assert_eq!(ssh.lock().exits, 1);
        assert!(store.list().expect("store read").is_empty());
    }

    #[test]
    fn teardown_of_a_host_with_no_forwards_still_exits_the_master() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(dir.path()).expect("store");
        let ssh = FakeSsh::default();

        assert!(tear_down(&ssh, &host(), Path::new("/tmp/x.ctl"), &store).is_empty());
        assert_eq!(ssh.lock().exits, 1);
    }

    #[test]
    fn teardown_leaves_another_hosts_records_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(dir.path()).expect("store");
        let mut other = spec(5173);
        other.host = "laptop".to_string();
        store
            .upsert(Forward {
                spec: other,
                created_unix_ms: 1,
            })
            .expect("seed");

        tear_down(
            &FakeSsh::default(),
            &host(),
            Path::new("/tmp/x.ctl"),
            &store,
        );

        assert_eq!(store.list().expect("store read").len(), 1);
    }

    #[test]
    fn teardown_exits_the_master_even_when_a_cancel_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(dir.path()).expect("store");
        store
            .upsert(Forward {
                spec: spec(5173),
                created_unix_ms: 1,
            })
            .expect("seed");
        let ssh = FakeSsh::default();
        ssh.lock().cancel_error = Some("port not found".to_string());

        let problems = tear_down(&ssh, &host(), Path::new("/tmp/x.ctl"), &store);

        assert_eq!(problems.len(), 1);
        assert_eq!(ssh.lock().exits, 1);
        assert!(
            store.list().expect("store read").is_empty(),
            "the master exited, so nothing it held is live any more"
        );
    }

    #[test]
    fn a_record_survives_a_teardown_that_could_neither_cancel_it_nor_exit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(dir.path()).expect("store");
        store
            .upsert(Forward {
                spec: spec(5173),
                created_unix_ms: 1,
            })
            .expect("seed");
        let ssh = FakeSsh::default();
        ssh.lock().cancel_error = Some("ssh: broken pipe".to_string());
        ssh.lock().exit_error = Some("ssh: broken pipe".to_string());

        let problems = tear_down(&ssh, &host(), Path::new("/tmp/x.ctl"), &store);

        assert_eq!(problems.len(), 2);
        assert_eq!(
            store.list().expect("store read").len(),
            1,
            "reconcile has to be able to find a forward this could not remove"
        );
    }

    #[test]
    fn a_teardown_that_cancelled_but_could_not_exit_forgets_only_what_it_cancelled() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(dir.path()).expect("store");
        store
            .upsert(Forward {
                spec: spec(5173),
                created_unix_ms: 1,
            })
            .expect("seed");
        let ssh = FakeSsh::default();
        ssh.lock().exit_error = Some("ssh: broken pipe".to_string());

        tear_down(&ssh, &host(), Path::new("/tmp/x.ctl"), &store);

        assert!(store.list().expect("store read").is_empty());
    }

    #[test]
    fn the_teardown_cleanup_reports_its_problems_and_runs_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(Store::open(dir.path()).expect("store"));
        store
            .upsert(Forward {
                spec: spec(5173),
                created_unix_ms: 1,
            })
            .expect("seed");
        let ssh = Arc::new(FakeSsh::default());
        ssh.lock().cancel_error = Some("port not found".to_string());
        let (warnings, reporter) = recorder();
        let cleanup = teardown_cleanup(
            Arc::clone(&ssh),
            host(),
            PathBuf::from("/tmp/x.ctl"),
            store,
            reporter,
        );

        cleanup.run();
        cleanup.run();

        assert_eq!(ssh.lock().exits, 1);
        assert_eq!(warnings.lock().expect("warnings").len(), 1);
    }

    // -- the real implementations of the seams -----------------------------

    #[test]
    fn the_system_clock_reads_a_plausible_wall_clock() {
        // Well past 2020 in milliseconds: enough to catch a seconds/millis mixup.
        assert!(SystemClock.now_unix_ms() > 1_600_000_000_000);
    }

    #[test]
    fn no_signal_means_no_interruption() {
        assert_eq!(ProcessSignals.pending(), None);
    }

    #[test]
    fn an_attach_plan_puts_the_agent_log_next_to_the_control_socket() {
        let plan = attach_plan(host(), vec!["--session".to_string()])
            .expect("the plan resolves from the environment");
        assert_eq!(plan.agent_log, agent_log_path(&plan.control_path));
        assert!(plan.control_path.ends_with("buildbox.ctl"));
        assert_eq!(plan.herdr_args, vec!["--session".to_string()]);
    }

    #[test]
    fn a_repeatedly_failing_request_is_reported_once_even_while_discovery_runs() {
        let mut h = Harness::discovering();
        h.requests.set(vec![request(1, 9999)]);
        h.ssh.lock().forward_error = Some("cannot listen to port: 9999".to_string());

        for _ in 0..5 {
            h.agent.step();
        }

        let failures = h
            .herdr
            .lock()
            .notifications
            .iter()
            .filter(|n| n.title.contains("not forwarded"))
            .count();
        assert_eq!(failures, 1);
    }

    #[test]
    fn a_forward_that_finally_succeeds_can_be_reported_again_later() {
        let mut h = Harness::new();
        h.requests.set(vec![request(1, 5173)]);
        h.ssh.lock().forward_error = Some("cannot listen to port: 5173".to_string());
        h.agent.step();

        h.ssh.lock().forward_error = None;
        h.agent.step();
        h.store.remove("buildbox", 5173).expect("drop the record");
        h.requests.set(vec![request(2, 5173)]);
        h.ssh.lock().forward_error = Some("cannot listen to port: 5173".to_string());
        h.agent.step();

        assert_eq!(
            h.herdr
                .lock()
                .notifications
                .iter()
                .filter(|n| n.title.contains("not forwarded"))
                .count(),
            2,
            "a fresh failure after a success is news again"
        );
    }
}
