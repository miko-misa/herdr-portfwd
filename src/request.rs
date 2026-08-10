//! The record that lets "decide to forward" and "actually forward" run on
//! different machines.
//!
//! Under herdr-mirror both happen locally and this is barely more than a
//! function call. Under `herdr --remote` the plugin runs on the remote host and
//! writes here; the local agent reads it over SSH and applies it. Same record,
//! same semantics, only the transport differs.
//!
//! Requests are therefore a stable serialized format, not an internal detail:
//! a newer plugin can be talking to an older local agent. Unknown fields are
//! ignored rather than fatal, and every request carries the plugin version that
//! wrote it.
//!
//! Requests are also idempotent and self-expiring: the agent may read the same
//! file twice, and a request nobody consumed must not be applied hours later.
//!
//! Only the applier knows a request was applied, and under `herdr --remote` the
//! applier is not the process that wrote the request — it is the local agent,
//! reaching this directory through a shell on the far side of an SSH master. It
//! therefore records what it finished with in a second file, [`ACK_FILE`], and
//! everything here folds that mark in before answering. That is what keeps
//! `list` and `status` — which run wherever the *plugin* runs — from reporting
//! an applied request as still waiting, and what keeps a restarted agent from
//! opening a port the user has since stopped.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::{state_dir, stderr_reporter, JsonFile, Reporter, Result};
use crate::types::{ForwardSpec, Origin};

/// File holding pending requests, inside the state directory.
pub const REQUEST_FILE: &str = "requests.json";

/// File carrying the applier's high-water mark, next to [`REQUEST_FILE`].
///
/// A separate file because the two have different writers. Requests are written
/// under this crate's lock by a process that links this crate; the mark is
/// written by the applier, which under `herdr --remote` can only reach this
/// directory as a shell command over SSH and cannot take that lock. One decimal
/// integer and nothing else, so the writer needs no more than a temp file and a
/// rename, a truncated read is impossible to misinterpret, and a missing file
/// simply means "nothing applied yet".
pub const ACK_FILE: &str = "requests.acked";

/// Schema version stamped into the file. Informational, and it buys less than
/// it looks: what a reader older than a writer survives is unknown *fields*,
/// which serde skips. A [`Request`]'s own identity — its id, host, ports,
/// origin and timestamp — is required, exactly as [`crate::state::STATE_VERSION`]
/// describes for a record, and a value none of them can be read as fails the
/// whole queue. That failure is now reported as
/// [`crate::state::StateError::Damaged`]; the applier retries rather than
/// concluding there is nothing left to apply.
pub const REQUEST_VERSION: u32 = 1;

/// How long an unconsumed request stays applicable.
///
/// A request is a statement about what the user wanted a moment ago. Opening a
/// port because of a click from three hours and one closed laptop ago is not
/// what anyone asked for.
pub const DEFAULT_TTL_MS: u64 = 10 * 60 * 1_000;

/// Version of the plugin that writes a request. Recorded so a mismatch is
/// visible in a bug report rather than inferred.
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The remote side v1 forwards to. Only loopback, per `ARCHITECTURE.md`.
fn default_remote_bind() -> String {
    "127.0.0.1".to_string()
}

/// One "please forward this" record.
///
/// Deliberately flat and self-describing: it crosses a machine boundary and a
/// version boundary, and the reader may be older than the writer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Monotonic per request file, supplied by [`RequestLog::enqueue`]. Also
    /// the idempotency key: an id at or below the last consumed one is a
    /// replay and is dropped.
    pub id: u64,
    pub host: String,
    pub remote_port: u16,
    /// Same number as `remote_port` unless the user accepted a remap.
    pub local_port: u16,
    #[serde(default = "default_remote_bind")]
    pub remote_bind: String,
    pub origin: Origin,
    /// Pane the port was attributed to, when one was.
    #[serde(default)]
    pub pane_id: Option<String>,
    /// A URL for the *applier* to open once it has made this forward.
    ///
    /// Under `herdr --remote` the plugin that handles a Ctrl+click runs on the
    /// remote host, and the browser is on the machine the local agent runs on.
    /// Without this the agent had nothing to open and the notification promising
    /// that the URL "opens once the local agent applies the forward" was simply
    /// false.
    ///
    /// Optional in both directions, which is the forward-compatibility rule this
    /// module exists for: an agent older than the field ignores it — nothing
    /// here is `deny_unknown_fields` — and applies the forward exactly as it
    /// always did rather than refusing to parse the queue; a newer agent reading
    /// an older writer's record simply has no URL to open. It is never
    /// serialized when absent, so an older reader's file is byte-identical to
    /// what it wrote before.
    ///
    /// A hint, never an instruction. It crossed a machine boundary in a file
    /// written on the remote, so the applier re-validates it as loopback and
    /// against the port it actually bound before any of it reaches a browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_url: Option<String>,
    /// Plugin version that wrote the record.
    #[serde(default)]
    pub plugin_version: String,
    /// Milliseconds since the Unix epoch, stamped by the caller so the clock
    /// stays out of this layer.
    pub created_unix_ms: u64,
}

