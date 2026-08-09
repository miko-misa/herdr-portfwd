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

use std::env;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::marker::PhantomData;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::types::{Forward, ForwardSpec, Origin};

/// File holding the recorded forwards, inside the state directory.
pub const STATE_FILE: &str = "forwards.json";

/// Schema version stamped into the file. Readers ignore it: every field is
/// optional on the way in, so an older binary can still read a newer file.
pub const STATE_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("could not {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("could not encode {0} as JSON")]
    Encode(&'static str),
    #[error(
        "no directory to keep {file} in: set HERDR_PLUGIN_STATE_DIR, \
         XDG_STATE_HOME or HOME"
    )]
    NoStateDir { file: &'static str },
    #[error(
        "{path} is not usable as a state directory: {why}. Refusing to keep \
         records where another user could rewrite them"
    )]
    UnsafeStateDir { path: String, why: String },
}

pub type Result<T> = std::result::Result<T, StateError>;

/// Where a degraded read is announced. Warnings go to stderr by default, which
/// Herdr captures per plugin command (`herdr plugin log list`). Tests install
/// their own sink instead of scraping the process's stderr.
pub type Reporter = Arc<dyn Fn(&str) + Send + Sync>;

/// The default sink: stderr, prefixed so it is attributable in a plugin log.
pub fn stderr_reporter() -> Reporter {
    Arc::new(|message: &str| eprintln!("herdr-portfwd: warning: {message}"))
}

/// Directory holding this plugin's durable files.
pub fn state_dir(file: &'static str) -> Result<PathBuf> {
    state_dir_from(
        env::var_os("HERDR_PLUGIN_STATE_DIR"),
        env::var_os("XDG_STATE_HOME"),
        env::var_os("HOME"),
    )
    .ok_or(StateError::NoStateDir { file })
}

/// Resolution rule behind [`state_dir`], separated so it is testable without
/// mutating the process-global environment.
///
/// `None` rather than a fixed path when nothing points anywhere: there is no
/// safe fallback. `/tmp/herdr-portfwd` would be a predictable name in a
/// world-writable directory, which another local user can create first and then
/// read forwards out of, plant requests in, or leave a symlink in the way of.
/// A process launched with no `HOME` — a system-level launchd or systemd unit —
/// is told to configure one instead.
pub fn state_dir_from(
    plugin_state_dir: Option<OsString>,
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(dir) = non_empty(plugin_state_dir) {
        return Some(dir);
    }
    if let Some(dir) = non_empty(xdg_state_home) {
        return Some(dir.join("herdr-portfwd"));
    }
    Some(non_empty(home)?.join(".local/state/herdr-portfwd"))
}

fn non_empty(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The recorded forwards.
///
/// Keyed by `(host, local_port)`: that pair is what a cancel has to name, and
/// two forwards cannot share a local port on the same host.
pub struct Store {
    file: JsonFile<StateDoc>,
}

impl Store {
    /// Open the store in `dir`, creating the directory if needed.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        Self::with_reporter(dir, stderr_reporter())
    }

    /// Same, with a caller-supplied sink for degraded-read warnings.
    pub fn with_reporter(dir: impl AsRef<Path>, reporter: Reporter) -> Result<Self> {
        Ok(Self {
            file: JsonFile::open(dir.as_ref(), STATE_FILE, reporter)?,
        })
    }

    /// Open the store at the location Herdr or XDG points at.
    pub fn from_env() -> Result<Self> {
        Self::open(state_dir(STATE_FILE)?)
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// Every recorded forward, ordered by host then local port.
    pub fn list(&self) -> Result<Vec<Forward>> {
        Ok(self.file.read()?.forwards)
    }

    /// Recorded forwards for one host.
    pub fn list_host(&self, host: &str) -> Result<Vec<Forward>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|f| f.spec.host == host)
            .collect())
    }

    pub fn get(&self, host: &str, local_port: u16) -> Result<Option<Forward>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|f| f.spec.host == host && f.spec.local_port == local_port))
    }

    /// The record forwarding `remote_port` from `host`, whatever local port it
    /// landed on.
    ///
    /// The key of a record is its *local* port, because that is what a cancel
    /// has to name. Discovery asks the other question: it sees a port on the
    /// far side and needs to know whether anything already forwards it. For a
    /// forward the user remapped the two numbers differ, and asking [`get`]
    /// with a remote port finds nothing at all.
    ///
    /// [`get`]: Store::get
    pub fn get_remote(&self, host: &str, remote_port: u16) -> Result<Option<Forward>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|f| f.spec.host == host && f.spec.remote_port == remote_port))
    }

    /// Record `forward`, replacing any record with the same host and local
    /// port. Returns the record it displaced.
    pub fn upsert(&self, forward: Forward) -> Result<Option<Forward>> {
        self.update(move |forwards| {
            let key = (forward.spec.host.clone(), forward.spec.local_port);
            let previous = forwards
                .iter()
                .position(|f| (f.spec.host.clone(), f.spec.local_port) == key)
                .map(|at| forwards.remove(at));
            forwards.push(forward);
            previous
        })
    }

    /// Drop one record. Removing a forward that was never recorded is a no-op,
    /// not an error: reconciliation calls this speculatively (invariant 4).
    pub fn remove(&self, host: &str, local_port: u16) -> Result<Option<Forward>> {
        self.update(|forwards| {
            forwards
                .iter()
                .position(|f| f.spec.host == host && f.spec.local_port == local_port)
                .map(|at| forwards.remove(at))
        })
    }

    /// Drop every record for `host`, returning what was dropped.
    pub fn remove_host(&self, host: &str) -> Result<Vec<Forward>> {
        self.update(|forwards| {
            let mut dropped = Vec::new();
            forwards.retain(|f| {
                if f.spec.host == host {
                    dropped.push(f.clone());
                    false
                } else {
                    true
                }
            });
            dropped
        })
    }

    /// Replace the whole record set.
    ///
    /// Only for a caller that decided what the set should be *under this lock*.
    /// A caller that read earlier and did something slow in between — anything
    /// touching ssh — would throw away every record written meanwhile, which is
    /// invariant 2 broken one level up; [`Store::update`] is what that caller
    /// wants.
    pub fn replace_all(&self, forwards: Vec<Forward>) -> Result<()> {
        self.update(move |current| *current = forwards)
    }

    /// Read-modify-write the record set under one exclusive lock. Everything
    /// above is a thin wrapper over this; callers that need to read and decide
    /// in one atomic step use it directly.
    pub fn update<R>(&self, mutate: impl FnOnce(&mut Vec<Forward>) -> R) -> Result<R> {
        self.file.update(|doc| {
            let out = mutate(&mut doc.forwards);
            doc.version = STATE_VERSION;
            dedupe_by_key(&mut doc.forwards);
            out
        })
    }
}