impl Request {
    pub fn new(id: u64, created_unix_ms: u64, spec: ForwardSpec) -> Self {
        Self {
            id,
            host: spec.host,
            remote_port: spec.remote_port,
            local_port: spec.local_port,
            remote_bind: spec.remote_bind,
            origin: spec.origin,
            pane_id: spec.pane_id,
            plugin_version: PLUGIN_VERSION.to_string(),
            created_unix_ms,
            open_url: None,
        }
    }

    /// The same request, asking the applier to open `url` once the forward is
    /// in place. Separate from [`Request::new`] because a forward is the whole
    /// record and a URL is an extra the applier may ignore.
    pub fn opening(mut self, url: Option<String>) -> Self {
        self.open_url = url;
        self
    }

    /// What the apply side actually acts on.
    pub fn to_spec(&self) -> ForwardSpec {
        ForwardSpec {
            host: self.host.clone(),
            remote_port: self.remote_port,
            local_port: self.local_port,
            remote_bind: self.remote_bind.clone(),
            origin: self.origin,
            pane_id: self.pane_id.clone(),
        }
    }

    /// Identity for supersession: a second request for the same local port on
    /// the same host replaces the first rather than queueing behind it.
    pub fn key(&self) -> (&str, u16) {
        (self.host.as_str(), self.local_port)
    }

    /// Age at `now_unix_ms`. A timestamp in the future reads as age zero: a
    /// clock skew between the two machines must not expire a fresh request.
    pub fn age_ms(&self, now_unix_ms: u64) -> u64 {
        now_unix_ms.saturating_sub(self.created_unix_ms)
    }

    /// Whether the request is too old to apply. `now` is a parameter, not a
    /// clock read, so expiry is testable without waiting.
    pub fn is_expired(&self, now_unix_ms: u64, ttl_ms: u64) -> bool {
        self.age_ms(now_unix_ms) > ttl_ms
    }
}

/// Outcome of offering a request to the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    /// Queued as a new request.
    Accepted,
    /// Replaced a pending request with the same id or the same target.
    Superseded,
    /// Already consumed, or older than what is already pending. Dropping it is
    /// what makes re-writing a request harmless.
    Stale,
}

/// On-disk shape. Every field defaults so an older reader survives a newer
/// writer, and a partially-understood document still yields its requests.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Queue {
    #[serde(default)]
    pub version: u32,
    /// Highest id ever consumed. The high-water mark that makes a re-read of
    /// the same request a no-op instead of a second forward.
    #[serde(default)]
    pub last_consumed_id: u64,
    #[serde(default)]
    pub pending: Vec<Request>,
}

impl Queue {
    /// Next id to hand out: past everything pending and everything consumed, so
    /// ids stay monotonic across a take.
    fn next_id(&self) -> u64 {
        let highest_pending = self.pending.iter().map(|r| r.id).max().unwrap_or(0);
        highest_pending.max(self.last_consumed_id).saturating_add(1)
    }
}

/// The pending-request file, with the same lock-read-mutate-rename discipline
/// as [`crate::state::Store`] — literally the same implementation, since a
/// second copy of that discipline would be a second chance to get it wrong.
pub struct RequestLog {
    file: JsonFile<Queue>,
    ack_path: PathBuf,
}

impl RequestLog {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        Self::with_reporter(dir, stderr_reporter())
    }

    pub fn with_reporter(dir: impl AsRef<Path>, reporter: Reporter) -> Result<Self> {
        Ok(Self {
            file: JsonFile::open(dir.as_ref(), REQUEST_FILE, reporter)?,
            ack_path: dir.as_ref().join(ACK_FILE),
        })
    }

    /// The log next to this plugin's state, which under `herdr --remote` means
    /// the *remote* state directory: that is where the plugin runs.
    pub fn from_env() -> Result<Self> {
        Self::open(state_dir(REQUEST_FILE)?)
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// Where the applier records what it has finished with.
    pub fn ack_path(&self) -> &Path {
        &self.ack_path
    }

    /// The applier's high-water mark, or zero when there is none.
    ///
    /// A missing, unreadable or nonsensical file reads as zero rather than as
    /// an error. The mark can only ever *remove* requests, so the safe reading
    /// of a damaged one is "nothing has been applied", which at worst leaves a
    /// request pending that the agent will recognise and skip.
    fn ack_mark(&self) -> u64 {
        std::fs::read_to_string(&self.ack_path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Queue a request, allocating its id under the same lock that writes it.
    /// Two processes enqueueing at once therefore cannot collide on an id.
    pub fn enqueue(&self, spec: ForwardSpec, created_unix_ms: u64) -> Result<Request> {
        self.enqueue_opening(spec, created_unix_ms, None)
    }

    /// Queue a request that also asks the applier to open `open_url` once the
    /// forward is live. See [`Request::open_url`] for what the applier is
    /// allowed to believe about it.
    pub fn enqueue_opening(
        &self,
        spec: ForwardSpec,
        created_unix_ms: u64,
        open_url: Option<String>,
    ) -> Result<Request> {
        let mark = self.ack_mark();
        self.file.update(move |queue| {
            settle_to(queue, mark);
            let request = Request::new(queue.next_id(), created_unix_ms, spec).opening(open_url);
            apply_push(queue, request.clone());
            queue.version = REQUEST_VERSION;
            request
        })
    }

    /// Offer a request built elsewhere — a retry, or one relayed from another
    /// machine. Ids at or below the high-water mark are dropped.
    pub fn push(&self, request: Request) -> Result<Push> {
        let mark = self.ack_mark();
        self.file.update(move |queue| {
            settle_to(queue, mark);
            let outcome = apply_push(queue, request);
            queue.version = REQUEST_VERSION;
            outcome
        })
    }

    /// Look without consuming. What `list` and `status` report as waiting for
    /// the applier, so it has to account for what the applier already did.
    pub fn pending(&self) -> Result<Vec<Request>> {
        let mut queue = self.file.read()?;
        settle_to(&mut queue, self.ack_mark());
        normalize(&mut queue.pending);
        Ok(queue.pending)
    }

    /// Record that the applier has finished with every request up to and
    /// including `id` — applied, skipped, or expired — and drop them.
    ///
    /// Deliberately *not* "hand me everything pending and mark it consumed":
    /// the applier learns what happened to a request only after it has tried,
    /// and a forward that failed has to stay pending for the next pass. A
    /// take-then-apply API advances the mark past exactly those, which is a
    /// dropped forward with nothing said. The applier names what it settled.
    ///
    /// For an applier that cannot link this crate — the local agent, which
    /// reaches a remote state directory only through a shell — the equivalent
    /// is writing [`ACK_FILE`], which every read here folds in.
    pub fn consume_through(&self, id: u64) -> Result<Vec<Request>> {
        let mark = self.ack_mark().max(id);
        self.file.update(move |queue| {
            let settled = settle_to(queue, mark);
            queue.version = REQUEST_VERSION;
            settled
        })
    }

    /// Highest id settled so far, by whichever applier got there.
    pub fn last_consumed_id(&self) -> Result<u64> {
        Ok(self.file.read()?.last_consumed_id.max(self.ack_mark()))
    }
}

/// Carry `mark` into `queue` and drop everything it covers, returning what was
/// dropped.
///
/// The high-water mark is the whole idempotency story: a request at or below it
/// has been applied, skipped or expired, and must never be applied again — not
/// by a re-read, not by a re-write, and not by an agent that restarted.
fn settle_to(queue: &mut Queue, mark: u64) -> Vec<Request> {
    queue.last_consumed_id = queue.last_consumed_id.max(mark);
    let cut = queue.last_consumed_id;
    let mut settled = Vec::new();
    queue.pending.retain(|request| {
        if request.id <= cut {
            settled.push(request.clone());
            false
        } else {
            true
        }
    });
    settled
}

/// Queue a request into `queue`, enforcing the idempotency rules. Pure, so the
/// rules can be read in one place and tested without a filesystem.
fn apply_push(queue: &mut Queue, request: Request) -> Push {
    if request.id <= queue.last_consumed_id {
        return Push::Stale;
    }
    if let Some(at) = queue.pending.iter().position(|r| r.id == request.id) {
        queue.pending[at] = request;
        return Push::Superseded;
    }
    if let Some(at) = queue.pending.iter().position(|r| r.key() == request.key()) {
        if queue.pending[at].id > request.id {
            return Push::Stale;
        }
        queue.pending[at] = request;
        return Push::Superseded;
    }
    queue.pending.push(request);
    Push::Accepted
}

/// Put a hand-written or concurrently-merged pending list into the shape the
/// rest of the module assumes: ordered by id, one entry per id, one entry per
/// target with the newest id winning.
fn normalize(pending: &mut Vec<Request>) {
    pending.sort_by_key(|r| r.id);

    let mut seen_ids: Vec<u64> = Vec::new();
    let mut seen_keys: Vec<(String, u16)> = Vec::new();
    let mut kept: Vec<Request> = Vec::new();
    for request in pending.drain(..).rev() {
        let key = (request.host.clone(), request.local_port);
        if seen_ids.contains(&request.id) || seen_keys.contains(&key) {
            continue;
        }
        seen_ids.push(request.id);
        seen_keys.push(key);
        kept.push(request);
    }
    kept.sort_by_key(|r| r.id);
    *pending = kept;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::thread;

    const NOW: u64 = 1_700_000_000_000;

    #[derive(Default)]
    struct Recorder {
        messages: Mutex<Vec<String>>,
    }

    impl Recorder {
        fn reporter(self: &Arc<Self>) -> Reporter {
            let sink = Arc::clone(self);
            Arc::new(move |message: &str| {
                sink.messages
                    .lock()
                    .expect("recorder mutex poisoned")
                    .push(message.to_string())
            })
        }

        fn messages(&self) -> Vec<String> {
            self.messages
                .lock()
                .expect("recorder mutex poisoned")
                .clone()
        }
    }

    fn spec(host: &str, port: u16) -> ForwardSpec {
        ForwardSpec {
            host: host.into(),
            remote_port: port,
            local_port: port,
            remote_bind: "127.0.0.1".into(),
            origin: Origin::Explicit,
            pane_id: Some("pane-7".into()),
        }
    }

    fn log_in(dir: &Path) -> RequestLog {
        RequestLog::open(dir).expect("opening a log in a temp dir should work")
    }

    fn recording_log(dir: &Path) -> (RequestLog, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        let log = RequestLog::with_reporter(dir, recorder.reporter())
            .expect("opening a log in a temp dir should work");
        (log, recorder)
    }

    #[test]
    fn a_request_round_trips_through_json() {
        let request = Request::new(7, NOW, spec("buildbox", 5173));
        let json = serde_json::to_string(&request).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, request);
        assert_eq!(back.plugin_version, PLUGIN_VERSION);
    }

    /// Forward compatibility, the whole reason this is a serialized format:
    /// a newer plugin may write fields this binary has never heard of.
    #[test]
    fn unknown_fields_from_a_newer_writer_are_ignored() {
        let json = r#"{
          "id": 3,
          "host": "buildbox",
          "remote_port": 5173,
          "local_port": 5173,
          "remote_bind": "127.0.0.1",
          "origin": "offered",
          "pane_id": "pane-7",
          "plugin_version": "9.9.9",
          "created_unix_ms": 1700000000000,
          "protocol": "quic",
          "nested_future_thing": {"a": [1, 2, 3]}
        }"#;
        let request: Request =
            serde_json::from_str(json).expect("unknown fields must not be fatal");
        assert_eq!(request.id, 3);
        assert_eq!(request.origin, Origin::Offered);
        assert_eq!(request.plugin_version, "9.9.9");
    }

    /// The other half of forward compatibility, and the reason `open_url` is
    /// optional: an agent that predates the field must go on applying the
    /// forward instead of choking on the record that carries it. This is that
    /// older agent's view, since `Request` is the type it would be using.
    #[test]
    fn a_url_from_a_newer_writer_is_ignorable_rather_than_fatal() {
        let json = r#"{
          "id": 4,
          "host": "buildbox",
          "remote_port": 5173,
          "local_port": 5173,
          "origin": "explicit",
          "created_unix_ms": 1700000000000,
          "open_url": "http://localhost:5173/app"
        }"#;
        let request: Request =
            serde_json::from_str(json).expect("an added optional field must not be fatal");
        assert_eq!(request.local_port, 5173, "the forward still parses");
        assert_eq!(
            request.open_url.as_deref(),
            Some("http://localhost:5173/app")
        );
    }

    #[test]
    fn a_request_with_no_url_serializes_exactly_as_it_did_before_the_field() {
        // An older reader sees the same document it always saw, rather than a
        // null it has no `#[serde(default)]` for.
        let json = serde_json::to_string(&Request::new(1, NOW, spec("buildbox", 5173)))
            .expect("serialize");
        assert!(!json.contains("open_url"), "{json}");
    }

    #[test]
    fn a_url_survives_the_queue_it_travels_in() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        let queued = log
            .enqueue_opening(
                spec("buildbox", 5173),
                NOW,
                Some("http://localhost:5173/app".to_string()),
            )
            .expect("enqueue");
        assert_eq!(
            queued.open_url.as_deref(),
            Some("http://localhost:5173/app")
        );
        let pending = log.pending().expect("read back");
        assert_eq!(pending[0].open_url, queued.open_url);
        // And a plain enqueue still asks for nothing to be opened: only a
        // clicked URL may put one in front of a browser.
        log.enqueue(spec("buildbox", 3000), NOW).expect("enqueue");
        let pending = log.pending().expect("read back");
        assert_eq!(pending[1].open_url, None);
    }

    #[test]
    fn a_request_from_a_writer_that_omitted_the_optional_fields_still_loads() {
        let json = r#"{
          "id": 1,
          "host": "buildbox",
          "remote_port": 5173,
          "local_port": 5173,
          "origin": "explicit",
          "created_unix_ms": 1700000000000
        }"#;
        let request: Request = serde_json::from_str(json).expect("optional fields must default");
        assert_eq!(request.remote_bind, "127.0.0.1", "v1 forwards to loopback");
        assert_eq!(request.pane_id, None);
        assert_eq!(request.plugin_version, "");
        assert_eq!(request.open_url, None, "nothing asked for a browser");
    }

    #[test]
    fn a_request_maps_onto_the_spec_the_apply_side_uses() {
        let request = Request::new(1, NOW, spec("buildbox", 5173));
        assert_eq!(request.to_spec(), spec("buildbox", 5173));
        assert_eq!(
            request.to_spec().forward_arg(),
            "localhost:5173:127.0.0.1:5173"
        );
    }

    #[test]
    fn a_request_is_applicable_up_to_and_including_its_ttl() {
        let request = Request::new(1, NOW, spec("buildbox", 5173));
        assert!(!request.is_expired(NOW, DEFAULT_TTL_MS), "fresh");
        assert!(
            !request.is_expired(NOW + DEFAULT_TTL_MS, DEFAULT_TTL_MS),
            "exactly at the ttl is still applicable"
        );
        assert!(
            request.is_expired(NOW + DEFAULT_TTL_MS + 1, DEFAULT_TTL_MS),
            "one millisecond past the ttl is not"
        );
    }

    #[test]
    fn a_zero_ttl_expires_everything_but_the_current_instant() {
        let request = Request::new(1, NOW, spec("buildbox", 5173));
        assert!(!request.is_expired(NOW, 0));
        assert!(request.is_expired(NOW + 1, 0));
    }

    #[test]
    fn a_clock_skewed_into_the_future_does_not_expire_a_request() {
        // The writer's clock may be ahead of the applier's; that is a reason to
        // treat the request as brand new, not to throw it away.
        let request = Request::new(1, NOW + 60_000, spec("buildbox", 5173));
        assert_eq!(request.age_ms(NOW), 0);
        assert!(!request.is_expired(NOW, DEFAULT_TTL_MS));
    }

    #[test]
    fn an_extreme_timestamp_does_not_overflow_the_age() {
        let request = Request::new(1, 0, spec("buildbox", 5173));
        assert_eq!(request.age_ms(u64::MAX), u64::MAX);
        assert!(request.is_expired(u64::MAX, u64::MAX - 1));
        assert!(!request.is_expired(u64::MAX, u64::MAX));
    }

    #[test]
    fn a_missing_log_has_nothing_pending() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (log, recorder) = recording_log(dir.path());
        assert!(log.pending().expect("read").is_empty());
        assert!(log.consume_through(9).expect("consume").is_empty());
        assert!(recorder.messages().is_empty());
    }

    #[test]
    fn an_empty_log_file_has_nothing_pending() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (log, recorder) = recording_log(dir.path());
        fs::write(log.path(), b"").expect("write empty file");
        assert!(log.pending().expect("read").is_empty());
        assert!(recorder.messages().is_empty());
    }

    #[test]
    fn a_truncated_log_is_a_damaged_read_rather_than_an_empty_queue() {
        // The same distinction the store draws: a queue that could not be read
        // is not a queue with nothing in it. The agent reports "could not read
        // requests" and tries again; concluding there is nothing to apply would
        // drop every request in a file it never saw.
        let dir = tempfile::tempdir().expect("temp dir");
        let (log, recorder) = recording_log(dir.path());
        fs::write(log.path(), br#"{"pending":[{"id":1,"host":"build"#).expect("write");

        let Err(err) = log.pending() else {
            panic!("a queue that could not be read was answered as nothing pending");
        };

        assert!(
            matches!(err, crate::state::StateError::Damaged { .. }),
            "{err:?}: {err}"
        );
        assert!(recorder.messages().is_empty());
    }

    #[test]
    fn a_mutation_over_an_unparseable_log_keeps_the_original_bytes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (log, recorder) = recording_log(dir.path());
        let bytes = b"not json";
        fs::write(log.path(), bytes).expect("write junk");

        log.enqueue(spec("buildbox", 5173), NOW)
            .expect("enqueue must survive a corrupt log");

        assert_eq!(log.pending().expect("read back").len(), 1);
        let quarantined = PathBuf::from(format!("{}.corrupt", log.path().display()));
        assert_eq!(fs::read(&quarantined).expect("bad file kept"), bytes);
        assert_eq!(recorder.messages().len(), 1);
    }

    #[test]
    fn enqueue_assigns_strictly_increasing_ids() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        let first = log.enqueue(spec("buildbox", 5173), NOW).expect("enqueue");
        let second = log.enqueue(spec("buildbox", 3000), NOW).expect("enqueue");
        assert_eq!((first.id, second.id), (1, 2));
    }

    #[test]
    fn ids_keep_increasing_after_a_take() {
        // Reusing an id would let a consumed request be re-applied by a later,
        // unrelated one.
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        let first = log.enqueue(spec("buildbox", 5173), NOW).expect("enqueue");
        log.consume_through(first.id).expect("consume");
        let next = log.enqueue(spec("buildbox", 3000), NOW).expect("enqueue");
        assert_eq!(next.id, 2);
    }

    #[test]
    fn consuming_the_same_request_twice_settles_it_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        log.enqueue(spec("buildbox", 5173), NOW).expect("enqueue");

        let first = log.consume_through(1).expect("consume");
        let second = log.consume_through(1).expect("consume again");

        assert_eq!(first.len(), 1);
        assert!(second.is_empty(), "a second consume must not re-apply");
    }

    #[test]
    fn consuming_settles_everything_at_or_below_the_id_and_nothing_above() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        for port in [5173u16, 3000, 8080] {
            log.enqueue(spec("buildbox", port), NOW).expect("enqueue");
        }

        let settled = log.consume_through(2).expect("consume");

        assert_eq!(
            settled.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 2],
            "only what the applier said it finished with"
        );
        let pending = log.pending().expect("read");
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].id, 3,
            "a request the applier has not reached stays pending"
        );
    }

    #[test]
    fn re_pushing_an_already_consumed_request_is_dropped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        let request = log.enqueue(spec("buildbox", 5173), NOW).expect("enqueue");
        log.consume_through(request.id).expect("consume");

        assert_eq!(log.push(request).expect("push"), Push::Stale);
        assert!(log.pending().expect("read").is_empty());
    }

    #[test]
    fn a_newer_request_for_the_same_port_supersedes_the_pending_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        log.enqueue(spec("buildbox", 5173), NOW).expect("enqueue");

        let mut remapped = spec("buildbox", 5173);
        remapped.origin = Origin::Offered;
        log.enqueue(remapped, NOW + 10).expect("enqueue");

        let pending = log.pending().expect("read");
        assert_eq!(pending.len(), 1, "one port, one pending request");
        assert_eq!(pending[0].origin, Origin::Offered);
        assert_eq!(pending[0].id, 2);
    }

    #[test]
    fn a_request_for_another_host_on_the_same_port_is_separate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        log.enqueue(spec("alpha", 5173), NOW).expect("enqueue");
        log.enqueue(spec("beta", 5173), NOW).expect("enqueue");
        assert_eq!(log.pending().expect("read").len(), 2);
    }

    #[test]
    fn an_out_of_order_push_for_a_pending_port_loses_to_the_newer_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        log.push(Request::new(5, NOW, spec("buildbox", 5173)))
            .expect("push");
        assert_eq!(
            log.push(Request::new(4, NOW - 10, spec("buildbox", 5173)))
                .expect("push"),
            Push::Stale
        );
        assert_eq!(log.pending().expect("read")[0].id, 5);
    }

    #[test]
    fn pushing_the_same_id_twice_leaves_one_request() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        let request = Request::new(1, NOW, spec("buildbox", 5173));
        assert_eq!(log.push(request.clone()).expect("push"), Push::Accepted);
        assert_eq!(log.push(request).expect("push again"), Push::Superseded);
        assert_eq!(log.pending().expect("read").len(), 1);
    }

    #[test]
    fn a_request_the_applier_gave_up_on_cannot_be_pushed_back_in() {
        // The applier settles an expired request the same way it settles an
        // applied one: by naming it. Coming back through a re-write would be a
        // forward the user asked for a quarter of an hour ago.
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        let request = log.enqueue(spec("buildbox", 5173), NOW).expect("enqueue");
        assert!(request.is_expired(NOW + DEFAULT_TTL_MS + 1, DEFAULT_TTL_MS));
        log.consume_through(request.id).expect("consume");

        assert_eq!(log.push(request).expect("push"), Push::Stale);
        assert!(log.pending().expect("read").is_empty());
    }

    #[test]
    fn pending_requests_come_back_in_the_order_they_were_queued() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        for port in [5173u16, 3000, 8080] {
            log.enqueue(spec("buildbox", port), NOW).expect("enqueue");
        }
        let ports: Vec<u16> = log
            .pending()
            .expect("read")
            .iter()
            .map(|r| r.local_port)
            .collect();
        assert_eq!(ports, vec![5173, 3000, 8080]);
    }

    #[test]
    fn consuming_everything_bars_its_return() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        let request = log.enqueue(spec("buildbox", 5173), NOW).expect("enqueue");

        assert_eq!(log.consume_through(u64::MAX).expect("consume").len(), 1);
        assert!(log.pending().expect("read").is_empty());
        assert_eq!(log.push(request).expect("push"), Push::Stale);
    }

    // -- the applier's mark, written from outside this crate ----------------

    /// What the local agent's shell snippet leaves on a remote host: one
    /// decimal integer, written by a rename.
    fn write_ack(log: &RequestLog, mark: u64) {
        fs::write(log.ack_path(), format!("{mark}\n")).expect("write the mark");
    }

    #[test]
    fn a_request_the_applier_marked_applied_is_no_longer_pending() {
        // The bug this exists for: the agent applied the request and forwarded
        // the port, and `status` went on reporting it as waiting for the agent
        // until the file was deleted by hand.
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        let request = log.enqueue(spec("buildbox", 5173), NOW).expect("enqueue");

        write_ack(&log, request.id);

        assert!(
            log.pending().expect("read").is_empty(),
            "an applied request is not waiting for anybody"
        );
        assert_eq!(log.last_consumed_id().expect("read"), request.id);
    }

    #[test]
    fn the_appliers_mark_leaves_later_requests_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        log.enqueue(spec("buildbox", 5173), NOW).expect("enqueue");
        let later = log.enqueue(spec("buildbox", 3000), NOW).expect("enqueue");

        write_ack(&log, 1);

        let pending = log.pending().expect("read");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, later.id);
    }

    #[test]
    fn a_marked_request_cannot_be_re_queued_and_does_not_reuse_its_id() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        let request = log.enqueue(spec("buildbox", 5173), NOW).expect("enqueue");
        write_ack(&log, request.id);

        assert_eq!(
            log.push(request).expect("push"),
            Push::Stale,
            "a mark bars a replay exactly as a local consume does"
        );
        let next = log.enqueue(spec("buildbox", 3000), NOW).expect("enqueue");
        assert_eq!(next.id, 2, "ids stay past everything the applier settled");
    }

    #[test]
    fn a_write_prunes_what_the_applier_has_already_settled() {
        // The applier cannot take this crate's lock from a remote shell, so the
        // file only shrinks when a writer that *can* touches it next.
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        log.enqueue(spec("buildbox", 5173), NOW).expect("enqueue");
        write_ack(&log, 1);

        log.enqueue(spec("buildbox", 3000), NOW).expect("enqueue");

        let raw = fs::read_to_string(log.path()).expect("read the file back");
        assert!(
            !raw.contains("5173"),
            "the settled request is still there:\n{raw}"
        );
        assert!(raw.contains("3000"));
    }

    #[test]
    fn an_unusable_mark_file_reads_as_nothing_applied() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (log, recorder) = recording_log(dir.path());
        log.enqueue(spec("buildbox", 5173), NOW).expect("enqueue");

        for junk in [
            "",
            "   ",
            "not a number",
            "-1",
            "9\n9",
            "18446744073709551616",
        ] {
            fs::write(log.ack_path(), junk).expect("write junk");
            assert_eq!(
                log.pending().expect("read").len(),
                1,
                "{junk:?} must not settle anything"
            );
        }
        assert!(recorder.messages().is_empty(), "and it is not a failure");
    }

    #[test]
    fn duplicate_ids_in_a_hand_written_file_collapse_to_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        fs::write(
            log.path(),
            br#"{
              "version": 1,
              "last_consumed_id": 0,
              "pending": [
                {"id":1,"host":"buildbox","remote_port":5173,"local_port":5173,
                 "origin":"explicit","created_unix_ms":1700000000000},
                {"id":1,"host":"buildbox","remote_port":3000,"local_port":5173,
                 "origin":"offered","created_unix_ms":1700000000001}
              ]
            }"#,
        )
        .expect("write");

        let pending = log.pending().expect("read");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].origin, Origin::Offered, "the last one wins");
    }

    #[test]
    fn an_unknown_top_level_section_does_not_hide_the_pending_requests() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (log, recorder) = recording_log(dir.path());
        fs::write(
            log.path(),
            br#"{
              "version": 42,
              "last_consumed_id": 3,
              "pending": [
                {"id":4,"host":"buildbox","remote_port":5173,"local_port":5173,
                 "origin":"explicit","created_unix_ms":1700000000000}
              ],
              "leases": {"unknown": true}
            }"#,
        )
        .expect("write");

        assert_eq!(log.pending().expect("read").len(), 1);
        assert_eq!(log.last_consumed_id().expect("read"), 3);
        assert!(recorder.messages().is_empty());
    }

    #[test]
    fn atomic_replacement_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = log_in(dir.path());
        for port in 5000..5010u16 {
            log.enqueue(spec("buildbox", port), NOW).expect("enqueue");
        }
        log.consume_through(u64::MAX).expect("consume");

        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "left temp files: {leftovers:?}");
    }

    /// Same discipline as the state file: concurrent writers must not lose a
    /// request, and must not hand out the same id twice.
    #[test]
    fn concurrent_enqueues_lose_no_requests_and_reuse_no_ids() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().to_path_buf();

        let writers = 8u16;
        let per_writer = 16u16;
        let handles: Vec<_> = (0..writers)
            .map(|writer| {
                let path = path.clone();
                thread::spawn(move || {
                    let log = RequestLog::open(&path).expect("open log");
                    for n in 0..per_writer {
                        let port = 20_000 + writer * per_writer + n;
                        log.enqueue(spec("buildbox", port), NOW).expect("enqueue");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("writer thread panicked");
        }

        let pending = log_in(dir.path()).pending().expect("read back");
        assert_eq!(pending.len(), (writers * per_writer) as usize);

        let mut ids: Vec<u64> = pending.iter().map(|r| r.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), pending.len(), "an id was handed out twice");
    }

    /// One consumer among many writers must see each request exactly once.
    #[test]
    fn a_concurrent_consumer_sees_every_request_exactly_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().to_path_buf();
        let total = 120u16;

        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            let log = RequestLog::open(&writer_path).expect("open log");
            for n in 0..total {
                log.enqueue(spec("buildbox", 20_000 + n), NOW)
                    .expect("enqueue");
            }
        });

        // What the agent does: read, decide, then settle exactly what it
        // reached. The two steps are not one lock, so the consume must not take
        // anything the read did not hand over.
        let consumer = log_in(dir.path());
        let mut seen: Vec<u64> = Vec::new();
        let consume = |seen: &mut Vec<u64>| {
            let batch = consumer.pending().expect("read");
            if let Some(highest) = batch.iter().map(|r| r.id).max() {
                consumer.consume_through(highest).expect("consume");
                seen.extend(batch.iter().map(|r| r.id));
            }
        };
        // Bounded so a regression that drops requests fails the test instead of
        // spinning here forever.
        let mut polls = 0;
        while seen.len() < total as usize && polls < 100_000 {
            consume(&mut seen);
            polls += 1;
        }
        writer.join().expect("writer thread panicked");
        consume(&mut seen);

        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), seen.len(), "a request was consumed twice");
        assert_eq!(seen.len(), total as usize, "a request was never consumed");
    }

    #[test]
    fn normalize_keeps_the_newest_request_per_target() {
        let mut pending = vec![
            Request::new(2, NOW, spec("buildbox", 5173)),
            Request::new(1, NOW, spec("buildbox", 5173)),
            Request::new(3, NOW, spec("alpha", 5173)),
        ];
        normalize(&mut pending);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].id, 2);
        assert_eq!(pending[1].id, 3);
    }

    #[test]
    fn normalize_on_an_empty_list_is_a_no_op() {
        let mut pending: Vec<Request> = Vec::new();
        normalize(&mut pending);
        assert!(pending.is_empty());
    }
}