/// Fold a fresh intent into a record that already covers the same port.
///
/// Adding a forward that is already live is idempotent, but it is not always a
/// no-op: an explicit `add`, or a Ctrl+click on a URL, over a forward the agent
/// opened by itself is the user pinning that port. Left as it was, the record
/// still says the forward belongs to a pane, and `reconcile` cancels it the
/// moment that pane exits — killing the port the user had just asked to keep.
///
/// Only that one upgrade, and never the reverse: an automatic detection over a
/// pinned forward leaves the pin alone. `None` when the record already says
/// everything the new intent does, so the caller can skip a write.
///
/// Only the origin is compared here, so **the caller must first have decided
/// that `existing` really is the record `intent` describes** — same remote port
/// and same remote bind, not merely the same local port. A record is keyed by
/// its local port, and two different forwards can want the same one; folding an
/// intent into the wrong record pins a forward nobody asked to pin and answers
/// the request with someone else's port.
pub fn promoted(existing: &Forward, intent: &ForwardSpec) -> Option<Forward> {
    if intent.origin != Origin::Explicit || existing.spec.origin == Origin::Explicit {
        return None;
    }
    let mut promoted = existing.clone();
    promoted.spec.origin = Origin::Explicit;
    // The pane the user asked from, when there was one. A pinned forward
    // outlives its pane either way; this only keeps the record honest about
    // where the intent came from.
    if intent.pane_id.is_some() {
        promoted.spec.pane_id = intent.pane_id.clone();
    }
    Some(promoted)
}

/// Last writer for a `(host, local_port)` wins, then the set is sorted so the
/// file is byte-stable across processes and diffable by a human.
fn dedupe_by_key(forwards: &mut Vec<Forward>) {
    let mut seen: Vec<(String, u16)> = Vec::new();
    let mut kept: Vec<Forward> = Vec::new();
    for forward in forwards.drain(..).rev() {
        let key = (forward.spec.host.clone(), forward.spec.local_port);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        kept.push(forward);
    }
    kept.sort_by(|a, b| (&a.spec.host, a.spec.local_port).cmp(&(&b.spec.host, b.spec.local_port)));
    *forwards = kept;
}

/// On-disk shape. Every field defaults, so a truncated-but-valid document or a
/// document from another version still loads.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StateDoc {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    forwards: Vec<Forward>,
}

/// A JSON document on disk, with the file discipline every durable record in
/// this crate shares: advisory lock, read, mutate, temp file, fsync, rename.
///
/// Generic because [`crate::request`] needs exactly the same guarantees for a
/// different payload, and two copies of this would drift.
pub struct JsonFile<T> {
    path: PathBuf,
    lock_path: PathBuf,
    reporter: Reporter,
    /// `fn() -> T` rather than `T` so the handle stays `Send + Sync`
    /// independent of the payload.
    _doc: PhantomData<fn() -> T>,
}

impl<T> JsonFile<T>
where
    T: Serialize + DeserializeOwned + Default,
{
    /// Create `dir` if needed and take a handle on `file_name` inside it.
    pub fn open(dir: &Path, file_name: &str, reporter: Reporter) -> Result<Self> {
        ensure_private_dir(dir)?;
        Ok(Self {
            path: dir.join(file_name),
            // A separate lock file, so the data file can be renamed out from
            // under a holder without the lock going with it.
            lock_path: dir.join(format!("{file_name}.lock")),
            reporter,
            _doc: PhantomData,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read under a shared lock. A missing, empty, unreadable, or malformed
    /// file degrades to the default document with a reported warning; it never
    /// panics and never writes.
    pub fn read(&self) -> Result<T> {
        let _lock = FileLock::acquire(&self.lock_path, LockMode::Shared)?;
        Ok(match read_doc(&self.path) {
            Loaded::Value(value) => value,
            Loaded::Absent => T::default(),
            Loaded::Damaged(why) => {
                self.report(&format!(
                    "{} {why}; continuing as if there were no records",
                    self.path.display()
                ));
                T::default()
            }
        })
    }

    /// Read, mutate, and atomically replace under one exclusive lock.
    pub fn update<R>(&self, mutate: impl FnOnce(&mut T) -> R) -> Result<R> {
        let _lock = FileLock::acquire(&self.lock_path, LockMode::Exclusive)?;
        let mut doc = match read_doc(&self.path) {
            Loaded::Value(value) => value,
            Loaded::Absent => T::default(),
            Loaded::Damaged(why) => {
                // Never overwrite bytes we could not parse: the file may belong
                // to something else entirely. Move it aside first, so starting
                // over is recoverable rather than a silent wipe.
                let quarantine = quarantine_path(&self.path);
                fs::rename(&self.path, &quarantine).map_err(|source| StateError::Io {
                    action: "set aside the unreadable file at",
                    path: self.path.display().to_string(),
                    source,
                })?;
                self.report(&format!(
                    "{} {why}; moved it to {} and starting over",
                    self.path.display(),
                    quarantine.display()
                ));
                T::default()
            }
        };
        let out = mutate(&mut doc);
        let mut bytes =
            serde_json::to_vec_pretty(&doc).map_err(|_| StateError::Encode("the document"))?;
        bytes.push(b'\n');
        write_atomically(&self.path, &bytes)?;
        Ok(out)
    }

    fn report(&self, message: &str) {
        (self.reporter)(message);
    }
}

/// Mode a directory this crate creates gets: nobody else's business.
const STATE_DIR_MODE: u32 = 0o700;

/// Create `dir` if it is missing, and refuse to use one somebody else could
/// write through.
///
/// The records here decide which ports this machine opens and which requests it
/// applies, so a directory another user can write is a directory another user
/// can forward through. A directory Herdr itself created is typically `0755`,
/// which is fine — only *write* access by anyone but the owner is refused.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(STATE_DIR_MODE)
        .create(dir)
        .map_err(|source| StateError::Io {
            action: "create the state directory",
            path: dir.display().to_string(),
            source,
        })?;
    let metadata = fs::metadata(dir).map_err(|source| StateError::Io {
        action: "inspect the state directory",
        path: dir.display().to_string(),
        source,
    })?;
    let unsafe_because = |why: String| StateError::UnsafeStateDir {
        path: dir.display().to_string(),
        why,
    };
    if !metadata.is_dir() {
        return Err(unsafe_because("it is not a directory".to_string()));
    }
    // SAFETY: geteuid reads a process property and cannot fail.
    let me = unsafe { libc::geteuid() };
    if metadata.uid() != me {
        return Err(unsafe_because(format!(
            "it belongs to uid {}, not to uid {me}",
            metadata.uid()
        )));
    }
    let mode = metadata.mode() & 0o777;
    if mode & 0o002 != 0 {
        return Err(unsafe_because(format!(
            "its mode is {mode:04o}, which lets anybody write in it"
        )));
    }
    // Group-writable is tightened rather than refused: it is what a `umask 002`
    // login leaves behind, the directory is ours to fix, and refusing would
    // make the plugin unusable on a machine that is set up that way.
    if mode & 0o020 != 0 {
        let mut tightened = metadata.permissions();
        tightened.set_mode(mode & !0o020);
        fs::set_permissions(dir, tightened).map_err(|source| StateError::Io {
            action: "restrict the state directory",
            path: dir.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

enum Loaded<T> {
    Value(T),
    Absent,
    Damaged(String),
}

fn read_doc<T: DeserializeOwned>(path: &Path) -> Loaded<T> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Loaded::Absent,
        Err(err) => return Loaded::Damaged(format!("could not be read ({err})")),
    };
    // A zero-byte file is what an interrupted create leaves behind, and it says
    // the same thing as no file at all. Not worth a warning.
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Loaded::Absent;
    }
    match serde_json::from_slice(&bytes) {
        Ok(value) => Loaded::Value(value),
        Err(err) => Loaded::Damaged(format!("is not valid JSON ({err})")),
    }
}

/// Where a file we could not parse is kept. One slot, not a growing pile: the
/// most recent bad file is the one worth looking at.
fn quarantine_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".corrupt");
    PathBuf::from(name)
}

/// Write a sibling temp file, fsync it, and rename it over `path`. The rename
/// is atomic within a filesystem, so a reader sees either the whole old file or
/// the whole new one, never a truncated mix.
fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let (temp, mut handle) = create_temp_file(path)?;

    let mut write = || -> io::Result<()> {
        handle.write_all(contents)?;
        // fsync before the rename: rename only orders the directory entry, not
        // the data behind it.
        handle.sync_all()
    };
    if let Err(source) = write() {
        let _ = fs::remove_file(&temp);
        return Err(StateError::Io {
            action: "write the temp file",
            path: temp.display().to_string(),
            source,
        });
    }

    if let Err(source) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(StateError::Io {
            action: "replace",
            path: path.display().to_string(),
            source,
        });
    }

    // Best effort: makes the rename itself survive a power loss. Not every
    // filesystem allows opening a directory for this, and failing here does not
    // make the write wrong.
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

/// Create the sibling temp file the new contents are written through.
///
/// `create_new`, never `File::create`: the latter follows a symlink somebody
/// else planted at a guessable name and writes this file's JSON wherever that
/// link points, as us. `O_EXCL` refuses instead, and a fresh name is tried.
fn create_temp_file(path: &Path) -> Result<(PathBuf, File)> {
    create_temp_file_named(path, temp_path)
}

/// The body of [`create_temp_file`], with the name generator injected.
///
/// A test cannot guess a production temp name — an unpredictable one is the
/// whole point of [`temp_path`] — so it supplies its own and plants a symlink
/// there. Without this seam the `O_EXCL` guarantee could only be tested on
/// [`create_new_file`] in isolation, and swapping this function's open for a
/// truncating one would go unnoticed at the call site that actually writes the
/// state.
fn create_temp_file_named(path: &Path, name: impl Fn(&Path) -> PathBuf) -> Result<(PathBuf, File)> {
    let mut last: Option<io::Error> = None;
    for _ in 0..32 {
        let temp = name(path);
        match create_new_file(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => last = Some(err),
            Err(err) => {
                return Err(StateError::Io {
                    action: "create the temp file",
                    path: temp.display().to_string(),
                    source: err,
                })
            }
        }
    }
    Err(StateError::Io {
        action: "find an unused temp name beside",
        path: path.display().to_string(),
        source: last.unwrap_or_else(|| io::Error::other("no attempt was made")),
    })
}

/// `O_CREAT | O_EXCL`, so an existing name — including a symlink somebody else
/// planted — is refused rather than written through.
fn create_new_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// A name for the temp file that another process cannot work out in advance:
/// the pid and a counter alone are guessable, so the clock joins them.
fn temp_path(path: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.subsec_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state".to_string());
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!(
        ".{name}.tmp-{}-{seq}-{nonce:08x}",
        std::process::id()
    ))
}

#[derive(Clone, Copy)]
enum LockMode {
    Shared,
    Exclusive,
}

/// Advisory `flock` on a lock file, released when the guard drops.
///
/// `flock` is per open file description, so two handles in one process contend
/// exactly as two processes do. That is what makes the in-process concurrency
/// test meaningful.
struct FileLock {
    file: File,
}

impl FileLock {
    fn acquire(path: &Path, mode: LockMode) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|source| StateError::Io {
                action: "open the lock file",
                path: path.display().to_string(),
                source,
            })?;
        let operation = match mode {
            LockMode::Shared => libc::LOCK_SH,
            LockMode::Exclusive => libc::LOCK_EX,
        };
        loop {
            // SAFETY: `file` owns the descriptor for the whole call and outlives
            // the lock; flock takes no memory it can invalidate.
            let rc = unsafe { libc::flock(file.as_raw_fd(), operation) };
            if rc == 0 {
                return Ok(Self { file });
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(StateError::Io {
                action: "lock",
                path: path.display().to_string(),
                source: err,
            });
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Closing the descriptor would release it anyway; unlocking explicitly
        // keeps the release in one obvious place.
        // SAFETY: the descriptor is still open until this struct finishes
        // dropping.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::thread;

    use crate::types::{ForwardSpec, Origin};

    /// A reporter that keeps what it was told, so a degraded read can be
    /// asserted on without touching the process's stderr.
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

    fn forward(host: &str, port: u16) -> Forward {
        Forward {
            spec: ForwardSpec {
                host: host.into(),
                remote_port: port,
                local_port: port,
                remote_bind: "127.0.0.1".into(),
                origin: Origin::Explicit,
                pane_id: None,
            },
            created_unix_ms: 1_700_000_000_000,
        }
    }

    fn store_in(dir: &Path) -> Store {
        Store::open(dir).expect("opening a store in a temp dir should work")
    }

    fn remapped(host: &str, remote_port: u16, local_port: u16) -> Forward {
        let mut forward = forward(host, remote_port);
        forward.spec.local_port = local_port;
        forward
    }

    #[test]
    fn a_record_can_be_found_by_the_remote_port_it_forwards() {
        // Discovery only ever sees the port on the far side. Looking a remapped
        // forward up by that number has to find it, or the listener reads as
        // unforwarded and gets forwarded a second time.
        let dir = tempfile::tempdir().expect("temp dir");
        let store = store_in(dir.path());
        store
            .upsert(remapped("buildbox", 5173, 15173))
            .expect("seed");

        assert!(store.get("buildbox", 5173).expect("read").is_none());
        let found = store
            .get_remote("buildbox", 5173)
            .expect("read")
            .expect("the remap is a forward of 5173");
        assert_eq!(found.spec.local_port, 15173);
        assert!(store
            .get_remote("someone-else", 5173)
            .expect("read")
            .is_none());
        assert!(store.get_remote("buildbox", 15173).expect("read").is_none());
    }

    #[test]
    fn an_explicit_intent_pins_a_record_the_agent_made() {
        let mut auto = forward("buildbox", 5173);
        auto.spec.origin = Origin::Auto;
        auto.spec.pane_id = Some("w1:p1".into());
        let mut intent = auto.spec.clone();
        intent.origin = Origin::Explicit;
        intent.pane_id = Some("w2:p9".into());

        let promoted = promoted(&auto, &intent).expect("an explicit add upgrades it");

        assert_eq!(promoted.spec.origin, Origin::Explicit);
        assert_eq!(promoted.spec.pane_id.as_deref(), Some("w2:p9"));
        assert_eq!(
            promoted.created_unix_ms, auto.created_unix_ms,
            "pinning is not a new forward"
        );
    }

    #[test]
    fn nothing_downgrades_a_pinned_record() {
        let pinned = forward("buildbox", 5173);
        for origin in [Origin::Auto, Origin::Offered] {
            let mut intent = pinned.spec.clone();
            intent.origin = origin;
            assert!(
                promoted(&pinned, &intent).is_none(),
                "{origin:?} must not overwrite a pin"
            );
        }
        assert!(
            promoted(&pinned, &pinned.spec).is_none(),
            "and a pin over a pin is not a write"
        );
    }

    #[test]
    fn an_explicit_intent_with_no_pane_keeps_the_one_on_the_record() {
        let mut auto = forward("buildbox", 5173);
        auto.spec.origin = Origin::Auto;
        auto.spec.pane_id = Some("w1:p1".into());
        let mut intent = auto.spec.clone();
        intent.origin = Origin::Explicit;
        intent.pane_id = None;

        let promoted = promoted(&auto, &intent).expect("still an upgrade");

        assert_eq!(promoted.spec.origin, Origin::Explicit);
        assert_eq!(promoted.spec.pane_id.as_deref(), Some("w1:p1"));
    }

    fn recording_store(dir: &Path) -> (Store, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        let store = Store::with_reporter(dir, recorder.reporter())
            .expect("opening a store in a temp dir should work");
        (store, recorder)
    }

    #[test]
    fn state_dir_prefers_the_directory_herdr_injected() {
        let dir = state_dir_from(
            Some("/plugin/state".into()),
            Some("/xdg".into()),
            Some("/home/u".into()),
        );
        assert_eq!(dir, Some(PathBuf::from("/plugin/state")));
    }

    #[test]
    fn state_dir_falls_back_through_xdg_then_home() {
        assert_eq!(
            state_dir_from(None, Some("/xdg".into()), Some("/home/u".into())),
            Some(PathBuf::from("/xdg/herdr-portfwd"))
        );
        assert_eq!(
            state_dir_from(None, None, Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.local/state/herdr-portfwd"))
        );
    }

    #[test]
    fn nowhere_to_put_the_state_is_refused_rather_than_put_in_tmp() {
        // A fixed path in a world-writable directory is one another local user
        // can create first, and then plant requests in or read forwards out of.
        // A daemon started without HOME is told to configure one.
        assert_eq!(state_dir_from(None, None, None), None);
        assert_eq!(state_dir_from(Some("".into()), Some("".into()), None), None);
    }

    #[test]
    fn an_empty_env_value_is_treated_as_unset() {
        // Herdr exports the variable even in contexts where it has nothing to
        // put in it; an empty string must not become the state directory.
        assert_eq!(
            state_dir_from(Some("".into()), Some("".into()), Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.local/state/herdr-portfwd"))
        );
    }

    #[test]
    fn a_state_directory_this_crate_creates_is_private() {
        let root = tempfile::tempdir().expect("temp dir");
        let dir = root.path().join("nested/state");
        let _store = store_in(&dir);
        let mode = fs::metadata(&dir).expect("metadata").mode() & 0o777;
        assert_eq!(mode, 0o700, "created {mode:04o}");
    }

    #[test]
    fn a_directory_anyone_could_write_in_is_refused() {
        // The `/tmp/herdr-portfwd` an attacker pre-creates, or a state dir left
        // world-writable by something else: records there decide which ports
        // this machine opens.
        let root = tempfile::tempdir().expect("temp dir");
        let dir = root.path().join("open-house");
        fs::create_dir(&dir).expect("create dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).expect("chmod");

        let Err(err) = Store::open(&dir) else {
            panic!("a world-writable state dir must be refused");
        };

        assert!(
            matches!(err, StateError::UnsafeStateDir { .. }),
            "{err:?}: {err}"
        );
    }

    #[test]
    fn a_directory_belonging_to_another_user_is_refused() {
        // `HERDR_PLUGIN_STATE_DIR`, or an XDG variable, pointing at somebody
        // else's directory. A 0755 directory trips neither mode check, so the
        // ownership test is the only guard: without it, that user's
        // `forwards.json` and `requests.json` decide which ports this machine
        // opens and which requests the agent applies.
        //
        // SAFETY: geteuid reads a process property and cannot fail.
        let me = unsafe { libc::geteuid() };
        if me == 0 {
            // Running as root, everything is ours; there is nothing to refuse.
            return;
        }
        let owned_by_someone_else = ["/usr", "/"].into_iter().map(Path::new).find(|dir| {
            fs::metadata(dir).is_ok_and(|meta| meta.uid() != me && meta.mode() & 0o022 == 0)
        });
        let Some(dir) = owned_by_someone_else else {
            // No system directory to borrow. Better skipped than asserted on a
            // machine whose layout does not provide one.
            return;
        };

        let Err(err) = Store::open(dir) else {
            panic!(
                "{} belongs to another user and must be refused",
                dir.display()
            );
        };

        let StateError::UnsafeStateDir { why, .. } = err else {
            panic!("{err:?}: the refusal must name the directory, not the file");
        };
        assert!(why.contains("uid"), "the reason must name the owner: {why}");
    }

    #[test]
    fn a_group_writable_directory_is_tightened_rather_than_refused() {
        // What a `umask 002` login leaves behind. The directory is ours, so it
        // is fixable, and refusing would make the plugin unusable on a machine
        // set up that way.
        let root = tempfile::tempdir().expect("temp dir");
        let dir = root.path().join("group-writable");
        fs::create_dir(&dir).expect("create dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).expect("chmod");

        let store = store_in(&dir);
        store.upsert(forward("buildbox", 5173)).expect("upsert");

        let mode = fs::metadata(&dir).expect("metadata").mode() & 0o777;
        assert_eq!(mode & 0o022, 0, "left writable by others: {mode:04o}");
    }

    #[test]
    fn a_planted_symlink_is_refused_rather_than_written_through() {
        // The temp name used to be a pid plus a counter from zero, which
        // another local user can plant a symlink for; `File::create` would then
        // write this file's JSON wherever it pointed, as us.
        let dir = tempfile::tempdir().expect("temp dir");
        let planted = dir.path().join(".forwards.json.tmp-1-0");
        let elsewhere = dir.path().join("victim");
        std::os::unix::fs::symlink(&elsewhere, &planted).expect("plant a symlink");

        let err = create_new_file(&planted).expect_err("an existing name must be refused");

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(
            !elsewhere.exists(),
            "the write followed a symlink out of the state directory"
        );
    }

    #[test]
    fn the_write_path_itself_refuses_a_planted_symlink_rather_than_retrying_past_it() {
        // The test above pins the primitive. This one pins the caller: nothing
        // else binds `O_EXCL` to the function every atomic write goes through,
        // and a plain create there follows the link and writes this file's JSON
        // wherever it points, as us. A production temp name cannot be guessed,
        // so the name generator is the one supplied here — every attempt lands
        // on the planted name, leaving the caller nowhere to retreat to.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(STATE_FILE);
        let planted = dir.path().join(".forwards.json.tmp-planted");
        let elsewhere = dir.path().join("victim");
        std::os::unix::fs::symlink(&elsewhere, &planted).expect("plant a symlink");

        let outcome = create_temp_file_named(&path, |_| planted.clone());

        assert!(
            outcome.is_err(),
            "a symlink at the temp name was accepted as a temp file"
        );
        assert!(
            !elsewhere.exists(),
            "the state write followed a symlink out of the state directory"
        );
    }

    #[test]
    fn the_temp_file_every_write_goes_through_is_a_fresh_private_sibling() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(STATE_FILE);

        let (temp, file) = create_temp_file(&path).expect("a fresh temp name is available");

        assert_eq!(temp.parent(), path.parent(), "renamed across a filesystem");
        let planted = fs::symlink_metadata(&temp).expect("the temp file exists");
        assert!(
            planted.file_type().is_file(),
            "the temp path is not a regular file: {planted:?}"
        );
        let mode = file.metadata().expect("metadata").mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the state passes through this file: {mode:04o}"
        );
    }

    #[test]
    fn temp_names_are_not_guessable_and_never_repeat() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(STATE_FILE);
        let names: Vec<PathBuf> = (0..16).map(|_| temp_path(&path)).collect();

        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "a temp name was reused");
        for name in &names {
            let name = name.to_string_lossy();
            assert!(
                name.matches('-').count() >= 3,
                "a pid and a counter alone can be worked out in advance: {name}"
            );
        }

        let (used, _handle) = create_temp_file(&path).expect("a temp file is created");
        assert!(used.exists());
    }

    #[test]
    fn a_missing_state_file_reads_as_no_forwards_without_warning() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, recorder) = recording_store(dir.path());
        assert!(store.list().expect("read should succeed").is_empty());
        assert!(recorder.messages().is_empty(), "absence is not a problem");
        assert!(!store.path().exists(), "a read must not create the file");
    }

    #[test]
    fn an_empty_state_file_reads_as_no_forwards_without_warning() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, recorder) = recording_store(dir.path());
        fs::write(store.path(), b"").expect("write empty file");
        assert!(store.list().expect("read should succeed").is_empty());
        assert!(recorder.messages().is_empty());
    }

    #[test]
    fn whitespace_only_state_file_reads_as_no_forwards() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, recorder) = recording_store(dir.path());
        fs::write(store.path(), b"\n\n  \t\n").expect("write blank file");
        assert!(store.list().expect("read should succeed").is_empty());
        assert!(recorder.messages().is_empty());
    }

    #[test]
    fn truncated_json_degrades_to_no_forwards_and_reports_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, recorder) = recording_store(dir.path());
        fs::write(store.path(), br#"{"version":1,"forwards":[{"host":"build"#)
            .expect("write truncated file");

        assert!(
            store
                .list()
                .expect("a corrupt file must not be fatal")
                .is_empty(),
            "a truncated file means no known records, not a panic"
        );
        assert_eq!(recorder.messages().len(), 1, "the degradation is reported");
    }

    #[test]
    fn a_read_never_touches_a_file_it_could_not_parse() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _recorder) = recording_store(dir.path());
        let bytes = b"this is somebody else's file";
        fs::write(store.path(), bytes).expect("write foreign file");

        let _ = store.list().expect("read should degrade, not fail");

        assert_eq!(
            fs::read(store.path()).expect("file still there"),
            bytes,
            "reading must never rewrite a file it did not understand"
        );
    }

    #[test]
    fn a_mutation_over_an_unparseable_file_keeps_the_original_bytes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, recorder) = recording_store(dir.path());
        let bytes = b"{ not json at all";
        fs::write(store.path(), bytes).expect("write junk");

        store
            .upsert(forward("buildbox", 5173))
            .expect("a mutation must survive a corrupt file");

        assert_eq!(store.list().expect("read back").len(), 1);
        let quarantined = quarantine_path(store.path());
        assert_eq!(
            fs::read(&quarantined).expect("the bad file is set aside, not deleted"),
            bytes
        );
        assert_eq!(recorder.messages().len(), 1);
    }

    #[test]
    fn unknown_fields_in_a_record_are_ignored() {
        // A newer plugin may add fields; an older binary must still read the
        // records it does understand.
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, recorder) = recording_store(dir.path());
        fs::write(
            store.path(),
            br#"{
              "version": 99,
              "forwards": [
                {
                  "host": "buildbox",
                  "remote_port": 5173,
                  "local_port": 5173,
                  "remote_bind": "127.0.0.1",
                  "origin": "explicit",
                  "pane_id": null,
                  "created_unix_ms": 1700000000000,
                  "tls_fingerprint": "from a newer writer"
                }
              ],
              "future_section": {"anything": true}
            }"#,
        )
        .expect("write forward-compatible file");

        let forwards = store.list().expect("unknown fields must not be fatal");
        assert_eq!(forwards.len(), 1);
        assert_eq!(forwards[0].spec.remote_port, 5173);
        assert!(recorder.messages().is_empty(), "this file is not corrupt");
    }

    #[test]
    fn records_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = store_in(dir.path());
        store.upsert(forward("buildbox", 5173)).expect("upsert");

        let reopened = store_in(dir.path());
        assert_eq!(
            reopened.list().expect("read back"),
            vec![forward("buildbox", 5173)]
        );
    }

    #[test]
    fn upsert_replaces_the_record_with_the_same_host_and_local_port() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = store_in(dir.path());
        store
            .upsert(forward("buildbox", 5173))
            .expect("first upsert");

        let mut remapped = forward("buildbox", 5173);
        remapped.spec.remote_port = 3000;
        remapped.spec.origin = Origin::Offered;
        let previous = store.upsert(remapped.clone()).expect("second upsert");

        assert_eq!(previous, Some(forward("buildbox", 5173)));
        assert_eq!(store.list().expect("read back"), vec![remapped]);
    }

    #[test]
    fn the_same_local_port_on_another_host_is_a_separate_record() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = store_in(dir.path());
        store.upsert(forward("alpha", 5173)).expect("upsert alpha");
        store.upsert(forward("beta", 5173)).expect("upsert beta");
        assert_eq!(store.list().expect("read back").len(), 2);
    }

    #[test]
    fn removing_a_forward_that_was_never_recorded_is_a_no_op() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = store_in(dir.path());
        assert_eq!(
            store
                .remove("buildbox", 5173)
                .expect("remove must not error"),
            None
        );
    }

    #[test]
    fn remove_host_drops_only_that_hosts_records() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = store_in(dir.path());
        store.upsert(forward("alpha", 5173)).expect("upsert");
        store.upsert(forward("alpha", 3000)).expect("upsert");
        store.upsert(forward("beta", 5173)).expect("upsert");

        let dropped = store.remove_host("alpha").expect("remove host");
        assert_eq!(dropped.len(), 2);
        assert_eq!(
            store.list().expect("read back"),
            vec![forward("beta", 5173)]
        );
    }

    #[test]
    fn replace_all_overwrites_the_whole_set() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = store_in(dir.path());
        store.upsert(forward("alpha", 5173)).expect("upsert");
        store
            .replace_all(vec![forward("beta", 8080)])
            .expect("replace");
        assert_eq!(
            store.list().expect("read back"),
            vec![forward("beta", 8080)]
        );
    }

    #[test]
    fn duplicate_keys_written_by_hand_collapse_to_the_last_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = store_in(dir.path());
        let mut second = forward("buildbox", 5173);
        second.created_unix_ms = 1_800_000_000_000;
        store
            .replace_all(vec![forward("buildbox", 5173), second.clone()])
            .expect("replace");
        assert_eq!(store.list().expect("read back"), vec![second]);
    }

    #[test]
    fn port_numbers_at_the_edges_of_the_range_survive_a_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = store_in(dir.path());
        store.upsert(forward("buildbox", 0)).expect("upsert 0");
        store
            .upsert(forward("buildbox", 65535))
            .expect("upsert max");

        let ports: Vec<u16> = store
            .list()
            .expect("read back")
            .iter()
            .map(|f| f.spec.local_port)
            .collect();
        assert_eq!(ports, vec![0, 65535]);
    }

    #[test]
    fn atomic_replacement_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = store_in(dir.path());
        for port in 5000..5010 {
            store.upsert(forward("buildbox", port)).expect("upsert");
        }
        store.remove("buildbox", 5000).expect("remove");

        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "left temp files: {leftovers:?}");
    }

    /// Invariant 2. A startup hook, an event hook, an action, and the local
    /// agent all write at once; not one record may be lost to a lost update.
    #[test]
    fn concurrent_writers_lose_no_records() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().to_path_buf();

        let writers = 8;
        let per_writer = 16;
        let handles: Vec<_> = (0..writers)
            .map(|writer| {
                let path = path.clone();
                thread::spawn(move || {
                    // A separate Store per thread, exactly as separate
                    // processes would each open their own.
                    let store = Store::open(&path).expect("open store");
                    for n in 0..per_writer {
                        let port = 20_000 + (writer * per_writer + n) as u16;
                        store.upsert(forward("buildbox", port)).expect("upsert");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("writer thread panicked");
        }

        let forwards = store_in(dir.path()).list().expect("read back");
        assert_eq!(
            forwards.len(),
            (writers * per_writer) as usize,
            "a concurrent write dropped records"
        );
    }

    /// The same, with every writer aimed at the same key: the file must end up
    /// with exactly one record, not a duplicate or a torn one.
    #[test]
    fn concurrent_writers_on_one_key_converge_to_a_single_record() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().to_path_buf();

        let handles: Vec<_> = (0..8)
            .map(|writer| {
                let path = path.clone();
                thread::spawn(move || {
                    let store = Store::open(&path).expect("open store");
                    for _ in 0..16 {
                        let mut record = forward("buildbox", 5173);
                        record.created_unix_ms = 1_700_000_000_000 + writer;
                        store.upsert(record).expect("upsert");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("writer thread panicked");
        }

        assert_eq!(store_in(dir.path()).list().expect("read back").len(), 1);
    }

    #[test]
    fn a_reader_racing_writers_never_sees_a_partial_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().to_path_buf();

        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            let store = Store::open(&writer_path).expect("open store");
            for port in 0..200u16 {
                store
                    .upsert(forward("buildbox", 30_000 + port))
                    .expect("upsert");
            }
        });

        let (reader, recorder) = recording_store(dir.path());
        for _ in 0..200 {
            reader.list().expect("a read must never fail mid-write");
        }
        writer.join().expect("writer thread panicked");

        assert!(
            recorder.messages().is_empty(),
            "a reader saw a torn file: {:?}",
            recorder.messages()
        );
    }
}
