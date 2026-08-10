//! Plugin pane rendering.
//!
//! Three entrypoints: a `list` overlay of live forwards with stop keys, a
//! `picker` popup over attributed listeners, and a `confirm` popup that says
//! what a Ctrl+clicked URL is about to do before it does it. All are plain ANSI
//! on a terminal Herdr owns — no TUI framework, matching how other Herdr
//! plugins stay small.
//!
//! A popup receives all terminal input including Escape and closes when the
//! command exits, so key handling is ordinary stdin reading.
//!
//! # Reaching the other panes
//!
//! A pane can send the user to another one: `confirm` offers `l` and `s`, and
//! `list` offers `a`. The UI never opens anything itself — it returns a
//! [`Handoff`] in its [`Outcome`] and the caller asks Herdr. That is not
//! squeamishness about I/O: *when* the ask happens is the whole design. See
//! [`Handoff`].
//!
//! Rendering is a pure function from a snapshot to bytes, so layout, column
//! widths, and truncation are testable without a terminal.
//!
//! Three things drive the shape of this module:
//!
//! - **Columns are measured in terminal cells, not bytes or chars.** Workspace
//!   labels here are frequently Japanese, and `str::len` or `chars().count()`
//!   misaligns every column to the right of one. [`display_width`] is the only
//!   measure used.
//! - **The chrome is ASCII.** `…`, `→`, `▸` and friends are East Asian
//!   *ambiguous* width: a terminal configured for a CJK locale renders them two
//!   cells wide, which would break the alignment this module exists to keep.
//!   Truncation is marked with a trailing `+`, as `top` does.
//! - **Every value is sanitized before it is drawn.** Workspace labels, process
//!   names, and agent names come off a remote host. A raw escape sequence in
//!   one would let the far side drive the local terminal, so control bytes are
//!   dropped in [`sanitize`] rather than passed through.

use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};

use crate::confirm::ConfirmState;
use crate::types::{AttributedListener, Forward, Liveness, Origin, PaneRef};

/// Fallback terminal geometry when the real size cannot be read.
pub const DEFAULT_WIDTH: usize = 80;
pub const DEFAULT_HEIGHT: usize = 24;

/// Below this, column layout stops being meaningful; render at this width and
/// let the terminal wrap rather than producing empty columns.
pub const MIN_WIDTH: usize = 32;

/// Lines a frame spends on chrome: title, blank, header, blank, status, keys.
const CHROME_LINES: usize = 6;

/// Appended to a truncated cell. ASCII on purpose: see the module docs.
const TRUNCATION_MARK: char = '+';

/// Space between columns.
const GAP: usize = 2;

// ---------------------------------------------------------------------------
// Text measurement
// ---------------------------------------------------------------------------

/// East Asian Wide and Fullwidth ranges, sorted and non-overlapping.
///
/// Ambiguous-width characters are deliberately absent: they are one cell in a
/// Western locale and two in a CJK one, and no table can be right for both. The
/// chrome avoids them entirely so the only ambiguity left is inside user data,
/// where a one-cell miss is cosmetic rather than structural.
const WIDE_RANGES: &[(u32, u32)] = &[
    (0x1100, 0x115F),   // Hangul Jamo initial consonants
    (0x2329, 0x232A),   // angle brackets
    (0x2E80, 0x303E),   // CJK radicals through ideographic punctuation
    (0x3041, 0x33FF),   // kana, Bopomofo, compatibility blocks
    (0x3400, 0x4DBF),   // CJK extension A
    (0x4E00, 0x9FFF),   // CJK unified ideographs
    (0xA000, 0xA4CF),   // Yi
    (0xA960, 0xA97F),   // Hangul Jamo extended-A
    (0xAC00, 0xD7A3),   // Hangul syllables
    (0xF900, 0xFAFF),   // CJK compatibility ideographs
    (0xFE10, 0xFE19),   // vertical forms
    (0xFE30, 0xFE6F),   // CJK compatibility forms
    (0xFF00, 0xFF60),   // fullwidth forms
    (0xFFE0, 0xFFE6),   // fullwidth signs
    (0x17000, 0x18AFF), // Tangut
    (0x1B000, 0x1B001), // kana supplement
    (0x1F300, 0x1F64F), // pictographs and emoticons
    (0x1F680, 0x1F6FF), // transport symbols
    (0x1F900, 0x1F9FF), // supplemental pictographs
    (0x1FA70, 0x1FAFF), // symbols extended-A
    (0x20000, 0x2FFFD), // CJK extension B and beyond
    (0x30000, 0x3FFFD),
];

/// Combining and other zero-advance characters, sorted and non-overlapping.
const ZERO_WIDTH_RANGES: &[(u32, u32)] = &[
    (0x0300, 0x036F),   // combining diacriticals
    (0x0483, 0x0489),   // Cyrillic combining
    (0x0591, 0x05BD),   // Hebrew points
    (0x0610, 0x061A),   // Arabic marks
    (0x064B, 0x065F),   // Arabic marks
    (0x0670, 0x0670),   // Arabic superscript alef
    (0x06D6, 0x06DC),   // Arabic marks
    (0x200B, 0x200F),   // zero-width space and directional marks
    (0x20D0, 0x20F0),   // combining marks for symbols
    (0xFE00, 0xFE0F),   // variation selectors
    (0xFE20, 0xFE2F),   // combining half marks
    (0xE0100, 0xE01EF), // variation selectors supplement
];

fn in_ranges(ranges: &[(u32, u32)], value: u32) -> bool {
    ranges
        .binary_search_by(|&(lo, hi)| {
            if value < lo {
                std::cmp::Ordering::Greater
            } else if value > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Terminal cells one character occupies.
///
/// Control characters report 0 because [`sanitize`] removes them; a caller that
/// measures unsanitized text still gets a width that matches what a terminal
/// would advance.
pub fn char_width(ch: char) -> usize {
    let value = ch as u32;
    if ch.is_control() {
        return 0;
    }
    if in_ranges(ZERO_WIDTH_RANGES, value) {
        return 0;
    }
    if in_ranges(WIDE_RANGES, value) {
        return 2;
    }
    1
}

/// Terminal cells a string occupies.
///
/// Escape sequences count as nothing, because they are instructions rather than
/// glyphs. That matters for padding an already-painted line: a row carrying a
/// colour code is still exactly as wide as the text inside it.
///
/// Truncation is a different question — [`truncate_to_width`] would happily cut
/// a sequence in half — so it is only ever applied to sanitized user text,
/// before any painting.
pub fn display_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            // Skip to the sequence's final byte. `[` and `O` introduce one
            // rather than ending it, so they do not count as terminators.
            for next in chars.by_ref() {
                if matches!(next, '\u{40}'..='\u{7e}') && next != '[' && next != 'O' {
                    break;
                }
            }
            continue;
        }
        width += char_width(ch);
    }
    width
}

/// Drop what a terminal would interpret rather than display.
///
/// A workspace label or process name arrives from a remote host, so an escape
/// sequence embedded in one is remote input aimed at the local terminal. Bidi
/// overrides go too: they reorder a rendered line without changing its width,
/// which is exactly how a port number can be made to read as another one.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|ch| {
            !ch.is_control()
                && !matches!(
                    *ch as u32,
                    0x200B..=0x200F | 0x2028..=0x202E | 0x2066..=0x2069 | 0xFEFF
                )
        })
        .collect()
}

/// Cut `text` to at most `max` cells, marking that something was removed.
///
/// Never splits a wide character: when the last character that fits would
/// straddle the boundary it is dropped, so the result can be one cell short of
/// `max`. Callers pad afterwards, which is why [`pad_to_width`] exists.
pub fn truncate_to_width(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max - 1;
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = char_width(ch);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push(TRUNCATION_MARK);
    out
}

/// Pad on the right to `width` cells. Text already at or over `width` is
/// returned unchanged; truncation is a separate decision.
pub fn pad_to_width(text: &str, width: usize) -> String {
    let have = display_width(text);
    let mut out = text.to_string();
    for _ in have..width {
        out.push(' ');
    }
    out
}

/// Cut an already-painted line to `max` cells.
///
/// [`truncate_to_width`] is only ever handed sanitized text, so it may cut
/// anywhere; a finished frame line has colour codes in it, and cutting one in
/// half would send the tail of an escape sequence to the terminal as text. This
/// copies every sequence through whole, counts only glyphs, and closes with a
/// reset when it cut a painted line, so a colour opened before the cut cannot
/// leak into the rest of the pane.
///
/// A frame that overflowed its pane would wrap, and a wrapped line costs a row
/// the height budget already spent — the status line alone can carry an ssh
/// error of any length — so every line goes through here on the way in.
fn clamp_line(line: &str, max: usize) -> String {
    if display_width(line) <= max {
        return line.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max - 1;
    let mut out = String::new();
    let mut used = 0;
    let mut painted = false;
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            painted = true;
            out.push(ch);
            for next in chars.by_ref() {
                out.push(next);
                if matches!(next, '\u{40}'..='\u{7e}') && next != '[' && next != 'O' {
                    break;
                }
            }
            continue;
        }
        let w = char_width(ch);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push(TRUNCATION_MARK);
    if painted {
        out.push_str(RESET);
    }
    out
}

/// Pad on the left to `width` cells.
pub fn pad_left(text: &str, width: usize) -> String {
    let have = display_width(text);
    let mut out = String::new();
    for _ in have..width {
        out.push(' ');
    }
    out.push_str(text);
    out
}

/// A left-aligned column cell: sanitized, truncated, padded.
fn cell(text: &str, width: usize) -> String {
    pad_to_width(&truncate_to_width(&sanitize(text), width), width)
}

/// A right-aligned column cell, for numbers.
fn rcell(text: &str, width: usize) -> String {
    pad_left(&truncate_to_width(&sanitize(text), width), width)
}

fn widest<'a, I: Iterator<Item = &'a str>>(values: I, floor: usize, ceiling: usize) -> usize {
    values
        .map(|value| display_width(&sanitize(value)))
        .fold(floor, usize::max)
        .min(ceiling)
}

// ---------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const REVERSE: &str = "\x1b[7m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";

fn paint(text: &str, code: &str, color: bool) -> String {
    if color {
        format!("{code}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// Re-enter reverse video after every reset inside `line`.
///
/// `RESET` clears *all* attributes, reverse video included, so a highlighted row
/// would stop being highlighted at its first painted cell — the yellow local
/// port of a remapped forward closes with a reset, and everything right of it
/// would render as an ordinary row. SGR has no portable "reset colour but keep
/// the rest", so the row re-asserts the highlight immediately after each reset
/// instead, with no glyph in between.
fn keep_reverse(line: &str) -> String {
    line.replace(RESET, &format!("{RESET}{REVERSE}"))
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

/// One line of feedback under the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub text: String,
    pub error: bool,
}

impl Status {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: false,
        }
    }

    pub fn failed(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: true,
        }
    }
}

/// A recorded forward, flattened for display.
///
/// A *recorded* one, not a live one. The two are different questions and the
/// answer to the second travels with the row: dropping it is how the manager
/// came to draw a forward whose master had died exactly like one that was up,
/// and to count it in a header that said `live`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRow {
    pub host: String,
    pub remote_port: u16,
    pub local_port: u16,
    /// Workspace label when Herdr gave one, else the workspace id.
    pub workspace: Option<String>,
    pub agent: Option<String>,
    pub pane_id: Option<String>,
    pub origin: Origin,
    /// Whether the record describes a port that is actually open. Carried
    /// rather than assumed, and three-valued: see [`Liveness`].
    pub live: Liveness,
}

impl ForwardRow {
    /// Build from a stored record, the pane it was attributed to if that pane
    /// is still in the current snapshot, and what the listing found out about
    /// the port.
    pub fn from_forward(forward: &Forward, pane: Option<&PaneRef>, live: Liveness) -> Self {
        Self {
            host: forward.spec.host.clone(),
            remote_port: forward.spec.remote_port,
            local_port: forward.spec.local_port,
            workspace: pane.map(pane_workspace),
            agent: pane.and_then(|pane| pane.agent.clone()),
            pane_id: forward
                .spec
                .pane_id
                .clone()
                .or_else(|| pane.map(|pane| pane.pane_id.clone())),
            origin: forward.spec.origin,
            live,
        }
    }

    /// Whether the local port was moved off the remote one. Rendered as its own
    /// note because a remapped port is what breaks URLs that embed it.
    pub fn is_remapped(&self) -> bool {
        self.local_port != self.remote_port
    }
}

/// A remote listener offered for forwarding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerRow {
    pub remote_port: u16,
    /// Local port that would be opened. Equal to `remote_port` unless the user
    /// already accepted a remap.
    pub local_port: u16,
    pub bind: String,
    pub process: String,
    pub workspace: Option<String>,
    pub agent: Option<String>,
    pub pane_id: Option<String>,
    /// A forward for this port already exists, so Enter would be a no-op.
    pub already_forwarded: bool,
    pub marked: bool,
}

impl ListenerRow {
    pub fn from_attributed(
        attributed: &AttributedListener,
        local_port: u16,
        already_forwarded: bool,
    ) -> Self {
        Self {
            remote_port: attributed.listener.port,
            local_port,
            bind: attributed.listener.bind.clone(),
            process: attributed.listener.process.clone(),
            workspace: attributed.pane.as_ref().map(pane_workspace),
            agent: attributed.pane.as_ref().and_then(|pane| pane.agent.clone()),
            pane_id: attributed.pane.as_ref().map(|pane| pane.pane_id.clone()),
            already_forwarded,
            marked: false,
        }
    }

    pub fn is_remapped(&self) -> bool {
        self.local_port != self.remote_port
    }
}

fn pane_workspace(pane: &PaneRef) -> String {
    pane.workspace_label
        .clone()
        .filter(|label| !label.trim().is_empty())
        .unwrap_or_else(|| pane.workspace_id.clone())
}

/// The pane a forward belongs to, if it is still around.
pub fn find_pane<'a>(panes: &'a [PaneRef], pane_id: Option<&str>) -> Option<&'a PaneRef> {
    let wanted = pane_id?;
    panes.iter().find(|pane| pane.pane_id == wanted)
}

/// Whether a table's rows are a listing at all.
///
/// An empty table has two causes that look identical in a `Vec` and are
/// opposite answers to the user's question. *There is nothing* is a fact about
/// every host. *I could not find out* is a fact about this process — the host
/// name did not resolve, the store would not open — and says nothing whatever
/// about what is live. A screen that cannot tell them apart states the first
/// while the second is true, which is how
/// `HERDR_PORTFWD_HOST=stale-name … pane-ui list` came to answer "No forwards
/// are live on any host." over two open ports.
///
/// So the snapshot carries which one it is, and every wording that makes a
/// claim about hosts — the header's count and scope, the empty table's
/// sentence, and the statuses keys write when they find no rows — is gated on
/// it. The CLI has never had this problem: `list --host gone-host` exits
/// non-zero and refuses to print a world.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Listing {
    /// The rows are what the store and the hosts actually reported.
    #[default]
    Fetched,
    /// No listing was obtained. The rows are empty because nothing was learned.
    Unavailable,
}

impl Listing {
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Listing::Unavailable)
    }
}

/// Which host's forwards the manager is drawing.
///
/// The pane lists every host, because that is what is live; the filter is how a
/// user with several hosts reads one of them at a time. It is a property of the
/// screen, never of what exists — the snapshot always carries every row, so the
/// header can say how many are being hidden and an emptied view can say the
/// world is not empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HostFilter {
    #[default]
    All,
    Host(String),
}

impl HostFilter {
    pub fn host(&self) -> Option<&str> {
        match self {
            HostFilter::All => None,
            HostFilter::Host(name) => Some(name.as_str()),
        }
    }

    fn keeps(&self, row: &ForwardRow) -> bool {
        self.host().is_none_or(|only| only == row.host)
    }
}

/// How many rows are live, how many are not, and how many nobody checked.
///
/// Every number and every sentence the manager writes about forwards is built
/// from one of these rather than from `rows.len()`. A count of records is not a
/// count of live forwards: `list_title` printing the one under the word `live`
/// is how a forward whose master had died was announced as up, and
/// `"N live on other hosts"` said the same thing about the hosts it was hiding.
///
/// Three fields rather than a live/dead pair, because [`Liveness`] has a third
/// value and it is the common one under `herdr --remote`: this process is not
/// the machine that binds the port, so nothing here checked. A wording that
/// collapses that into either of the other two invents an answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub live: usize,
    pub missing: usize,
    pub unknown: usize,
}

impl Tally {
    /// Count `rows` by what is known about each.
    pub fn of<'a, I: IntoIterator<Item = &'a ForwardRow>>(rows: I) -> Self {
        let mut tally = Self::default();
        for row in rows {
            match row.live {
                Liveness::Live => tally.live += 1,
                Liveness::Missing => tally.missing += 1,
                Liveness::Unknown => tally.unknown += 1,
            }
        }
        tally
    }

    pub fn total(&self) -> usize {
        self.live + self.missing + self.unknown
    }

    /// How a screen says this out loud: `2 live`, `1 live, 1 not live`,
    /// `3 not checked`.
    ///
    /// Only the non-zero parts are named, so the ordinary case — everything is
    /// up — still reads as the plain `N live` it always did, and the stale
    /// record that used to hide inside that number now has its own clause. No
    /// rows at all is `0 live`: a listing that arrived and held nothing is a
    /// fact about liveness, unlike one that never arrived, which never reaches
    /// here (see [`ListSnapshot::unlisted`]).
    pub fn phrase(&self) -> String {
        let parts: Vec<String> = [
            (self.live, Liveness::Live),
            (self.missing, Liveness::Missing),
            (self.unknown, Liveness::Unknown),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, live)| format!("{count} {}", liveness_word(live)))
        .collect();
        if parts.is_empty() {
            return format!("0 {}", liveness_word(Liveness::Live));
        }
        parts.join(", ")
    }
}

/// The word the manager uses for a liveness, in the header's counts and in the
/// mark on the row itself.
///
/// One function for both so a row and the number above it cannot describe the
/// same record differently. Deliberately a phrase rather than the CLI's
/// one-word column (`live` / `missing` / `unknown`): a pane has no column
/// header to say what the word is about, so `not checked` has to carry that on
/// its own.
fn liveness_word(live: Liveness) -> &'static str {
    match live {
        Liveness::Live => "live",
        Liveness::Missing => "not live",
        Liveness::Unknown => "not checked",
    }
}

/// Hosts with a forward, each once, in the order they first appear.
pub fn host_cycle(rows: &[ForwardRow]) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for row in rows {
        if !hosts.contains(&row.host) {
            hosts.push(row.host.clone());
        }
    }
    hosts
}

/// Where `h` moves the filter next: all hosts, then each host that has a
/// forward, then back to all hosts.
///
/// A filter naming a host that no longer has one — its last forward was just
/// stopped — lands back on "all hosts" rather than on a neighbour the user
/// never chose, and so does a cycle with nothing left to narrow.
pub fn next_host_filter(current: &HostFilter, rows: &[ForwardRow]) -> HostFilter {
    let hosts = host_cycle(rows);
    if hosts.len() < 2 {
        return HostFilter::All;
    }
    match current {
        HostFilter::All => HostFilter::Host(hosts[0].clone()),
        HostFilter::Host(name) => match hosts.iter().position(|host| host == name) {
            Some(index) if index + 1 < hosts.len() => HostFilter::Host(hosts[index + 1].clone()),
            _ => HostFilter::All,
        },
    }
}

/// What pressing `h` does to a snapshot.
///
/// Returned rather than applied so the decision is a pure function of what is
/// on screen — the same shape the rendering has — and so the one case that must
/// write nothing at all is a case rather than an omission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCycle {
    /// Move the filter here.
    Filter(HostFilter),
    /// Nothing to move it to. Say this, because a key that redraws an identical
    /// frame reads as a key that does not work.
    Say(String),
    /// Nothing to move it to, and nothing this key is entitled to say: the
    /// table is empty because no listing arrived, and the status already
    /// explains that. Any replacement would be a claim about hosts the pane
    /// never reached, and would cost the user the one line that named the
    /// actual problem.
    Keep,
}

/// Where `h` leaves the snapshot. See [`HostCycle`].
pub fn host_cycle_effect(snapshot: &ListSnapshot) -> HostCycle {
    if snapshot.unlisted() {
        return HostCycle::Keep;
    }
    let hosts = host_cycle(&snapshot.rows);
    if snapshot.filter == HostFilter::All && hosts.len() < 2 {
        return HostCycle::Say(match hosts.first() {
            Some(host) => format!("every forward is on {host}"),
            None => "there is nothing to filter".to_string(),
        });
    }
    HostCycle::Filter(next_host_filter(&snapshot.filter, &snapshot.rows))
}

/// Everything `render_list` needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListSnapshot {
    pub title: String,
    /// Every live forward, on every host. What is *drawn* is
    /// [`ListSnapshot::shown`]; `selected` and `top` index that.
    pub rows: Vec<ForwardRow>,
    /// Whether [`ListSnapshot::rows`] is a listing or the absence of one. See
    /// [`Listing`]; read through [`ListSnapshot::unlisted`].
    pub listing: Listing,
    pub filter: HostFilter,
    pub selected: usize,
    /// First row on screen. What the pane remembers of its own scrolling; see
    /// [`scroll_top`] for why it is remembered rather than recomputed.
    pub top: usize,
    pub status: Option<Status>,
    pub width: usize,
    pub height: usize,
    pub color: bool,
}

impl Default for ListSnapshot {
    fn default() -> Self {
        Self {
            title: "Port forwards".to_string(),
            rows: Vec::new(),
            listing: Listing::Fetched,
            filter: HostFilter::All,
            selected: 0,
            top: 0,
            status: None,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            color: true,
        }
    }
}

impl ListSnapshot {
    pub fn new(rows: Vec<ForwardRow>) -> Self {
        Self {
            rows,
            ..Self::default()
        }
    }

    /// Whether the empty table means "nothing was learned" rather than "nothing
    /// is live".
    ///
    /// Only an empty table can mean it: rows on screen are a listing whatever
    /// happened since, so a refresh that failed over a table the pane is
    /// already holding leaves that table speaking for itself and puts its own
    /// reason on the status line.
    pub fn unlisted(&self) -> bool {
        self.listing.is_unavailable() && self.rows.is_empty()
    }

    /// The rows the filter lets through — what the table draws, what the keys
    /// act on, and what `X` means by "shown".
    pub fn shown(&self) -> Vec<ForwardRow> {
        self.rows
            .iter()
            .filter(|row| self.filter.keeps(row))
            .cloned()
            .collect()
    }

    fn selected_row(&self) -> Option<ForwardRow> {
        let shown = self.shown();
        shown
            .get(self.selected.min(shown.len().saturating_sub(1)))
            .cloned()
    }
}

/// Everything `render_picker` needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerSnapshot {
    pub host: String,
    pub rows: Vec<ListenerRow>,
    /// Whether [`PickerSnapshot::rows`] is a listing or the absence of one. See
    /// [`Listing`]; read through [`PickerSnapshot::unlisted`].
    pub listing: Listing,
    pub selected: usize,
    /// First row on screen; see [`ListSnapshot::top`].
    pub top: usize,
    pub status: Option<Status>,
    pub width: usize,
    pub height: usize,
    pub color: bool,
}

impl Default for PickerSnapshot {
    fn default() -> Self {
        Self {
            host: String::new(),
            rows: Vec::new(),
            listing: Listing::Fetched,
            selected: 0,
            top: 0,
            status: None,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            color: true,
        }
    }
}

impl PickerSnapshot {
    pub fn new(host: impl Into<String>, rows: Vec<ListenerRow>) -> Self {
        Self {
            host: host.into(),
            rows,
            ..Self::default()
        }
    }

    /// Whether the empty table means "nothing was learned" rather than "no
    /// listener here belongs to a pane". See [`ListSnapshot::unlisted`].
    pub fn unlisted(&self) -> bool {
        self.listing.is_unavailable() && self.rows.is_empty()
    }

    /// Rows the user marked, or the highlighted one when nothing is marked.
    /// Enter with no marks is the common case and must still forward something.
    pub fn chosen(&self) -> Vec<ListenerRow> {
        let marked: Vec<ListenerRow> = self.rows.iter().filter(|row| row.marked).cloned().collect();
        if !marked.is_empty() {
            return marked;
        }
        match self.rows.get(self.selected) {
            Some(row) => vec![row.clone()],
            None => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Selection and scrolling, both pure
// ---------------------------------------------------------------------------

/// Move the selection by `delta`.
///
/// A single step wraps, because a short list is the normal case and stopping at
/// the ends there feels broken. A page jump clamps: wrapping a page in a
/// five-row list lands somewhere unrelated to the key that was pressed.
pub fn step_selection(selected: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let len_i = len as isize;
    let current = (selected.min(len - 1)) as isize;
    // Saturating rather than wrapping: `delta` is public input, and an
    // overflowing add would panic in a debug build on a keystroke.
    let target = current.saturating_add(delta);
    if delta.unsigned_abs() == 1 {
        target.rem_euclid(len_i) as usize
    } else {
        target.clamp(0, len_i - 1) as usize
    }
}

/// Where the table is scrolled to after `selected` is brought back into view.
///
/// The offset is carried in the snapshot rather than derived from the selection
/// alone, because a formula over `selected` has to pick a place for a selection
/// that is off screen, and any fixed choice pins the highlight there: anchoring
/// it to the bottom row means walking up from the end of a long list scrolls the
/// whole table on every keypress while the highlight never moves. Moving the
/// offset by the least it takes is what makes the highlight travel the pane
/// first and the table scroll only once it reaches an edge.
pub fn scroll_top(top: usize, len: usize, selected: usize, capacity: usize) -> usize {
    if len == 0 || capacity == 0 {
        return 0;
    }
    let selected = selected.min(len - 1);
    let last = len.saturating_sub(capacity);
    let top = top.min(last);
    if selected < top {
        return selected;
    }
    if selected >= top + capacity {
        return (selected + 1 - capacity).min(last);
    }
    top
}

/// Half-open range of rows to draw: the pane's own offset, corrected by
/// [`scroll_top`] so `selected` is always on screen even when the offset is
/// stale — rendering stays a pure function of the snapshot it is handed.
pub fn visible_window(top: usize, len: usize, selected: usize, capacity: usize) -> (usize, usize) {
    if len == 0 || capacity == 0 {
        return (0, 0);
    }
    let start = scroll_top(top, len, selected, capacity);
    (start, (start + capacity).min(len))
}

fn row_capacity(height: usize) -> usize {
    height.saturating_sub(CHROME_LINES).max(1)
}

/// Rows a frame of this height can draw, allowing for the overflow line it adds
/// when the table does not fit. Shared with the drivers so the offset they keep
/// is measured against the window that is actually drawn.
fn rows_on_screen(height: usize, count: usize) -> usize {
    let capacity = row_capacity(height);
    if count > capacity {
        capacity.saturating_sub(1).max(1)
    } else {
        capacity
    }
}

fn origin_label(origin: Origin) -> &'static str {
    match origin {
        Origin::Explicit => "explicit",
        Origin::Offered => "offered",
        Origin::Auto => "auto",
    }
}

fn or_dash(value: Option<&String>) -> &str {
    match value {
        Some(text) if !text.trim().is_empty() => text,
        _ => "-",
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

struct Frame {
    lines: Vec<String>,
    width: usize,
    color: bool,
}

impl Frame {
    fn new(width: usize, color: bool) -> Self {
        Self {
            lines: Vec::new(),
            width,
            color,
        }
    }

    /// Every line the frame carries goes through here, so nothing reaches the
    /// pane wider than the pane is: a line that overflows wraps, and a wrapped
    /// line costs a row nothing budgeted for.
    fn push(&mut self, line: String) {
        self.lines.push(clamp_line(line.trim_end(), self.width));
    }

    fn blank(&mut self) {
        self.lines.push(String::new());
    }

    /// A selected row is padded to the full width first so reverse video covers
    /// the whole line rather than stopping at the last glyph, and its inner
    /// resets are patched so the highlight covers every cell rather than
    /// stopping at the first painted one.
    fn push_row(&mut self, line: String, selected: bool) {
        if selected && self.color {
            let padded = pad_to_width(&clamp_line(&line, self.width), self.width);
            self.lines
                .push(format!("{REVERSE}{}{RESET}", keep_reverse(&padded)));
        } else {
            self.push(line);
        }
    }

    fn push_status(&mut self, status: Option<&Status>) {
        match status {
            Some(status) => {
                let text = sanitize(&status.text);
                let code = if status.error { RED } else { DIM };
                self.push(paint(&text, code, self.color));
            }
            None => self.blank(),
        }
    }

    fn finish(self) -> String {
        self.lines.join("\n")
    }
}

fn header_line(text: &str, color: bool) -> String {
    paint(text.trim_end(), BOLD, color)
}

fn overflow_line(above: usize, below: usize) -> String {
    match (above, below) {
        (0, 0) => String::new(),
        (0, below) => format!("  {below} more below"),
        (above, 0) => format!("  {above} more above"),
        (above, below) => format!("  {above} more above, {below} more below"),
    }
}

/// The notes a forward row carries after its last column, with the colour each
/// is painted in.
///
/// One function so the width the layout reserves and the text the row draws
/// cannot drift apart, and so the row that is not live cannot be drawn
/// identically to the row that is. Liveness comes first because it is the one
/// that decides whether the row describes an open port at all; `remapped` is a
/// property of a forward that exists.
///
/// A live forward is marked by nothing, which is the point: the header counts
/// it, and a table where every row carries a badge has no marks left for the
/// row that needs one.
fn list_notes(row: &ForwardRow) -> Vec<(String, &'static str)> {
    let mut notes = Vec::new();
    match row.live {
        Liveness::Live => {}
        // Red: the record claims a port that is not open, which is the state a
        // user has to act on — a master that died under them.
        Liveness::Missing => notes.push((liveness_word(row.live).to_string(), RED)),
        // Dim: nothing is wrong, this is simply not the machine that knows.
        Liveness::Unknown => notes.push((liveness_word(row.live).to_string(), DIM)),
    }
    if row.is_remapped() {
        notes.push(("remapped".to_string(), YELLOW));
    }
    notes
}

/// Cells a row's notes occupy once joined, separators included.
fn notes_width(notes: &[(String, &'static str)]) -> usize {
    let text: usize = notes.iter().map(|(text, _)| display_width(text)).sum();
    text + notes.len().saturating_sub(1)
}

/// The notes as one painted cell.
fn painted_notes(notes: &[(String, &'static str)], color: bool) -> String {
    notes
        .iter()
        .map(|(text, code)| paint(text, code, color))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Column widths for the forward table, with the workspace column absorbing
/// whatever is left over.
struct ListLayout {
    host: usize,
    remote: usize,
    local: usize,
    workspace: usize,
    agent: usize,
    origin: usize,
}

impl ListLayout {
    fn compute(rows: &[ForwardRow], width: usize) -> Self {
        let host = widest(
            std::iter::once("HOST").chain(rows.iter().map(|row| row.host.as_str())),
            4,
            18,
        );
        let agent = widest(
            std::iter::once("AGENT").chain(rows.iter().filter_map(|row| row.agent.as_deref())),
            5,
            12,
        );
        let remote = 6;
        let local = 5;
        let origin = 8;
        // Everything `row` emits other than the workspace column, counted
        // exactly once each: the marker, the five other columns, the five gaps
        // between the six columns, and — only when a row carries one — the gap
        // and cells of the trailing notes. Leaving the notes out here spends
        // their cells on the workspace column and starts them past the right
        // edge, where they wrap and cost a line the frame had budgeted.
        let note = rows
            .iter()
            .map(|row| notes_width(&list_notes(row)))
            .max()
            .unwrap_or(0);
        let trailing = if note == 0 { 0 } else { GAP + note };
        let fixed = 2 + host + remote + local + agent + origin + GAP * 5 + trailing;
        let workspace = width.saturating_sub(fixed).clamp(8, 40);
        Self {
            host,
            remote,
            local,
            workspace,
            agent,
            origin,
        }
    }

    fn header(&self) -> String {
        let gap = " ".repeat(GAP);
        format!(
            "  {}{gap}{}{gap}{}{gap}{}{gap}{}{gap}{}",
            cell("HOST", self.host),
            rcell("REMOTE", self.remote),
            rcell("LOCAL", self.local),
            cell("WORKSPACE", self.workspace),
            cell("AGENT", self.agent),
            cell("ORIGIN", self.origin),
        )
    }

    fn row(&self, row: &ForwardRow, selected: bool, color: bool) -> String {
        let gap = " ".repeat(GAP);
        let marker = if selected { "> " } else { "  " };
        let local = rcell(&row.local_port.to_string(), self.local);
        let local = if row.is_remapped() && color {
            paint(&local, YELLOW, true)
        } else {
            local
        };
        let note = painted_notes(&list_notes(row), color);
        format!(
            "{marker}{}{gap}{}{gap}{}{gap}{}{gap}{}{gap}{}{gap}{}",
            cell(&row.host, self.host),
            rcell(&row.remote_port.to_string(), self.remote),
            local,
            cell(or_dash(row.workspace.as_ref()), self.workspace),
            cell(or_dash(row.agent.as_ref()), self.agent),
            cell(origin_label(row.origin), self.origin),
            note,
        )
    }
}

/// The manager's first line: what is on screen, and whether that is everything.
///
/// The header this replaced read `Port forwards (1 live)` whether one forward
/// existed or one was being shown out of several, which is the whole of the bug
/// this pane had: nothing on screen distinguished the two.
///
/// The count that replaced it was still a count of *records* wearing the word
/// `live`, so a forward whose master had died was announced as up by the same
/// screen that offered to stop it. It is a [`Tally`] now: the number says what
/// was checked and found, and a record nobody could check says that instead of
/// borrowing the word.
///
/// A listing that never arrived gets no number at all: `0 live` is a count of
/// what is live, and `all hosts` is a claim to have looked at them. Both were
/// false on a screen that had reached no host at all.
fn list_title(snapshot: &ListSnapshot, shown: &[ForwardRow]) -> String {
    let title = sanitize(&snapshot.title);
    if snapshot.unlisted() {
        return format!("{title}  (listing unavailable)");
    }
    let phrase = Tally::of(shown).phrase();
    match &snapshot.filter {
        HostFilter::All => format!("{title}  ({phrase}, all hosts)"),
        // The total is `recorded`, not `live`: the rows the filter is hiding
        // were never checked against this header's question, and several of
        // them may be exactly the stale records the user is looking for.
        HostFilter::Host(name) => format!(
            "{title}  ({phrase} of {} recorded, host {} only)",
            snapshot.rows.len(),
            sanitize(name)
        ),
    }
}

/// Draw the forward manager. Pure: same snapshot, same bytes.
pub fn render_list(snapshot: &ListSnapshot) -> String {
    let width = snapshot.width.max(MIN_WIDTH);
    let mut frame = Frame::new(width, snapshot.color);
    let rows = snapshot.shown();
    let count = rows.len();
    frame.push(paint(&list_title(snapshot, &rows), BOLD, snapshot.color));
    frame.blank();

    if snapshot.unlisted() {
        // No listing arrived, so this pane reached no host and knows nothing
        // about any of them. Every other sentence on this screen would be a
        // report about hosts it never got to; the reason is on the status line
        // and `r` is the key that could change the answer.
        frame.push("Could not list the forwards.".to_string());
        frame.push("Nothing here says whether any are live. Press r to try again.".to_string());
        frame.blank();
        frame.push_status(snapshot.status.as_ref());
        frame.push(paint(
            &["r refresh", "a add", "q close"].join(HINT_GAP),
            DIM,
            snapshot.color,
        ));
        return frame.finish();
    }

    if rows.is_empty() {
        // A filtered view with nothing in it is not an empty world, and saying
        // "No forwards are live" there would be the same lie the old header
        // told, from the other side.
        match &snapshot.filter {
            HostFilter::Host(name) if !snapshot.rows.is_empty() => {
                frame.push(format!("No forward on {}.", sanitize(name)));
                // The hidden rows, counted by what is known about each. Their
                // number under the word "live" was the header's own mistake
                // one line down, and it is the sentence a user reads when
                // deciding whether pressing `h` is worth it.
                frame.push(format!(
                    "{} on other hosts. Press h to show every host.",
                    Tally::of(
                        snapshot
                            .rows
                            .iter()
                            .filter(|row| !snapshot.filter.keeps(row))
                    )
                    .phrase()
                ));
                frame.blank();
                frame.push_status(snapshot.status.as_ref());
                frame.push(paint(
                    &["h host", "a add", "q close"].join(HINT_GAP),
                    DIM,
                    snapshot.color,
                ));
            }
            _ => {
                // "No forwards are live" was a claim about ports, drawn from
                // the absence of records. What this screen knows is that it
                // read the records and there were none.
                frame.push("No forwards are recorded on any host.".to_string());
                frame.push(
                    "Ctrl+click a localhost URL in a pane, or press a to pick one.".to_string(),
                );
                frame.blank();
                frame.push_status(snapshot.status.as_ref());
                // The two keys an empty table still has. Spelled out rather
                // than taken from `LIST_HINTS`, whose other rows act on a row
                // that is not there.
                frame.push(paint(
                    &["a add", "q close"].join(HINT_GAP),
                    DIM,
                    snapshot.color,
                ));
            }
        }
        return frame.finish();
    }

    let layout = ListLayout::compute(&rows, width);
    frame.push(header_line(&layout.header(), snapshot.color));

    let capacity = rows_on_screen(snapshot.height, count);
    let selected = snapshot.selected.min(count - 1);
    let (start, end) = visible_window(snapshot.top, count, selected, capacity);
    for (index, row) in rows[start..end].iter().enumerate() {
        let index = start + index;
        frame.push_row(
            layout.row(row, index == selected, snapshot.color),
            index == selected,
        );
    }
    let overflow = overflow_line(start, count - end);
    if !overflow.is_empty() {
        frame.push(paint(&overflow, DIM, snapshot.color));
    }

    frame.blank();
    frame.push_status(snapshot.status.as_ref());
    frame.push(paint(&hint_line(&LIST_HINTS), DIM, snapshot.color));
    frame.finish()
}

/// The notes a listener row carries after its last column, with the colour each
/// is painted in. One function so the width the layout reserves and the text the
/// row draws cannot drift apart.
fn picker_notes(row: &ListenerRow) -> Vec<(String, &'static str)> {
    let mut notes = Vec::new();
    if row.already_forwarded {
        notes.push(("forwarded".to_string(), DIM));
    }
    if row.is_remapped() {
        notes.push((format!("local {} remapped", row.local_port), YELLOW));
    }
    notes
}

/// Cells [`picker_notes`] occupies once joined, separators included.
fn picker_notes_width(row: &ListenerRow) -> usize {
    notes_width(&picker_notes(row))
}

/// Column widths for the listener table.
struct PickerLayout {
    port: usize,
    bind: usize,
    process: usize,
    workspace: usize,
    agent: usize,
}

impl PickerLayout {
    fn compute(rows: &[ListenerRow], width: usize) -> Self {
        let bind = widest(
            std::iter::once("BIND").chain(rows.iter().map(|row| row.bind.as_str())),
            4,
            12,
        );
        let process = widest(
            std::iter::once("PROCESS").chain(rows.iter().map(|row| row.process.as_str())),
            7,
            16,
        );
        let agent = widest(
            std::iter::once("AGENT").chain(rows.iter().filter_map(|row| row.agent.as_deref())),
            5,
            12,
        );
        let port = 5;
        // As in `ListLayout::compute`: the marker, the four other columns, the
        // four gaps between the five columns, and the trailing notes with their
        // own gap when a row carries any.
        let note = rows.iter().map(picker_notes_width).max().unwrap_or(0);
        let trailing = if note == 0 { 0 } else { GAP + note };
        let fixed = 3 + port + bind + process + agent + GAP * 4 + trailing;
        let workspace = width.saturating_sub(fixed).clamp(8, 40);
        Self {
            port,
            bind,
            process,
            workspace,
            agent,
        }
    }

    fn header(&self) -> String {
        let gap = " ".repeat(GAP);
        format!(
            "   {}{gap}{}{gap}{}{gap}{}{gap}{}",
            rcell("PORT", self.port),
            cell("BIND", self.bind),
            cell("PROCESS", self.process),
            cell("WORKSPACE", self.workspace),
            cell("AGENT", self.agent),
        )
    }

    fn row(&self, row: &ListenerRow, selected: bool, color: bool) -> String {
        let gap = " ".repeat(GAP);
        let marker = format!(
            "{}{} ",
            if selected { ">" } else { " " },
            if row.marked { "*" } else { " " },
        );
        format!(
            "{marker}{}{gap}{}{gap}{}{gap}{}{gap}{}{gap}{}",
            rcell(&row.remote_port.to_string(), self.port),
            cell(&row.bind, self.bind),
            cell(&row.process, self.process),
            cell(or_dash(row.workspace.as_ref()), self.workspace),
            cell(or_dash(row.agent.as_ref()), self.agent),
            painted_notes(&picker_notes(row), color),
        )
    }
}

/// Draw the listener picker. Pure: same snapshot, same bytes.
pub fn render_picker(snapshot: &PickerSnapshot) -> String {
    let width = snapshot.width.max(MIN_WIDTH);
    let mut frame = Frame::new(width, snapshot.color);
    let host = sanitize(&snapshot.host);
    let title = if host.is_empty() {
        "Forward a port".to_string()
    } else {
        format!("Forward a port from {host}")
    };
    frame.push(paint(&title, BOLD, snapshot.color));
    frame.blank();

    if snapshot.unlisted() {
        // The same distinction the manager draws: a scan that never ran has not
        // established that no listener belongs to a pane, and the host in the
        // title may be the very thing that could not be resolved.
        frame.push(if host.is_empty() {
            "Could not list the listening ports.".to_string()
        } else {
            format!("Could not list the listening ports on {host}.")
        });
        frame.push("Nothing here says which ports are open. Press r to try again.".to_string());
        frame.blank();
        frame.push_status(snapshot.status.as_ref());
        frame.push(paint(
            &["r refresh", "q close"].join(HINT_GAP),
            DIM,
            snapshot.color,
        ));
        return frame.finish();
    }

    if snapshot.rows.is_empty() {
        frame.push("No listening port on this host belongs to a Herdr pane.".to_string());
        frame.push(
            "Start a dev server in a pane, or use \"Port: forward a port\" to name one."
                .to_string(),
        );
        frame.blank();
        frame.push_status(snapshot.status.as_ref());
        frame.push(paint("q close", DIM, snapshot.color));
        return frame.finish();
    }

    let layout = PickerLayout::compute(&snapshot.rows, width);
    frame.push(header_line(&layout.header(), snapshot.color));

    let count = snapshot.rows.len();
    let capacity = rows_on_screen(snapshot.height, count);
    let selected = snapshot.selected.min(count - 1);
    let (start, end) = visible_window(snapshot.top, count, selected, capacity);
    for (index, row) in snapshot.rows[start..end].iter().enumerate() {
        let index = start + index;
        frame.push_row(
            layout.row(row, index == selected, snapshot.color),
            index == selected,
        );
    }
    let overflow = overflow_line(start, count - end);
    if !overflow.is_empty() {
        frame.push(paint(&overflow, DIM, snapshot.color));
    }

    frame.blank();
    frame.push_status(snapshot.status.as_ref());
    frame.push(paint(&hint_line(&PICKER_HINTS), DIM, snapshot.color));
    frame.finish()
}

// ---------------------------------------------------------------------------
// The confirmation screen
// ---------------------------------------------------------------------------

/// Everything `render_confirm` needs.
///
/// The click itself is carried whole rather than flattened, because it is the
/// same value the asking side encoded into the environment: one shape means the
/// screen cannot describe a click the payload does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmSnapshot {
    pub state: ConfirmState,
    pub status: Option<Status>,
    pub width: usize,
    pub height: usize,
    pub color: bool,
}

impl Default for ConfirmSnapshot {
    fn default() -> Self {
        Self {
            state: ConfirmState::default(),
            status: None,
            width: DEFAULT_WIDTH,
            height: CONFIRM_HEIGHT,
            color: true,
        }
    }
}

impl ConfirmSnapshot {
    pub fn new(state: ConfirmState) -> Self {
        Self {
            state,
            ..Self::default()
        }
    }
}

/// Rows the tallest confirmation frame needs — the height of the pane process,
/// not of the popup around it. Kept here so
/// `the_confirm_popup_is_declared_tall_enough_for_its_own_tallest_frame` can
/// check the manifest against the renderer rather than against a guess.
pub const CONFIRM_HEIGHT: usize = 16;

/// Rows a `[[panes]] height` spends on the popup's border before the pane
/// process sees any.
///
/// Herdr's `height` is the **outer** size. `resolve_popup_geometry` in
/// `src/popup_size.rs` derives the terminal it spawns as
/// `pane_inner_height = outer_height.saturating_sub(2)` and hands that to
/// `spawn_popup_command` as `rows`, so a popup declared `height = N` runs its
/// command in `N - 2` rows. Declaring [`CONFIRM_HEIGHT`] itself was a budget
/// the pane never had, and the two rows it was short are the last two of the
/// frame: `esc cancel` and the status line.
pub const POPUP_BORDER_ROWS: usize = 2;

/// Cells the key column of the confirmation's footer occupies.
const CONFIRM_KEY_WIDTH: usize = 8;

/// A title with a rule after it, filling the pane.
///
/// ASCII `-` rather than a box-drawing `─`: U+2500 is East Asian *ambiguous*,
/// two cells wide in a CJK locale, and a rule that changes length with the
/// reader's locale is the one piece of chrome guaranteed to overflow.
fn ruled_title(title: &str, width: usize) -> String {
    let title = sanitize(title);
    let used = display_width(&title) + 1;
    if used >= width {
        return title;
    }
    format!("{title} {}", "-".repeat(width - used))
}

/// The parenthesised note after the port mapping, and the colour it is painted
/// in. One function so the wording and the colour cannot drift from the state
/// that produced them.
fn confirm_note(state: &ConfirmState) -> Option<(String, &'static str)> {
    if state.already_forwarded == Some(true) {
        return Some(("already forwarded".to_string(), DIM));
    }
    match state.local_free {
        Some(true) => Some(("free".to_string(), DIM)),
        Some(false) => Some(("in use".to_string(), RED)),
        // The asking side was on the far end of the connection and could not
        // know. Saying "free" there would be inventing an answer; saying "in
        // use" would be inventing the opposite.
        None => None,
    }
}

/// The sentences under the mapping. Each is a fact about *this* click that the
/// two-line summary above cannot carry.
///
/// Kept to one short line each rather than one long one, because these are what
/// [`fit_confirm`] gives up first when the popup is too small: a sentence that
/// survives as half a sentence says less than one that was never drawn.
fn confirm_notices(state: &ConfirmState) -> Vec<(String, &'static str)> {
    let mut notices = Vec::new();
    if let Some(problem) = &state.problem {
        notices.push((sanitize(problem), RED));
        notices.push(("Nothing will be forwarded.".to_string(), DIM));
        return notices;
    }
    if state.port_taken() {
        notices.push((
            format!("Local port {} is held by something else.", state.local_port),
            RED,
        ));
        notices.push((
            "No remap is offered: the port in the URL is the point.".to_string(),
            DIM,
        ));
    }
    if state.already_forwarded == Some(true) {
        notices.push((
            "The forward is already up; enter just opens it.".to_string(),
            DIM,
        ));
    }
    if !state.applies_here {
        notices.push((
            "This Herdr server is not the machine with the browser.".to_string(),
            DIM,
        ));
        notices.push((
            "The local agent applies it and opens the page.".to_string(),
            DIM,
        ));
    }
    notices
}

/// The footer rows this particular click offers, as `(key, text)`.
///
/// The forward keys are dropped when there is nothing to forward, because a
/// footer that promises a key which cannot work is worse than one that stays
/// quiet: the user presses it, nothing happens, and the screen has lied.
fn confirm_hints(state: &ConfirmState) -> Vec<(&'static str, &'static str)> {
    let mut hints: Vec<(&'static str, &'static str)> = Vec::new();
    if state.actionable() {
        for (key, text, _, command) in CONFIRM_HINTS {
            match command {
                ConfirmCommand::ForwardAndOpen if state.already_forwarded == Some(true) => {
                    hints.push((key, "open in the browser"));
                }
                // Nothing left to forward, so the row would name a no-op. Only
                // a *known* existing forward drops it: an unknown one is not
                // evidence there is nothing to do.
                ConfirmCommand::ForwardOnly if state.already_forwarded == Some(true) => {}
                ConfirmCommand::ForwardAndOpen | ConfirmCommand::ForwardOnly => {
                    hints.push((key, text));
                }
                _ => {}
            }
        }
    }
    for (key, text, _, command) in CONFIRM_HINTS {
        if matches!(
            command,
            ConfirmCommand::ShowList | ConfirmCommand::ShowPicker | ConfirmCommand::Quit
        ) {
            hints.push((key, text));
        }
    }
    hints
}

/// One row of the confirmation, and whether a cramped popup may drop it.
///
/// Only the explanatory sentences are droppable. The mapping, the keys and the
/// status are what the screen is *for*: a popup too short to hold everything
/// must still say what will happen and how to say no.
struct ConfirmLine {
    text: String,
    droppable: bool,
}

fn essential(text: impl Into<String>) -> ConfirmLine {
    ConfirmLine {
        text: text.into(),
        droppable: false,
    }
}

fn optional(text: impl Into<String>) -> ConfirmLine {
    ConfirmLine {
        text: text.into(),
        droppable: true,
    }
}

/// Drop the rows a short popup has no room for, least important first.
///
/// A frame taller than its pane wraps, and a wrapped line costs a row the
/// budget already spent — the same reason every line goes through
/// [`clamp_line`] on the way in. So something has to go, and the order is:
/// blank rows, then the explanatory sentences from the bottom up, and only then
/// — when the rows that may not be dropped are by themselves taller than the
/// pane — the *head*.
///
/// Cutting the tail was the obvious implementation and the wrong one twice
/// over. The last rows of this frame are `esc cancel` and the status line, so a
/// `truncate` took the status first and `esc cancel` next: the two rows this
/// screen exists to show. Dropping from the front loses the title and then the
/// mapping, which are a caption on an answer the user can still act on, rather
/// than the answer itself and the way out.
fn fit_confirm(mut lines: Vec<ConfirmLine>, height: usize) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    while lines.len() > height {
        let blank = lines.iter().rposition(|line| line.text.trim().is_empty());
        let victim = blank.or_else(|| lines.iter().rposition(|line| line.droppable));
        match victim {
            Some(index) => {
                lines.remove(index);
            }
            // Nothing left that may be dropped, so the cut comes off the head.
            None => {
                lines.remove(0);
            }
        }
    }
    lines.into_iter().map(|line| line.text).collect()
}

/// Break `text` into lines of at most `width` cells, on spaces.
///
/// Measured in cells like everything else here, so a sentence carrying a
/// Japanese workspace label wraps where a reader sees it wrap. A single word
/// wider than the budget gets its own line and is cut by [`clamp_line`] on the
/// way into the frame; hyphenating it would invent characters that are not in
/// the value.
fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if display_width(&candidate) <= width || current.is_empty() {
            current = candidate;
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Draw the confirmation. Pure: same snapshot, same bytes.
pub fn render_confirm(snapshot: &ConfirmSnapshot) -> String {
    let width = snapshot.width.max(MIN_WIDTH);
    let state = &snapshot.state;
    let color = snapshot.color;
    // Every row is indented two cells, so a sentence has that much less to
    // wrap into.
    let body = width.saturating_sub(2).max(1);
    let mut lines: Vec<ConfirmLine> = Vec::new();

    let host = sanitize(state.host.as_deref().unwrap_or_default());
    let title = if host.is_empty() {
        "Forward a port".to_string()
    } else {
        format!("Forward a port from {host}")
    };
    lines.push(essential(paint(&ruled_title(&title, width), BOLD, color)));
    lines.push(essential(String::new()));

    // Who it belongs to. Absent when the click could not be attributed, which
    // is a normal outcome and not worth a row of dashes.
    let owner: Vec<String> = [state.workspace.as_ref(), state.agent.as_ref()]
        .into_iter()
        .flatten()
        .map(|value| sanitize(value))
        .filter(|value| !value.trim().is_empty())
        .collect();
    if !owner.is_empty() {
        lines.push(optional(format!("  {}", owner.join("    "))));
    }

    let mapping = format!(
        "  remote {}  ->  localhost {}",
        state.remote_port, state.local_port
    );
    let mapping = match confirm_note(state) {
        Some((note, code)) => format!("{mapping}   {}", paint(&format!("({note})"), code, color)),
        None => mapping,
    };
    lines.push(essential(mapping));
    lines.push(optional(paint(
        &format!("  {}", sanitize(&state.url)),
        DIM,
        color,
    )));

    let notices = confirm_notices(state);
    if !notices.is_empty() {
        lines.push(optional(String::new()));
    }
    for (text, code) in notices {
        for wrapped in wrap_to_width(&text, body) {
            lines.push(optional(paint(&format!("  {wrapped}"), code, color)));
        }
    }

    lines.push(essential(String::new()));
    for (key, text) in confirm_hints(state) {
        lines.push(essential(format!(
            "  {}{text}",
            pad_to_width(key, CONFIRM_KEY_WIDTH)
        )));
    }
    lines.push(essential(String::new()));
    match &snapshot.status {
        // Wrapped rather than clamped to one line, unlike the tables above.
        // This is the only place a refusal lands on this screen, and the one
        // that matters most — the local-port collision — is three clauses long
        // on purpose. Cutting it at the pane edge would leave the user with
        // "local port 5173 is already in use by another proce+".
        Some(status) => {
            let code = if status.error { RED } else { DIM };
            for wrapped in wrap_to_width(&sanitize(&status.text), width) {
                lines.push(essential(paint(&wrapped, code, color)));
            }
        }
        None => lines.push(essential(String::new())),
    }

    let mut frame = Frame::new(width, color);
    for line in fit_confirm(lines, snapshot.height.max(1)) {
        frame.push(line);
    }
    frame.finish()
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Enter,
    Escape,
    Backspace,
    /// Ctrl+C. Kept apart from a plain quit so the caller can exit non-zero.
    Interrupt,
    Char(char),
    /// Something recognised as a complete sequence but not bound to anything.
    Unknown,
}

enum Parsed {
    Key(Key, usize),
    /// The buffer ends mid-sequence: read more before deciding.
    Pending,
}

/// Decode as many keys as the buffer completes, returning them and how many
/// bytes they consumed.
///
/// `at_read_end` says whether the caller has nothing further buffered right
/// now. It exists for one character: a lone `ESC` is both the Escape key and
/// the first byte of every arrow sequence, and the only thing separating them
/// is whether more bytes arrived in the same read. A terminal sends `ESC [ A`
/// in one write, so resolving a trailing lone `ESC` to Escape is correct in
/// practice and is what makes Escape close a popup immediately.
pub fn parse_keys(bytes: &[u8], at_read_end: bool) -> (Vec<Key>, usize) {
    let mut keys = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        // `at_read_end` can only change the answer for a sequence whose first
        // byte is also the buffer's last, so passing it through unconditionally
        // cannot resolve a mid-buffer ESC early.
        match parse_one(&bytes[offset..], at_read_end) {
            Parsed::Key(key, used) => {
                keys.push(key);
                offset += used.max(1);
            }
            Parsed::Pending => break,
        }
    }
    (keys, offset)
}

fn parse_one(bytes: &[u8], at_read_end: bool) -> Parsed {
    let Some(&first) = bytes.first() else {
        return Parsed::Pending;
    };
    match first {
        0x1b => parse_escape(bytes, at_read_end),
        0x03 => Parsed::Key(Key::Interrupt, 1),
        0x0d | 0x0a => Parsed::Key(Key::Enter, 1),
        0x0e => Parsed::Key(Key::Down, 1), // ctrl+n, emacs-style
        0x10 => Parsed::Key(Key::Up, 1),   // ctrl+p
        0x08 | 0x7f => Parsed::Key(Key::Backspace, 1),
        0x00..=0x1f => Parsed::Key(Key::Unknown, 1),
        _ => parse_char(bytes),
    }
}

fn parse_char(bytes: &[u8]) -> Parsed {
    let lead = bytes[0];
    let len = match lead {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        // A continuation byte or an invalid lead cannot start a character.
        // Consuming it keeps a stray byte from stalling the buffer forever.
        _ => return Parsed::Key(Key::Unknown, 1),
    };
    if bytes.len() < len {
        return Parsed::Pending;
    }
    match std::str::from_utf8(&bytes[..len]) {
        Ok(text) => match text.chars().next() {
            Some(ch) => Parsed::Key(Key::Char(ch), len),
            None => Parsed::Key(Key::Unknown, len),
        },
        Err(_) => Parsed::Key(Key::Unknown, 1),
    }
}

fn parse_escape(bytes: &[u8], at_read_end: bool) -> Parsed {
    match bytes.get(1) {
        None => {
            if at_read_end {
                Parsed::Key(Key::Escape, 1)
            } else {
                Parsed::Pending
            }
        }
        // ESC ESC: some terminals double it. Take the first as Escape and let
        // the second parse on its own rather than swallowing both.
        Some(0x1b) => Parsed::Key(Key::Escape, 1),
        Some(b'[') => parse_csi(bytes),
        Some(b'O') => match bytes.get(2) {
            None => Parsed::Pending,
            Some(&final_byte) => Parsed::Key(arrow(final_byte), 3),
        },
        // ESC + a printable byte is alt+key; nothing here is bound to one.
        Some(_) => Parsed::Key(Key::Unknown, 2),
    }
}

fn parse_csi(bytes: &[u8]) -> Parsed {
    let mut index = 2;
    while index < bytes.len() && matches!(bytes[index], 0x30..=0x3f) {
        index += 1;
    }
    let params_end = index;
    while index < bytes.len() && matches!(bytes[index], 0x20..=0x2f) {
        index += 1;
    }
    let Some(&final_byte) = bytes.get(index) else {
        return Parsed::Pending;
    };
    if !matches!(final_byte, 0x40..=0x7e) {
        return Parsed::Key(Key::Unknown, index + 1);
    }
    let params = std::str::from_utf8(&bytes[2..params_end]).unwrap_or("");
    let key = match final_byte {
        b'A' | b'B' | b'C' | b'D' => arrow(final_byte),
        b'H' => Key::Home,
        b'F' => Key::End,
        b'~' => match params.split(';').next().unwrap_or("") {
            "1" | "7" => Key::Home,
            "4" | "8" => Key::End,
            "5" => Key::PageUp,
            "6" => Key::PageDown,
            _ => Key::Unknown,
        },
        _ => Key::Unknown,
    };
    Parsed::Key(key, index + 1)
}

fn arrow(final_byte: u8) -> Key {
    match final_byte {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        _ => Key::Unknown,
    }
}

/// How far a page key moves.
const PAGE: isize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListCommand {
    Move(isize),
    Top,
    Bottom,
    Stop,
    /// Stop every row the filter is showing — which with no filter is every
    /// forward on every host, and that is what the header says it is.
    StopShown,
    Reload,
    /// Show the next host on its own, or every host again. See
    /// [`next_host_filter`].
    CycleHost,
    /// Leave for the picker, so the overlay that shows what is forwarded is
    /// also a way to forward something else.
    AddPort,
    Quit,
    Interrupt,
    Ignore,
}

pub fn list_command(key: Key) -> ListCommand {
    match key {
        Key::Up | Key::Char('k') => ListCommand::Move(-1),
        Key::Down | Key::Char('j') => ListCommand::Move(1),
        Key::PageUp => ListCommand::Move(-PAGE),
        Key::PageDown => ListCommand::Move(PAGE),
        Key::Home | Key::Char('g') => ListCommand::Top,
        Key::End | Key::Char('G') => ListCommand::Bottom,
        Key::Enter | Key::Char('x') | Key::Char('d') => ListCommand::Stop,
        Key::Char('X') | Key::Char('D') => ListCommand::StopShown,
        Key::Char('r') => ListCommand::Reload,
        // `h`, for host. Not a movement key here — Left and Right do nothing in
        // this pane — and clear of `enter`, `X`, `a`, `r` and `q`, which are
        // what the footer already promises.
        Key::Char('h') => ListCommand::CycleHost,
        Key::Char('a') => ListCommand::AddPort,
        Key::Escape | Key::Char('q') => ListCommand::Quit,
        Key::Interrupt => ListCommand::Interrupt,
        _ => ListCommand::Ignore,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerCommand {
    Move(isize),
    Top,
    Bottom,
    Toggle,
    Confirm,
    Reload,
    Quit,
    Interrupt,
    Ignore,
}

/// What a key does on the confirmation screen.
///
/// Deliberately not a superset of the other two: there is nothing to move
/// through and nothing to mark, so every key that is not one of these five is
/// [`ConfirmCommand::Ignore`] rather than doing something adjacent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmCommand {
    /// Forward, then let the machine with the browser open the page.
    ForwardAndOpen,
    /// Forward, and open nothing.
    ForwardOnly,
    /// Leave for the `list` overlay.
    ShowList,
    /// Leave for the `picker`.
    ShowPicker,
    Quit,
    Interrupt,
    Ignore,
}

pub fn confirm_command(key: Key) -> ConfirmCommand {
    match key {
        Key::Enter => ConfirmCommand::ForwardAndOpen,
        Key::Char('f') => ConfirmCommand::ForwardOnly,
        Key::Char('l') => ConfirmCommand::ShowList,
        Key::Char('s') => ConfirmCommand::ShowPicker,
        Key::Escape | Key::Char('q') => ConfirmCommand::Quit,
        Key::Interrupt => ConfirmCommand::Interrupt,
        _ => ConfirmCommand::Ignore,
    }
}

pub fn picker_command(key: Key) -> PickerCommand {
    match key {
        Key::Up | Key::Char('k') => PickerCommand::Move(-1),
        Key::Down | Key::Char('j') => PickerCommand::Move(1),
        Key::PageUp => PickerCommand::Move(-PAGE),
        Key::PageDown => PickerCommand::Move(PAGE),
        Key::Home | Key::Char('g') => PickerCommand::Top,
        Key::End | Key::Char('G') => PickerCommand::Bottom,
        Key::Char(' ') => PickerCommand::Toggle,
        Key::Enter => PickerCommand::Confirm,
        Key::Char('r') => PickerCommand::Reload,
        Key::Escape | Key::Char('q') => PickerCommand::Quit,
        Key::Interrupt => PickerCommand::Interrupt,
        _ => PickerCommand::Ignore,
    }
}

// ---------------------------------------------------------------------------
// What the footers promise
// ---------------------------------------------------------------------------
//
// A pane's key hints are the only documentation a user reads while the pane is
// open, and a wrong one is worse than none: "X stop all" spelled `x` advertises
// a key bound to `Stop`, so somebody stops one forward believing they stopped
// every one. The hint text, the key it names, and the command that key must run
// therefore live in one row each, and the footers are rendered from these
// tables. `the_keys_the_panes_promise_are_all_bound` checks every row's key
// against the binding function, and
// `each_populated_pane_prints_the_footer_it_promises` pins the wording, which
// no table can check for itself.

/// Separator between key hints in a footer.
const HINT_GAP: &str = "   ";

/// The `list` pane's footer: hint, the key it names, the command it must run.
///
/// "X stop shown" rather than "X stop all": with every host on screen and a key
/// that narrows to one, "all" no longer names anything a user can point at, and
/// what `X` stops is exactly the rows in front of them.
const LIST_HINTS: [(&str, Key, ListCommand); 7] = [
    ("up/down move", Key::Down, ListCommand::Move(1)),
    ("enter stop", Key::Enter, ListCommand::Stop),
    ("X stop shown", Key::Char('X'), ListCommand::StopShown),
    ("h host", Key::Char('h'), ListCommand::CycleHost),
    ("a add", Key::Char('a'), ListCommand::AddPort),
    ("r refresh", Key::Char('r'), ListCommand::Reload),
    ("q close", Key::Char('q'), ListCommand::Quit),
];

/// The `picker` pane's footer, in the same shape.
const PICKER_HINTS: [(&str, Key, PickerCommand); 5] = [
    ("up/down move", Key::Down, PickerCommand::Move(1)),
    ("space mark", Key::Char(' '), PickerCommand::Toggle),
    ("enter forward", Key::Enter, PickerCommand::Confirm),
    ("r refresh", Key::Char('r'), PickerCommand::Reload),
    ("esc cancel", Key::Escape, PickerCommand::Quit),
];

/// The `confirm` pane's footer: the key as it is printed, what it does, the
/// key it names, and the command that key must run. One row per line rather
/// than one line for all of them, because this pane's rows are sentences.
///
/// Which rows a given click actually shows is [`confirm_hints`]; this is the
/// full set, and the checked one.
const CONFIRM_HINTS: [(&str, &str, Key, ConfirmCommand); 5] = [
    (
        "enter",
        "forward and open in the browser",
        Key::Enter,
        ConfirmCommand::ForwardAndOpen,
    ),
    (
        "f",
        "forward only",
        Key::Char('f'),
        ConfirmCommand::ForwardOnly,
    ),
    (
        "l",
        "see the forwards already up",
        Key::Char('l'),
        ConfirmCommand::ShowList,
    ),
    (
        "s",
        "look for other ports on this host",
        Key::Char('s'),
        ConfirmCommand::ShowPicker,
    ),
    ("esc", "cancel", Key::Escape, ConfirmCommand::Quit),
];

fn hint_line<C>(hints: &[(&str, Key, C)]) -> String {
    hints
        .iter()
        .map(|(text, _, _)| *text)
        .collect::<Vec<_>>()
        .join(HINT_GAP)
}

// ---------------------------------------------------------------------------
// Driving a pane
// ---------------------------------------------------------------------------

/// Why a pane stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The user closed it, or stdin reached end of file.
    Closed,
    /// Ctrl+C. Reported separately so the process can exit non-zero.
    Interrupted,
}

/// Another of this plugin's panes, which the user asked for from the one they
/// are in.
///
/// # Why this is a return value and not a call
///
/// Herdr keeps exactly one popup slot: `spawn_popup_command` refuses a second
/// popup with `popup already open`, and the popup it already has is the pane
/// making the request. (`ui_busy` is the *other* refusal, and a different one:
/// it fires when the app is in a modal mode — Settings, Copy — which a plugin
/// popup is not, since opening one sets the mode back to `Terminal`.) A popup
/// also cannot close itself: Herdr closes it when its process dies, and a popup
/// pane command is launched without `HERDR_PANE_ID`, so there is not even a
/// pane id it could name to `herdr pane close`.
///
/// So the ordering is forced. The pane cannot exit and then ask, because a
/// process that has exited asks nothing. It has to ask *before* it exits, while
/// its own popup is still up — which is fine for an overlay, and is why
/// [`crate::herdr_cli::plugin_pane_open_argv_with`] takes a placement: a popup
/// handing off to a popup asks for an overlay instead. Herdr opens it behind
/// the popup and reveals it the instant the popup's process exits.
///
/// This is deliberately *not* the recursion invariant 10 forbids. That rule is
/// about a pane that was not told enough asking for a pane whose command would
/// ask again; this is a key the user pressed, and the graph it walks —
/// `confirm` to `list` or `picker`, `list` to `picker` — has no cycle in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handoff {
    /// The `list` overlay of live forwards.
    List,
    /// The `picker` over attributed listeners.
    Picker,
}

/// What a pane leaves behind: the last status line, for the caller to log or
/// print once the pane is gone, and the pane the user asked to go to next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub exit: Exit,
    pub status: Option<Status>,
    /// Set when the user pressed a cross-link key. The caller opens it; see
    /// [`Handoff`] for why the pane does not.
    pub handoff: Option<Handoff>,
}

impl Outcome {
    fn closed(status: Option<Status>) -> Self {
        Self {
            exit: Exit::Closed,
            status,
            handoff: None,
        }
    }

    fn interrupted(status: Option<Status>) -> Self {
        Self {
            exit: Exit::Interrupted,
            status,
            handoff: None,
        }
    }

    fn handing_off(status: Option<Status>, handoff: Handoff) -> Self {
        Self {
            exit: Exit::Closed,
            status,
            handoff: Some(handoff),
        }
    }
}

/// What the `list` pane does when a key asks for it.
///
/// The UI performs no forwarding of its own: it renders, reads keys, and calls
/// back. Both arms return a status line, so a refusal is shown in place rather
/// than tearing the pane down.
pub trait ListActions {
    /// Stop the forward this row names — on this row's host, whatever host the
    /// invocation resolved to.
    fn stop(&mut self, row: &ForwardRow) -> std::result::Result<String, String>;
    /// Stop every row the user can see. The rows are passed rather than
    /// re-derived so "shown" means the same thing to the screen and to the
    /// action: whatever the host filter is letting through right now.
    fn stop_shown(&mut self, rows: &[ForwardRow]) -> std::result::Result<String, String>;
    /// Rows as they are now, on every host. Called after every action and on
    /// `r`; the filter is applied by the snapshot, not by the caller.
    fn rows(&mut self) -> std::result::Result<Vec<ForwardRow>, String>;
}

/// What the `picker` pane does when a key asks for it.
pub trait PickerActions {
    fn forward(&mut self, rows: &[ListenerRow]) -> std::result::Result<String, String>;
    fn rows(&mut self) -> std::result::Result<Vec<ListenerRow>, String>;
}

/// What the `confirm` pane does when a key asks for it.
///
/// One method, because the screen offers one decision with a yes/no attached:
/// whether the machine that ends up holding the forward should also open the
/// page. Under `herdr --remote` neither happens here — the forward is recorded
/// and the local agent does both — and that difference belongs to the caller,
/// not to the screen.
pub trait ConfirmActions {
    fn forward(&mut self, open_browser: bool) -> std::result::Result<String, String>;
}

/// Terminal newlines. In raw mode a bare `\n` moves down without returning the
/// carriage, so every column would drift right by the length of the line above.
pub fn crlf(text: &str) -> String {
    text.replace('\n', "\r\n")
}

fn draw<W: Write>(out: &mut W, page: &str) -> io::Result<()> {
    // Synchronized output so a repaint is not seen half-applied, then home and
    // clear. No alternate screen: Herdr owns the pane and closes it for us.
    write!(out, "\x1b[?2026h\x1b[?25l\x1b[H\x1b[2J{}", crlf(page))?;
    write!(out, "\x1b[?2026l")?;
    out.flush()
}

fn restore<W: Write>(out: &mut W) -> io::Result<()> {
    write!(out, "\x1b[?2026l\x1b[?25h\r\n")?;
    out.flush()
}

/// Read one chunk of input and append it to `pending`. `Ok(false)` means EOF.
fn read_chunk<R: Read>(input: &mut R, pending: &mut Vec<u8>) -> io::Result<bool> {
    let mut buf = [0u8; 256];
    loop {
        return match input.read(&mut buf) {
            Ok(0) => Ok(false),
            Ok(n) => {
                pending.extend_from_slice(&buf[..n]);
                Ok(true)
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => Err(err),
        };
    }
}

/// Run the forward manager against arbitrary streams.
///
/// Terminal-free by construction: tests pass a byte slice and a `Vec<u8>`.
///
/// Keys are decoded with `at_read_end = true`, so a trailing lone `ESC` closes
/// the pane immediately. The alternative — holding it back in case the next
/// read completes an arrow sequence — would make Escape wait for a second
/// keystroke, and Escape closing a popup is the behavior users depend on.
/// Splitting `ESC [ A` across two reads is possible but rare, since a terminal
/// emits the whole sequence in one write.
pub fn run_list<A, R, W>(
    app: &mut A,
    mut snapshot: ListSnapshot,
    input: &mut R,
    out: &mut W,
) -> io::Result<Outcome>
where
    A: ListActions,
    R: Read,
    W: Write,
{
    let mut pending: Vec<u8> = Vec::new();
    loop {
        draw(out, &render_list(&snapshot))?;
        if !read_chunk(input, &mut pending)? {
            restore(out)?;
            return Ok(Outcome::closed(snapshot.status));
        }
        let (keys, consumed) = parse_keys(&pending, true);
        pending.drain(..consumed);
        for key in keys {
            let shown = snapshot.shown();
            match list_command(key) {
                ListCommand::Move(delta) => {
                    snapshot.selected = step_selection(snapshot.selected, shown.len(), delta);
                }
                ListCommand::Top => snapshot.selected = 0,
                ListCommand::Bottom => {
                    snapshot.selected = shown.len().saturating_sub(1);
                }
                ListCommand::Stop => match snapshot.selected_row() {
                    Some(row) => {
                        snapshot.status = Some(outcome_status(app.stop(&row)));
                        reload_list(app, &mut snapshot);
                    }
                    // "nothing to stop" is a report on a listing. Without one
                    // it would be a guess, and it would overwrite the line
                    // saying why there is no listing.
                    None if !snapshot.unlisted() => {
                        snapshot.status = Some(Status::info("nothing to stop"));
                    }
                    None => {}
                },
                ListCommand::StopShown => {
                    if shown.is_empty() {
                        if !snapshot.unlisted() {
                            snapshot.status = Some(Status::info("nothing to stop"));
                        }
                    } else {
                        snapshot.status = Some(outcome_status(app.stop_shown(&shown)));
                        reload_list(app, &mut snapshot);
                    }
                }
                ListCommand::CycleHost => match host_cycle_effect(&snapshot) {
                    HostCycle::Filter(filter) => {
                        snapshot.filter = filter;
                        // The old index pointed into a different table.
                        snapshot.selected = 0;
                        snapshot.top = 0;
                    }
                    HostCycle::Say(message) => snapshot.status = Some(Status::info(message)),
                    HostCycle::Keep => {}
                },
                ListCommand::Reload => reload_list(app, &mut snapshot),
                ListCommand::AddPort => {
                    restore(out)?;
                    return Ok(Outcome::handing_off(snapshot.status, Handoff::Picker));
                }
                ListCommand::Quit => {
                    restore(out)?;
                    return Ok(Outcome::closed(snapshot.status));
                }
                ListCommand::Interrupt => {
                    restore(out)?;
                    return Ok(Outcome::interrupted(snapshot.status));
                }
                ListCommand::Ignore => {}
            }
            // After every key, not only the ones that move: a stop, a refresh
            // or a change of filter changes how many rows there are, and the
            // offset has to stay a place the shortened table still has.
            let visible = snapshot.shown().len();
            snapshot.top = scroll_top(
                snapshot.top,
                visible,
                snapshot.selected,
                rows_on_screen(snapshot.height, visible),
            );
        }
    }
}

fn outcome_status(result: std::result::Result<String, String>) -> Status {
    match result {
        Ok(message) => Status::info(message),
        Err(message) => Status::failed(message),
    }
}

/// Refresh rows in place. A failed refresh keeps the rows already on screen:
/// blanking the table because one round trip failed loses the user's place —
/// and it leaves [`Listing`] alone for the same reason, because a stale listing
/// is still a listing and still speaks for itself.
///
/// A refresh that succeeds over a pane that had no listing retires that state
/// and the status explaining it, which is the whole of what `r` is for there.
/// That status is the only one such a pane can be carrying: every key that
/// finds no rows leaves it alone rather than replacing it.
fn reload_list<A: ListActions>(app: &mut A, snapshot: &mut ListSnapshot) {
    match app.rows() {
        Ok(rows) => {
            if snapshot.unlisted() {
                snapshot.listing = Listing::Fetched;
                snapshot.status = None;
            }
            snapshot.rows = rows;
            // Against what is drawn, not against everything recorded: with a
            // host filter on, the two differ, and an index into the wrong one
            // points at a row the user cannot see.
            snapshot.selected = snapshot
                .selected
                .min(snapshot.shown().len().saturating_sub(1));
        }
        Err(message) => snapshot.status = Some(Status::failed(message)),
    }
}

fn reload_picker<A: PickerActions>(app: &mut A, snapshot: &mut PickerSnapshot) {
    match app.rows() {
        Ok(rows) => {
            if snapshot.unlisted() {
                snapshot.listing = Listing::Fetched;
                snapshot.status = None;
            }
            snapshot.rows = rows;
            snapshot.selected = snapshot.selected.min(snapshot.rows.len().saturating_sub(1));
        }
        Err(message) => snapshot.status = Some(Status::failed(message)),
    }
}

/// Run the listener picker against arbitrary streams.
///
/// A successful forward closes the popup, which is what a popup is for; a
/// failed one stays open with the reason, because closing would hide it.
pub fn run_picker<A, R, W>(
    app: &mut A,
    mut snapshot: PickerSnapshot,
    input: &mut R,
    out: &mut W,
) -> io::Result<Outcome>
where
    A: PickerActions,
    R: Read,
    W: Write,
{
    let mut pending: Vec<u8> = Vec::new();
    loop {
        draw(out, &render_picker(&snapshot))?;
        if !read_chunk(input, &mut pending)? {
            restore(out)?;
            return Ok(Outcome::closed(snapshot.status));
        }
        let (keys, consumed) = parse_keys(&pending, true);
        pending.drain(..consumed);
        for key in keys {
            match picker_command(key) {
                PickerCommand::Move(delta) => {
                    snapshot.selected =
                        step_selection(snapshot.selected, snapshot.rows.len(), delta);
                }
                PickerCommand::Top => snapshot.selected = 0,
                PickerCommand::Bottom => {
                    snapshot.selected = snapshot.rows.len().saturating_sub(1);
                }
                PickerCommand::Toggle => {
                    if let Some(row) = snapshot.rows.get_mut(snapshot.selected) {
                        row.marked = !row.marked;
                    }
                }
                PickerCommand::Confirm => {
                    let chosen = snapshot.chosen();
                    if chosen.is_empty() {
                        // As in the manager: with no listing behind it,
                        // "nothing to forward" would be a claim about this
                        // host's ports, made over the line saying why nothing
                        // is known about them.
                        if !snapshot.unlisted() {
                            snapshot.status = Some(Status::info("nothing to forward"));
                        }
                        continue;
                    }
                    match app.forward(&chosen) {
                        Ok(message) => {
                            restore(out)?;
                            return Ok(Outcome::closed(Some(Status::info(message))));
                        }
                        Err(message) => {
                            snapshot.status = Some(Status::failed(message));
                            reload_picker(app, &mut snapshot);
                        }
                    }
                }
                PickerCommand::Reload => reload_picker(app, &mut snapshot),
                PickerCommand::Quit => {
                    restore(out)?;
                    return Ok(Outcome::closed(snapshot.status));
                }
                PickerCommand::Interrupt => {
                    restore(out)?;
                    return Ok(Outcome::interrupted(snapshot.status));
                }
                PickerCommand::Ignore => {}
            }
            snapshot.top = scroll_top(
                snapshot.top,
                snapshot.rows.len(),
                snapshot.selected,
                rows_on_screen(snapshot.height, snapshot.rows.len()),
            );
        }
    }
}

/// Run the confirmation against arbitrary streams.
///
/// A successful forward closes the popup, as in the picker: the click asked for
/// one thing and it happened. A failed one stays open with the reason, because
/// closing would take the reason with it — and a refused forward is exactly the
/// case this screen exists for.
///
/// A cross-link key closes the popup too, and says so through
/// [`Outcome::handoff`] rather than opening anything itself. [`Handoff`] argues
/// for that ordering; it is not incidental.
pub fn run_confirm<A, R, W>(
    app: &mut A,
    mut snapshot: ConfirmSnapshot,
    input: &mut R,
    out: &mut W,
) -> io::Result<Outcome>
where
    A: ConfirmActions,
    R: Read,
    W: Write,
{
    let mut pending: Vec<u8> = Vec::new();
    loop {
        draw(out, &render_confirm(&snapshot))?;
        if !read_chunk(input, &mut pending)? {
            restore(out)?;
            return Ok(Outcome::closed(snapshot.status));
        }
        let (keys, consumed) = parse_keys(&pending, true);
        pending.drain(..consumed);
        for key in keys {
            match confirm_command(key) {
                command @ (ConfirmCommand::ForwardAndOpen | ConfirmCommand::ForwardOnly) => {
                    if !snapshot.state.actionable() {
                        // The footer does not offer these keys here, but a
                        // keystroke must still say why rather than doing
                        // nothing visible.
                        snapshot.status = Some(Status::failed(
                            "there is no host to forward from; press esc".to_string(),
                        ));
                        continue;
                    }
                    let open_browser = command == ConfirmCommand::ForwardAndOpen;
                    match app.forward(open_browser) {
                        Ok(message) => {
                            restore(out)?;
                            return Ok(Outcome::closed(Some(Status::info(message))));
                        }
                        // Including the local-port collision: the refusal names
                        // the holder and both ports, and nothing was changed.
                        Err(message) => snapshot.status = Some(Status::failed(message)),
                    }
                }
                ConfirmCommand::ShowList => {
                    restore(out)?;
                    return Ok(Outcome::handing_off(snapshot.status, Handoff::List));
                }
                ConfirmCommand::ShowPicker => {
                    restore(out)?;
                    return Ok(Outcome::handing_off(snapshot.status, Handoff::Picker));
                }
                ConfirmCommand::Quit => {
                    restore(out)?;
                    return Ok(Outcome::closed(snapshot.status));
                }
                ConfirmCommand::Interrupt => {
                    restore(out)?;
                    return Ok(Outcome::interrupted(snapshot.status));
                }
                ConfirmCommand::Ignore => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The terminal edge: the only part that touches the OS
// ---------------------------------------------------------------------------

/// Puts a tty into raw mode and puts it back on drop.
///
/// Single keystrokes need it: a cooked terminal hands over nothing until Enter,
/// and echoes what the user types over the frame.
pub struct RawMode {
    fd: RawFd,
    saved: libc::termios,
}

impl RawMode {
    /// `Ok(None)` when `fd` is not a terminal, which is how the pane behaves
    /// when its output is captured rather than displayed.
    pub fn enter(fd: RawFd) -> io::Result<Option<Self>> {
        if unsafe { libc::isatty(fd) } != 1 {
            return Ok(None);
        }
        // Two reads rather than a copy: `termios` carries platform-specific
        // padding, and reading it twice needs no assumptions about that.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(Self { fd, saved }))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Best effort: there is nothing useful to do with a failure here, and
        // leaving the terminal raw is worse than ignoring it.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

/// Terminal size in cells, falling back to `COLUMNS`/`LINES` and then to a
/// conventional 80x24 when the pane is not a tty.
pub fn terminal_size() -> (usize, usize) {
    if let Some(size) = window_size(io::stdout().as_raw_fd()) {
        return size;
    }
    (
        env_size("COLUMNS").unwrap_or(DEFAULT_WIDTH),
        env_size("LINES").unwrap_or(DEFAULT_HEIGHT),
    )
}

fn window_size(fd: RawFd) -> Option<(usize, usize)> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut size) } != 0 {
        return None;
    }
    if size.ws_col == 0 || size.ws_row == 0 {
        return None;
    }
    Some((size.ws_col as usize, size.ws_row as usize))
}

fn env_size(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.parse().ok().filter(|&n| n > 0)
}

/// Open the forward manager on this process's terminal.
pub fn open_list<A: ListActions>(app: &mut A, snapshot: ListSnapshot) -> io::Result<Outcome> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let _raw = RawMode::enter(stdin.as_raw_fd())?;
    let mut input = stdin.lock();
    let mut out = stdout.lock();
    run_list(app, snapshot, &mut input, &mut out)
}

/// Open the listener picker on this process's terminal.
pub fn open_picker<A: PickerActions>(app: &mut A, snapshot: PickerSnapshot) -> io::Result<Outcome> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let _raw = RawMode::enter(stdin.as_raw_fd())?;
    let mut input = stdin.lock();
    let mut out = stdout.lock();
    run_picker(app, snapshot, &mut input, &mut out)
}

/// Open the confirmation on this process's terminal.
pub fn open_confirm<A: ConfirmActions>(
    app: &mut A,
    snapshot: ConfirmSnapshot,
) -> io::Result<Outcome> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let _raw = RawMode::enter(stdin.as_raw_fd())?;
    let mut input = stdin.lock();
    let mut out = stdout.lock();
    run_confirm(app, snapshot, &mut input, &mut out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ForwardSpec, Listener};

    // ---- fixtures --------------------------------------------------------

    fn row(host: &str, remote: u16, local: u16, workspace: Option<&str>) -> ForwardRow {
        ForwardRow {
            host: host.into(),
            remote_port: remote,
            local_port: local,
            workspace: workspace.map(Into::into),
            agent: Some("claude".into()),
            pane_id: Some("w1:p1".into()),
            origin: Origin::Explicit,
            live: Liveness::Live,
        }
    }

    /// [`row`], with the port checked and not there — a master that died under
    /// the user, which is the ordinary way a record goes stale.
    fn missing_row(host: &str, remote: u16, local: u16, workspace: Option<&str>) -> ForwardRow {
        ForwardRow {
            live: Liveness::Missing,
            ..row(host, remote, local, workspace)
        }
    }

    /// [`row`], on a host this machine does not apply forwards for: nothing
    /// here checked, and nothing here may claim either answer.
    fn unchecked_row(host: &str, remote: u16, local: u16, workspace: Option<&str>) -> ForwardRow {
        ForwardRow {
            live: Liveness::Unknown,
            ..row(host, remote, local, workspace)
        }
    }

    fn listener_row(port: u16, process: &str, workspace: Option<&str>) -> ListenerRow {
        ListenerRow {
            remote_port: port,
            local_port: port,
            bind: "127.0.0.1".into(),
            process: process.into(),
            workspace: workspace.map(Into::into),
            agent: Some("codex".into()),
            pane_id: Some("w1:p2".into()),
            already_forwarded: false,
            marked: false,
        }
    }

    fn plain_list(rows: Vec<ForwardRow>) -> ListSnapshot {
        ListSnapshot {
            rows,
            color: false,
            ..ListSnapshot::default()
        }
    }

    fn plain_picker(rows: Vec<ListenerRow>) -> PickerSnapshot {
        PickerSnapshot {
            host: "buildbox".into(),
            rows,
            color: false,
            ..PickerSnapshot::default()
        }
    }

    /// What the panes open with when the listing they asked for failed: no
    /// rows, the reason on the status line, and [`Listing::Unavailable`] saying
    /// that the two go together. This is what `main` builds from a `list` that
    /// could not resolve its host.
    const NO_LISTING: &str = "unknown host \"stale-name\"; configured: buildbox";

    fn unlisted_list() -> ListSnapshot {
        ListSnapshot {
            listing: Listing::Unavailable,
            status: Some(Status::failed(NO_LISTING)),
            ..plain_list(Vec::new())
        }
    }

    fn unlisted_picker() -> PickerSnapshot {
        PickerSnapshot {
            host: String::new(),
            listing: Listing::Unavailable,
            status: Some(Status::failed(NO_LISTING)),
            ..plain_picker(Vec::new())
        }
    }

    /// Cell width, computed independently of the code under test.
    ///
    /// An alignment test that measured with [`display_width`] would pass just
    /// as happily on a byte-counting layout, because the same wrong measure
    /// would do the padding and then check it. This covers exactly the
    /// characters the fixtures use.
    fn ref_width(text: &str) -> usize {
        text.chars()
            .map(|ch| match ch as u32 {
                0x3000..=0x30FF | 0x4E00..=0x9FFF | 0xAC00..=0xD7A3 | 0xFF01..=0xFF60 => 2,
                _ => 1,
            })
            .sum()
    }

    /// Cells before `needle` starts, which is the column a reader sees it in.
    fn column_of(line: &str, needle: &str) -> usize {
        let at = line
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing from {line:?}"));
        ref_width(&line[..at])
    }

    /// Whether reverse video is in force at every glyph of `line`.
    ///
    /// Written against the bytes rather than reusing the module's own painting,
    /// because the bug it exists for is precisely that the row *looks* wrapped
    /// in a highlight — it starts with the code and ends with a reset — while an
    /// inner reset silently turns the highlight off partway across.
    fn reverse_video_throughout(line: &str) -> bool {
        let mut reverse = false;
        let mut covered = true;
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                let mut sequence = String::new();
                for next in chars.by_ref() {
                    sequence.push(next);
                    if matches!(next, '\u{40}'..='\u{7e}') && next != '[' && next != 'O' {
                        break;
                    }
                }
                match sequence.as_str() {
                    "[7m" => reverse = true,
                    // What `paint` closes every cell with, and what clears
                    // reverse video along with the colour it meant to close.
                    "[0m" | "[m" => reverse = false,
                    _ => {}
                }
                continue;
            }
            if !reverse {
                covered = false;
            }
        }
        covered
    }

    fn body_lines(page: &str) -> Vec<&str> {
        page.lines().collect()
    }

    fn row_line<'a>(page: &'a str, needle: &str) -> &'a str {
        body_lines(page)
            .into_iter()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no line containing {needle:?} in:\n{page}"))
    }

    // ---- measurement -----------------------------------------------------

    #[test]
    fn ascii_measures_one_cell_per_character() {
        assert_eq!(display_width("buildbox"), 8);
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn japanese_characters_measure_two_cells_each() {
        // The whole point of not using str::len: 30 bytes, 10 chars, 20 cells.
        let label = "実験用ワークスペース";
        assert_eq!(label.len(), 30);
        assert_eq!(label.chars().count(), 10);
        assert_eq!(display_width(label), 20);
    }

    #[test]
    fn hangul_and_fullwidth_forms_measure_two_cells() {
        assert_eq!(display_width("한글"), 4);
        assert_eq!(display_width("ｆｕｌｌ"), 8);
        assert_eq!(display_width("　"), 2); // ideographic space
    }

    #[test]
    fn halfwidth_katakana_measures_one_cell() {
        // U+FF66.. is halfwidth: inside the fullwidth block by codepoint but
        // narrow on screen, so the table must stop at U+FF60.
        assert_eq!(display_width("ﾊﾝｶｸ"), 4);
    }

    #[test]
    fn combining_marks_and_variation_selectors_add_no_width() {
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(display_width("\u{fe0f}"), 0);
        assert_eq!(display_width("a\u{200b}b"), 2);
    }

    #[test]
    fn control_characters_are_dropped_before_measuring() {
        assert_eq!(sanitize("a\u{1b}[31mb"), "a[31mb");
        assert_eq!(sanitize("tab\there"), "tabhere");
        assert_eq!(display_width("a\tb"), 2);
    }

    #[test]
    fn an_escape_sequence_occupies_no_cells() {
        // Padding a painted line depends on this: the colour code is an
        // instruction, not four glyphs.
        assert_eq!(display_width("\u{1b}[33m5173\u{1b}[0m"), 4);
        assert_eq!(display_width("\u{1b}[7m"), 0);
        assert_eq!(display_width("\u{1b}"), 0);
    }

    #[test]
    fn an_escape_sequence_from_a_remote_label_never_reaches_the_terminal() {
        let hostile = "\u{1b}[2J\u{1b}[31mscholion\u{7}";
        let clean = sanitize(hostile);
        assert!(!clean.contains('\u{1b}'), "escape survived: {clean:?}");
        assert!(!clean.contains('\u{7}'), "bell survived: {clean:?}");
        assert!(clean.contains("scholion"));
    }

    #[test]
    fn bidi_overrides_are_dropped_so_a_port_cannot_be_made_to_read_backwards() {
        let spoof = "svc\u{202e}3715\u{202c}";
        let clean = sanitize(spoof);
        assert_eq!(clean, "svc3715");
    }

    // ---- truncation and padding -----------------------------------------

    #[test]
    fn text_that_fits_is_left_alone() {
        assert_eq!(truncate_to_width("scholion", 8), "scholion");
        assert_eq!(truncate_to_width("scholion", 40), "scholion");
    }

    #[test]
    fn truncation_marks_the_cut() {
        assert_eq!(truncate_to_width("scholion", 5), "scho+");
        assert_eq!(truncate_to_width("scholion", 1), "+");
        assert_eq!(truncate_to_width("scholion", 0), "");
    }

    #[test]
    fn truncation_never_splits_a_wide_character() {
        // Budget 5 leaves 4 cells before the mark: two kana fit, the third
        // would straddle, so the cell comes back one cell short and padding
        // makes up the difference.
        let cut = truncate_to_width("あいうえお", 5);
        assert_eq!(cut, "あい+");
        assert_eq!(display_width(&cut), 5);

        let odd = truncate_to_width("あいうえお", 6);
        assert_eq!(odd, "あい+");
        assert_eq!(display_width(&odd), 5, "one cell short, never six");
        assert_eq!(display_width(&pad_to_width(&odd, 6)), 6);
    }

    #[test]
    fn padding_counts_cells_not_bytes() {
        assert_eq!(display_width(&pad_to_width("実験", 8)), 8);
        assert_eq!(display_width(&pad_left("5173", 8)), 8);
        assert_eq!(pad_left("5173", 8), "    5173");
        // Already at or over the width: left alone, not re-truncated.
        assert_eq!(pad_to_width("scholion", 4), "scholion");
    }

    // ---- selection and scrolling ----------------------------------------

    #[test]
    fn a_single_step_wraps_at_both_ends() {
        assert_eq!(step_selection(0, 3, -1), 2);
        assert_eq!(step_selection(2, 3, 1), 0);
        assert_eq!(step_selection(0, 3, 1), 1);
    }

    #[test]
    fn a_page_step_clamps_instead_of_wrapping() {
        assert_eq!(step_selection(1, 5, -10), 0);
        assert_eq!(step_selection(1, 5, 10), 4);
    }

    #[test]
    fn selection_on_an_empty_list_stays_at_zero() {
        assert_eq!(step_selection(0, 0, 1), 0);
        assert_eq!(step_selection(9, 0, -1), 0);
    }

    #[test]
    fn an_absurd_step_lands_at_an_end_rather_than_overflowing() {
        assert_eq!(step_selection(1, 3, isize::MIN), 0);
        assert_eq!(step_selection(1, 3, isize::MAX), 2);
    }

    #[test]
    fn selection_past_the_end_is_pulled_back_in_range() {
        assert_eq!(step_selection(99, 3, 1), 0);
        assert_eq!(step_selection(99, 3, -1), 1);
    }

    #[test]
    fn the_window_follows_the_selection_downward() {
        assert_eq!(visible_window(0, 10, 0, 4), (0, 4));
        assert_eq!(visible_window(0, 10, 3, 4), (0, 4));
        assert_eq!(visible_window(0, 10, 4, 4), (1, 5));
        assert_eq!(visible_window(0, 10, 9, 4), (6, 10));
    }

    #[test]
    fn the_window_holds_still_until_the_selection_reaches_its_top_edge() {
        // The bug this pins: a window derived from the selection alone put it
        // on the bottom row, so every step up scrolled the whole table by one
        // and the highlight never moved. Walking up from the end of a 20-row
        // list in a 5-row window, the first four steps must move the highlight
        // inside a window that does not budge.
        let (len, capacity) = (20, 5);
        let mut top = scroll_top(0, len, len - 1, capacity);
        assert_eq!(visible_window(top, len, 19, capacity), (15, 20));
        for selected in [18, 17, 16, 15] {
            top = scroll_top(top, len, selected, capacity);
            assert_eq!(
                visible_window(top, len, selected, capacity),
                (15, 20),
                "the table scrolled while the highlight had room to climb"
            );
        }
        // Only now, with the selection on the top row, does the table move —
        // and by exactly one row, not a windowful.
        top = scroll_top(top, len, 14, capacity);
        assert_eq!(visible_window(top, len, 14, capacity), (14, 19));
    }

    #[test]
    fn the_window_degenerates_safely() {
        assert_eq!(visible_window(0, 0, 0, 5), (0, 0));
        assert_eq!(visible_window(0, 5, 0, 0), (0, 0));
        assert_eq!(visible_window(0, 4, 0, 4), (0, 4));
        assert_eq!(visible_window(0, 4, 99, 2), (2, 4));
        // An offset from a longer table the pane no longer has: corrected
        // rather than trusted, so rendering cannot slice out of range.
        assert_eq!(visible_window(99, 4, 0, 2), (0, 2));
        assert_eq!(visible_window(99, 4, 3, 2), (2, 4));
        assert_eq!(visible_window(3, 4, 3, 9), (0, 4));
        assert_eq!(scroll_top(9, 0, 0, 4), 0);
    }

    // ---- list rendering --------------------------------------------------

    #[test]
    fn an_empty_list_says_what_to_do_instead_of_rendering_nothing() {
        let page = render_list(&plain_list(Vec::new()));
        assert!(
            page.contains("No forwards are recorded on any host."),
            "{page}"
        );
        assert!(page.contains("Ctrl+click"), "{page}");
        assert!(page.contains("(0 live, all hosts)"), "{page}");
        assert!(page.contains("q close"), "{page}");
        assert!(
            !page.contains("HOST"),
            "no table header without rows:\n{page}"
        );
    }

    #[test]
    fn a_manager_that_could_not_list_never_reports_an_empty_world() {
        // The report: `HERDR_PORTFWD_HOST=stale-name … pane-ui list` drew
        // `(0 live, all hosts)` over "No forwards are live on any host." while
        // `list` in the same sandbox found two, on two machines. The listing
        // had failed on the host name; the pane had reached nobody and said so
        // about everybody.
        let page = render_list(&unlisted_list());
        for claim in [
            "No forwards are recorded on any host.",
            "(0 live, all hosts)",
            "all hosts",
            "0 live",
        ] {
            assert!(
                !page.contains(claim),
                "a pane that reached no host claimed {claim:?}:\n{page}"
            );
        }
        assert!(page.contains("Could not list the forwards."), "{page}");
        assert!(
            page.contains(NO_LISTING),
            "the reason is the only thing this screen knows:\n{page}"
        );
        assert!(
            page.contains("r refresh"),
            "the key that could change the answer has to be on screen:\n{page}"
        );
        assert!(
            !page.contains("HOST"),
            "no table header without rows:\n{page}"
        );
    }

    #[test]
    fn a_picker_that_could_not_scan_never_reports_a_host_with_no_listeners() {
        let page = render_picker(&unlisted_picker());
        assert!(
            !page.contains("belongs to a Herdr pane"),
            "a scan that never ran established nothing:\n{page}"
        );
        assert!(
            page.contains("Could not list the listening ports"),
            "{page}"
        );
        assert!(page.contains(NO_LISTING), "{page}");
        assert!(page.contains("r refresh"), "{page}");
    }

    #[test]
    fn a_pane_that_could_not_list_still_fits_the_pane() {
        // The unavailable frames are two more sentences than the empty ones and
        // carry a status line that came from an ssh failure, which is the text
        // most likely to be long.
        let long = Status::failed(format!("{NO_LISTING}, {}", "buildbox-".repeat(30)));
        for width in [MIN_WIDTH, 40, 56, DEFAULT_WIDTH, 100] {
            let list = ListSnapshot {
                width,
                status: Some(long.clone()),
                ..unlisted_list()
            };
            let picker = PickerSnapshot {
                width,
                status: Some(long.clone()),
                ..unlisted_picker()
            };
            for (page, height) in [
                (render_list(&list), list.height),
                (render_picker(&picker), picker.height),
            ] {
                for line in page.lines() {
                    assert!(
                        ref_width(line) <= width,
                        "a {}-cell line in a {width}-cell pane: {line:?}",
                        ref_width(line)
                    );
                }
                assert!(page.lines().count() <= height, "{page}");
            }
        }
    }

    #[test]
    fn rows_on_screen_outrank_a_stale_unavailable_mark() {
        // Defensive: `unlisted` is about an empty table. A listing that arrived
        // is drawn whatever the mark says, rather than being hidden behind a
        // sentence saying it could not be fetched.
        let snapshot = ListSnapshot {
            listing: Listing::Unavailable,
            ..plain_list(two_hosts())
        };
        let page = render_list(&snapshot);
        assert!(page.contains("8788") && page.contains("5178"), "{page}");
        assert!(page.contains("(2 live, all hosts)"), "{page}");
    }

    #[test]
    fn an_empty_picker_says_why_it_is_empty() {
        let page = render_picker(&plain_picker(Vec::new()));
        assert!(page.contains("belongs to a Herdr pane"), "{page}");
        assert!(page.contains("Port: forward a port"), "{page}");
        // The same footer the empty list prints. Untested, the two empty
        // states drifted apart in wording with nothing to say so.
        assert!(page.contains("q close"), "{page}");
        assert!(
            !page.contains("WORKSPACE"),
            "no table header without rows:\n{page}"
        );
    }

    fn two_hosts() -> Vec<ForwardRow> {
        vec![
            row("mac-mini-honkoma", 8788, 8788, Some("mini: scholion")),
            row("mac-studio-honkoma", 5178, 5178, Some("studio: 実験")),
        ]
    }

    #[test]
    fn the_manager_draws_every_host_and_says_that_is_what_it_is_doing() {
        // The bug this pane had: it drew the one host the invocation resolved
        // to, under a header — `(1 live)` — that read exactly the same whether
        // one forward existed or one was being shown out of two.
        let page = render_list(&plain_list(two_hosts()));
        assert!(page.contains("8788"), "{page}");
        assert!(page.contains("5178"), "{page}");
        assert!(page.contains("mac-studio-honkoma"), "{page}");
        assert!(
            page.contains("(2 live, all hosts)"),
            "the header has to say how much of the world this is:\n{page}"
        );
    }

    #[test]
    fn a_filtered_manager_says_which_host_and_how_much_it_is_hiding() {
        let snapshot = ListSnapshot {
            filter: HostFilter::Host("mac-studio-honkoma".into()),
            ..plain_list(two_hosts())
        };
        let page = render_list(&snapshot);
        assert!(
            page.contains("(1 live of 2 recorded, host mac-studio-honkoma only)"),
            "{page}"
        );
        assert!(page.contains("5178"), "{page}");
        assert!(
            !page.contains("8788"),
            "the filter is not filtering:\n{page}"
        );
    }

    #[test]
    fn a_filter_with_nothing_in_it_does_not_read_as_an_empty_world() {
        // Every forward on the filtered host was just stopped. "No forwards are
        // live" here would be the old lie from the other side.
        let snapshot = ListSnapshot {
            filter: HostFilter::Host("gpu".into()),
            ..plain_list(two_hosts())
        };
        let page = render_list(&snapshot);
        assert!(page.contains("No forward on gpu."), "{page}");
        assert!(page.contains("2 live on other hosts"), "{page}");
        assert!(
            page.contains("h "),
            "the way back to every host has to be on screen:\n{page}"
        );
        assert!(
            !page.contains("No forwards are recorded"),
            "there are two, one host over:\n{page}"
        );
    }

    // ---- what the manager counts -----------------------------------------

    #[test]
    fn a_record_whose_port_is_gone_is_not_counted_as_a_live_forward() {
        // The report: `list` said `8788 live` and `54321 missing`, and the pane
        // above it said `(2 live, all hosts)` and drew both rows identically.
        // `Manager::forward_view` had dropped `ForwardStatus::live` on the way
        // in, so `rows.len()` was doing duty as a count of live forwards.
        let snapshot = plain_list(vec![
            row("mac-mini-honkoma", 8788, 8788, Some("mini: scholion")),
            missing_row("mac-mini-honkoma", 54321, 54321, Some("mini: scholion")),
        ]);
        let page = render_list(&snapshot);
        assert!(
            !page.contains("2 live"),
            "one of these two is not live:\n{page}"
        );
        assert!(
            page.contains("(1 live, 1 not live, all hosts)"),
            "the header has to count what was checked and found:\n{page}"
        );
        // And the rows have to be told apart, or the count says two things are
        // different while the table says they are the same.
        assert!(
            row_line(&page, "54321").contains("not live"),
            "the stale row is drawn exactly like the open one: {:?}",
            row_line(&page, "54321")
        );
        assert!(
            !row_line(&page, "8788").contains("not live"),
            "the live row was marked too, which marks nothing: {:?}",
            row_line(&page, "8788")
        );
    }

    #[test]
    fn a_forward_this_machine_cannot_check_is_neither_live_nor_dead() {
        // Under `herdr --remote` the pane runs on the far side, where probing a
        // local port answers a question about the wrong machine, so `list_in`
        // reports `Unknown` for every row. A wording built on a live/dead pair
        // has to pick one of them here, and both are inventions.
        let snapshot = plain_list(vec![
            unchecked_row("mac-mini-honkoma", 8788, 8788, None),
            unchecked_row("mac-studio-honkoma", 5178, 5178, None),
        ]);
        let page = render_list(&snapshot);
        assert!(
            page.contains("(2 not checked, all hosts)"),
            "a pane that checked nothing has to say so:\n{page}"
        );
        assert!(
            !page.contains("2 live") && !page.contains("2 not live"),
            "nothing here established either:\n{page}"
        );
        for port in ["8788", "5178"] {
            assert!(
                row_line(&page, port).contains("not checked"),
                "{port} is drawn as though its state were known: {:?}",
                row_line(&page, port)
            );
        }
    }

    #[test]
    fn a_filtered_header_counts_the_shown_rows_and_calls_the_rest_records() {
        // `1 of 3 live` counted rows on both sides of the "of": the shown ones
        // under a word nothing had checked them against, and the hidden ones
        // the filter had kept the pane from looking at at all.
        let snapshot = ListSnapshot {
            filter: HostFilter::Host("mac-mini-honkoma".into()),
            ..plain_list(vec![
                missing_row("mac-mini-honkoma", 8788, 8788, None),
                row("mac-studio-honkoma", 5178, 5178, None),
            ])
        };
        let page = render_list(&snapshot);
        assert!(
            page.contains("(1 not live of 2 recorded, host mac-mini-honkoma only)"),
            "{page}"
        );
        assert!(
            !page.contains("1 live of") && !page.contains("1 of 2 live"),
            "the one row on screen is a record, not a live forward:\n{page}"
        );
    }

    #[test]
    fn the_forwards_a_filter_hides_are_counted_by_what_is_known_about_them() {
        // The sentence a user reads when deciding whether `h` is worth
        // pressing. "2 live on other hosts" was the header's own mistake one
        // line down, and it was drawn from `rows.len()` the same way.
        let snapshot = ListSnapshot {
            filter: HostFilter::Host("gpu".into()),
            ..plain_list(vec![
                row("mac-mini-honkoma", 8788, 8788, None),
                missing_row("mac-studio-honkoma", 5178, 5178, None),
            ])
        };
        let page = render_list(&snapshot);
        assert!(
            page.contains("1 live, 1 not live on other hosts."),
            "{page}"
        );
        assert!(!page.contains("2 live on other hosts"), "{page}");
    }

    #[test]
    fn an_empty_table_that_was_read_says_it_read_records_rather_than_ports() {
        // The one claim an empty listing supports: the records were read and
        // there were none. Whether anything is live is a question about ports,
        // and no port was looked at to produce this screen.
        let page = render_list(&plain_list(Vec::new()));
        assert!(
            page.contains("No forwards are recorded on any host."),
            "{page}"
        );
        assert!(
            !page.contains("No forwards are live"),
            "an absent record is not a probed port:\n{page}"
        );
    }

    #[test]
    fn a_tally_names_only_the_answers_it_has() {
        let tally = |live, missing, unknown| {
            Tally {
                live,
                missing,
                unknown,
            }
            .phrase()
        };
        // The ordinary case still reads exactly as it always did.
        assert_eq!(tally(3, 0, 0), "3 live");
        assert_eq!(tally(0, 2, 0), "2 not live");
        assert_eq!(tally(0, 0, 2), "2 not checked");
        assert_eq!(tally(1, 1, 0), "1 live, 1 not live");
        assert_eq!(tally(1, 0, 2), "1 live, 2 not checked");
        assert_eq!(tally(1, 2, 3), "1 live, 2 not live, 3 not checked");
        // Nothing to count is a count, not a shrug: the listing arrived.
        assert_eq!(tally(0, 0, 0), "0 live");
        assert_eq!(
            Tally::of(&[
                row("a", 1, 1, None),
                missing_row("a", 2, 2, None),
                unchecked_row("b", 3, 3, None),
            ]),
            Tally {
                live: 1,
                missing: 1,
                unknown: 1
            }
        );
    }

    #[test]
    fn a_liveness_mark_is_paid_for_by_the_layout_rather_than_hung_off_the_edge() {
        // The notes are a column like any other, and this is the widest a row
        // can get: a mark and a remap on the same row.
        let mut both = missing_row("buildbox", 8787, 18787, Some("mini: api"));
        both.local_port = 18787;
        let page = render_list(&plain_list(vec![both]));
        let line = row_line(&page, "18787");
        assert!(
            line.ends_with("not live remapped"),
            "the notes were cut or pushed off: {line:?}"
        );
        assert!(
            ref_width(line) <= DEFAULT_WIDTH,
            "a {}-cell row in an {DEFAULT_WIDTH}-cell pane: {line:?}",
            ref_width(line)
        );
    }

    #[test]
    fn the_filter_cycles_through_all_hosts_and_each_host_that_has_a_forward() {
        let rows = two_hosts();
        let all = HostFilter::All;
        let mini = next_host_filter(&all, &rows);
        assert_eq!(mini, HostFilter::Host("mac-mini-honkoma".into()));
        let studio = next_host_filter(&mini, &rows);
        assert_eq!(studio, HostFilter::Host("mac-studio-honkoma".into()));
        assert_eq!(next_host_filter(&studio, &rows), HostFilter::All);
        // A host that no longer has a forward is not a place to stay.
        assert_eq!(
            next_host_filter(&HostFilter::Host("gone".into()), &rows),
            HostFilter::All
        );
        // And with one host, or none, there is nothing to narrow to.
        assert_eq!(
            next_host_filter(&all, &[row("only", 1, 1, None)]),
            HostFilter::All
        );
        assert_eq!(next_host_filter(&all, &[]), HostFilter::All);
    }

    #[test]
    fn the_filter_key_says_nothing_at_all_when_there_was_no_listing() {
        // `h` cannot change a table that was never fetched, and the status it
        // would have written — "there is nothing to filter" — describes a
        // listing that came back empty. Over a failed one it replaces the only
        // sentence on screen that named the actual problem.
        assert_eq!(host_cycle_effect(&unlisted_list()), HostCycle::Keep);
        // The same emptiness, arrived at honestly, still gets its message.
        assert_eq!(
            host_cycle_effect(&plain_list(Vec::new())),
            HostCycle::Say("there is nothing to filter".to_string())
        );
        assert_eq!(
            host_cycle_effect(&plain_list(vec![row("only", 1, 1, None)])),
            HostCycle::Say("every forward is on only".to_string())
        );
        assert_eq!(
            host_cycle_effect(&plain_list(two_hosts())),
            HostCycle::Filter(HostFilter::Host("mac-mini-honkoma".into()))
        );
    }

    #[test]
    fn no_row_outgrows_the_pane_when_two_hosts_and_east_asian_labels_share_it() {
        // Host names are the widest column that grew here: two long ones and a
        // wide label in the same table is what a two-machine user has.
        // One of them not live and remapped as well, so the row carrying the
        // most trailing notes is the one measured.
        let rows = vec![
            row("mac-mini-honkoma", 8788, 8788, Some("実験用ワークスペース")),
            missing_row("mac-studio-honkoma", 5178, 15178, Some("ミラー: 日本語")),
        ];
        for width in [MIN_WIDTH, 40, 56, DEFAULT_WIDTH, 100] {
            for filter in [
                HostFilter::All,
                HostFilter::Host("mac-studio-honkoma".into()),
            ] {
                let snapshot = ListSnapshot {
                    width,
                    filter,
                    ..plain_list(rows.clone())
                };
                let page = render_list(&snapshot);
                for line in page.lines() {
                    assert!(
                        ref_width(line) <= width,
                        "a {}-cell line in a {width}-cell pane: {line:?}",
                        ref_width(line)
                    );
                }
                assert!(page.lines().count() <= snapshot.height, "{page}");
            }
        }
    }

    #[test]
    fn columns_stay_aligned_when_a_workspace_label_is_truncated() {
        let long = "mini: an-extremely-long-workspace-label-nobody-should-type";
        let snapshot = plain_list(vec![
            row("buildbox", 5173, 5173, Some("mini: scholion")),
            row("buildbox", 8501, 8501, Some(long)),
        ]);
        let page = render_list(&snapshot);
        let short_line = row_line(&page, "5173");
        let long_line = row_line(&page, "8501");

        assert!(
            long_line.contains('+'),
            "truncation unmarked: {long_line:?}"
        );
        assert!(
            !long_line.contains("nobody-should-type"),
            "the tail should be gone: {long_line:?}"
        );
        assert_eq!(
            column_of(short_line, "claude"),
            column_of(long_line, "claude"),
            "agent column moved between rows:\n{page}"
        );
        assert_eq!(ref_width(short_line), ref_width(long_line));
    }

    #[test]
    fn columns_stay_aligned_with_east_asian_workspace_labels() {
        // A byte- or char-counted layout misaligns every column right of the
        // label here: 30 bytes, 10 chars, 20 cells.
        let snapshot = plain_list(vec![
            row("buildbox", 5173, 5173, Some("mini: scholion")),
            row("buildbox", 8501, 8501, Some("実験用ワークスペース")),
            row("buildbox", 8787, 8787, Some("ミラー: 日本語ラベル")),
        ]);
        let page = render_list(&snapshot);
        let ascii = row_line(&page, "5173");
        let wide = row_line(&page, "8501");
        let mixed = row_line(&page, "8787");

        for line in [wide, mixed] {
            assert_eq!(
                column_of(ascii, "explicit"),
                column_of(line, "explicit"),
                "origin column moved:\n{page}"
            );
            assert_eq!(
                ref_width(ascii),
                ref_width(line),
                "row widths differ:\n{page}"
            );
        }
    }

    #[test]
    fn a_wide_label_too_long_for_its_column_is_cut_on_a_character_boundary() {
        let label = "実験用ワークスペースの名前がとても長い場合のこと";
        let snapshot = plain_list(vec![
            row("buildbox", 5173, 5173, Some("mini: scholion")),
            row("buildbox", 8501, 8501, Some(label)),
        ]);
        let page = render_list(&snapshot);
        let cut = row_line(&page, "8501");
        assert!(cut.contains('+'), "{cut:?}");
        assert!(!cut.contains('\u{fffd}'), "split a character: {cut:?}");
        assert_eq!(
            ref_width(row_line(&page, "5173")),
            ref_width(cut),
            "\n{page}"
        );
    }

    #[test]
    fn a_remapped_port_is_called_out_and_a_matching_one_is_not() {
        let snapshot = plain_list(vec![
            row("buildbox", 5173, 5173, Some("mini: scholion")),
            row("buildbox", 8787, 18787, Some("mini: api")),
        ]);
        let page = render_list(&snapshot);
        let matched = row_line(&page, "5173");
        let remapped = row_line(&page, "18787");

        assert!(
            !matched.contains("remapped"),
            "a matching port must not be flagged: {matched:?}"
        );
        assert!(
            remapped.contains("remapped"),
            "a remapped port must say so: {remapped:?}"
        );
        assert!(remapped.contains("8787") && remapped.contains("18787"));
    }

    #[test]
    fn no_rendered_line_outgrows_the_pane_it_is_drawn_in() {
        // Measured with `ref_width`, not the module's own measure: a line that
        // overflows wraps in the terminal, which both misaligns the table and
        // costs a row the height budget never allowed for.
        let long = "ssh forward failed for buildbox (exit 255): bind [127.0.0.1]:5173: \
                    Address already in use";
        for width in [MIN_WIDTH, 40, 56, 72, DEFAULT_WIDTH, 100, 132] {
            let mut list = plain_list(vec![
                row("buildbox", 5173, 5173, Some("mini: scholion")),
                missing_row("buildbox", 8787, 18787, Some("実験用ワークスペース")),
                unchecked_row("a-rather-long-hostname", 3000, 3000, None),
            ]);
            list.width = width;
            list.status = Some(Status::failed(long));
            let page = render_list(&list);
            for line in page.lines() {
                assert!(
                    ref_width(line) <= width,
                    "a {}-cell line in a {width}-cell pane: {line:?}",
                    ref_width(line)
                );
            }
            assert!(page.lines().count() <= list.height, "{page}");

            let mut forwarded = listener_row(5173, "Code Helper (Plugin)", Some("mini: scholion"));
            forwarded.already_forwarded = true;
            let mut moved = listener_row(8787, "workerd", Some("実験用ワークスペース"));
            moved.local_port = 18787;
            let mut picker = plain_picker(vec![forwarded, moved]);
            picker.width = width;
            picker.status = Some(Status::failed(long));
            let page = render_picker(&picker);
            for line in page.lines() {
                assert!(
                    ref_width(line) <= width,
                    "a {}-cell line in a {width}-cell pane: {line:?}",
                    ref_width(line)
                );
            }
            assert!(page.lines().count() <= picker.height, "{page}");

            // The confirmation is the third pane and obeys the same rule. It
            // has a shape the others do not — indented sentences that wrap,
            // rather than columns that are cut — so it is the one most able to
            // put a line over the edge without a table looking wrong.
            let mut crowded = click();
            crowded.workspace = Some("実験用ワークスペース".to_string());
            crowded.url =
                "http://localhost:5173/a/deep/path?with=a-query-string#and-a-fragment".to_string();
            crowded.local_free = Some(false);
            crowded.applies_here = false;
            let confirm = ConfirmSnapshot {
                width,
                status: Some(Status::failed(long)),
                ..plain_confirm(crowded)
            };
            let page = render_confirm(&confirm);
            for line in page.lines() {
                assert!(
                    ref_width(line) <= width,
                    "a {}-cell line in a {width}-cell pane: {line:?}",
                    ref_width(line)
                );
            }
            assert!(page.lines().count() <= confirm.height, "{page}");
        }
    }

    #[test]
    fn the_remapped_note_is_paid_for_by_the_layout_rather_than_hung_off_the_edge() {
        // The note is a column like any other: if the layout does not reserve
        // its cells and the gap before it, the workspace column spends them and
        // the note starts past the right edge, where it wraps.
        let snapshot = plain_list(vec![row("buildbox", 8787, 18787, Some("mini: api"))]);
        assert_eq!(
            snapshot.width, DEFAULT_WIDTH,
            "the pane a user actually has"
        );
        let page = render_list(&snapshot);
        let line = row_line(&page, "18787");
        assert!(
            line.ends_with("remapped"),
            "the note was cut or pushed off: {line:?}"
        );
        assert!(
            ref_width(line) <= DEFAULT_WIDTH,
            "a {}-cell row in an {DEFAULT_WIDTH}-cell pane: {line:?}",
            ref_width(line)
        );
    }

    #[test]
    fn a_picker_note_is_paid_for_by_the_layout_rather_than_hung_off_the_edge() {
        let mut both = listener_row(8787, "workerd", Some("mini: api"));
        both.local_port = 18787;
        both.already_forwarded = true;
        let snapshot = plain_picker(vec![both]);
        let page = render_picker(&snapshot);
        let line = row_line(&page, "18787");
        assert!(
            line.ends_with("forwarded local 18787 remapped"),
            "the notes were cut or pushed off: {line:?}"
        );
        assert!(
            ref_width(line) <= DEFAULT_WIDTH,
            "a {}-cell row in an {DEFAULT_WIDTH}-cell pane: {line:?}",
            ref_width(line)
        );
    }

    #[test]
    fn a_status_too_long_for_the_pane_is_cut_rather_than_wrapped_over_the_footer() {
        // A refusal carries an ssh error verbatim, and there is no bound on one.
        // The footer under it is the pane's only documentation.
        let mut snapshot = plain_list(vec![row("buildbox", 5173, 5173, Some("mini: scholion"))]);
        snapshot.status = Some(Status::failed(
            "ssh forward failed for buildbox (exit 255): bind [127.0.0.1]:5173: Address \
             already in use; channel_setup_fwd_listener_tcpip: cannot listen to port 5173",
        ));
        let page = render_list(&snapshot);
        let status = row_line(&page, "ssh forward failed");
        assert!(
            ref_width(status) <= snapshot.width,
            "a {}-cell status in a {}-cell pane: {status:?}",
            ref_width(status),
            snapshot.width
        );
        assert!(
            status.ends_with(TRUNCATION_MARK),
            "a cut status must say it was cut: {status:?}"
        );
        assert!(page.contains("q close"), "the footer went missing:\n{page}");
    }

    #[test]
    fn the_highlight_covers_every_cell_of_a_selected_row_including_painted_ones() {
        // A row wrapped in reverse video still loses the highlight at its first
        // painted cell, because the reset that closes the colour closes the
        // highlight too. Checking the first and last bytes of the line cannot
        // see that; walking its escape sequences can.
        let mut list = plain_list(vec![row("buildbox", 8787, 18787, Some("mini: api"))]);
        list.color = true;
        let page = render_list(&list);
        let line = row_line(&page, "18787");
        assert!(
            reverse_video_throughout(line),
            "the highlight stopped partway across the row: {line:?}"
        );

        let mut both = listener_row(8787, "workerd", Some("mini: api"));
        both.local_port = 18787;
        both.already_forwarded = true;
        let mut picker = plain_picker(vec![both]);
        picker.color = true;
        let page = render_picker(&picker);
        let line = row_line(&page, "18787");
        assert!(
            reverse_video_throughout(line),
            "the highlight stopped partway across the row: {line:?}"
        );
    }

    #[test]
    fn a_remapped_port_is_the_only_thing_painted_when_colour_is_on() {
        let mut snapshot = plain_list(vec![
            row("buildbox", 5173, 5173, Some("mini: scholion")),
            row("buildbox", 8787, 18787, Some("mini: api")),
        ]);
        snapshot.color = true;
        snapshot.selected = 0;
        let page = render_list(&snapshot);
        let remapped = row_line(&page, "18787");
        assert!(remapped.contains(YELLOW), "{remapped:?}");
        let matched = row_line(&page, "5173");
        assert!(matched.starts_with(REVERSE), "selection unhighlighted");
        assert!(!matched.contains(YELLOW));
    }

    #[test]
    fn the_highlight_covers_a_selected_row_even_when_it_carries_a_colour() {
        let mut snapshot = plain_list(vec![row("buildbox", 8787, 18787, Some("mini: api"))]);
        snapshot.color = true;
        snapshot.width = 100;
        let page = render_list(&snapshot);
        let line = row_line(&page, "18787");
        assert!(
            line.starts_with(REVERSE) && line.ends_with(RESET),
            "{line:?}"
        );
        assert_eq!(
            display_width(line),
            100,
            "reverse video must reach the right edge: {line:?}"
        );
    }

    #[test]
    fn colour_off_emits_no_escape_bytes_at_all() {
        let snapshot = plain_list(vec![
            row("buildbox", 5173, 5173, Some("mini: scholion")),
            row("gpu", 8787, 18787, None),
        ]);
        let page = render_list(&snapshot);
        assert!(!page.contains('\u{1b}'), "escape leaked:\n{page:?}");
        let picker = render_picker(&plain_picker(vec![listener_row(5173, "node", None)]));
        assert!(!picker.contains('\u{1b}'), "escape leaked:\n{picker:?}");
    }

    #[test]
    fn a_hostile_workspace_label_cannot_drive_the_terminal() {
        let snapshot = plain_list(vec![row(
            "buildbox",
            5173,
            5173,
            Some("\u{1b}[2J\u{1b}[1;1Hgotcha"),
        )]);
        let page = render_list(&snapshot);
        assert!(!page.contains('\u{1b}'), "{page:?}");
        assert!(page.contains("gotcha"), "the text itself still shows");
    }

    #[test]
    fn a_missing_workspace_or_agent_renders_a_placeholder_not_a_gap() {
        let mut only = row("buildbox", 5173, 5173, None);
        only.agent = None;
        let page = render_list(&plain_list(vec![only]));
        let line = row_line(&page, "5173");
        assert_eq!(line.matches('-').count(), 2, "{line:?}");
    }

    #[test]
    fn a_blank_workspace_label_falls_back_to_the_placeholder() {
        let page = render_list(&plain_list(vec![row("buildbox", 5173, 5173, Some("   "))]));
        assert!(row_line(&page, "5173").contains('-'));
    }

    #[test]
    fn the_selected_row_is_the_one_the_marker_points_at() {
        let mut snapshot = plain_list(vec![
            row("a", 1, 1, Some("one")),
            row("b", 2, 2, Some("two")),
            row("c", 3, 3, Some("three")),
        ]);
        snapshot.selected = 1;
        let page = render_list(&snapshot);
        assert!(row_line(&page, "two").starts_with("> "), "{page}");
        assert!(row_line(&page, "one").starts_with("  "));
    }

    #[test]
    fn a_selection_past_the_end_renders_the_last_row_rather_than_panicking() {
        let mut snapshot = plain_list(vec![
            row("a", 1, 1, Some("one")),
            row("b", 2, 2, Some("two")),
        ]);
        snapshot.selected = 99;
        let page = render_list(&snapshot);
        assert!(row_line(&page, "two").starts_with("> "), "{page}");
    }

    #[test]
    fn rows_beyond_the_pane_height_are_windowed_and_the_rest_is_counted() {
        let rows: Vec<ForwardRow> = (0..20)
            .map(|i| row("buildbox", 9000 + i, 9000 + i, Some("mini: scholion")))
            .collect();
        let mut snapshot = plain_list(rows);
        snapshot.height = 12;
        snapshot.selected = 0;
        let page = render_list(&snapshot);
        assert!(page.contains("more below"), "{page}");
        assert!(
            !page.contains("9019"),
            "row 20 should be off-screen:\n{page}"
        );
        assert!(
            page.lines().count() <= 12,
            "frame overflowed the pane:\n{page}"
        );

        snapshot.selected = 19;
        let page = render_list(&snapshot);
        assert!(
            page.contains("9019"),
            "the selection must stay visible:\n{page}"
        );
        assert!(page.contains("more above"), "{page}");
    }

    #[test]
    fn a_pane_too_short_for_any_row_still_shows_one() {
        let mut snapshot = plain_list(vec![
            row("a", 1, 1, Some("one")),
            row("b", 2, 2, Some("two")),
        ]);
        snapshot.height = 1;
        let page = render_list(&snapshot);
        assert!(page.contains("one"), "{page}");
    }

    #[test]
    fn a_narrow_pane_renders_at_the_minimum_width_instead_of_collapsing() {
        let mut snapshot = plain_list(vec![row("buildbox", 5173, 5173, Some("mini: scholion"))]);
        snapshot.width = 1;
        let page = render_list(&snapshot);
        let line = row_line(&page, "5173");
        assert!(line.contains("buildbox"), "{line:?}");
        assert!(display_width(line) >= MIN_WIDTH / 2);
    }

    #[test]
    fn a_very_wide_pane_does_not_stretch_the_workspace_column_without_bound() {
        let mut snapshot = plain_list(vec![row("buildbox", 5173, 5173, Some("mini: scholion"))]);
        snapshot.width = 4000;
        let page = render_list(&snapshot);
        assert!(display_width(row_line(&page, "5173")) < 200, "{page}");
    }

    #[test]
    fn every_origin_prints_a_word_rather_than_a_debug_shape() {
        for (origin, word) in [
            (Origin::Explicit, "explicit"),
            (Origin::Offered, "offered"),
            (Origin::Auto, "auto"),
        ] {
            let mut only = row("buildbox", 5173, 5173, Some("mini: scholion"));
            only.origin = origin;
            let page = render_list(&plain_list(vec![only]));
            assert!(row_line(&page, "5173").contains(word), "{page}");
        }
    }

    #[test]
    fn the_status_line_is_shown_and_an_error_is_painted_differently() {
        let mut snapshot = plain_list(vec![row("buildbox", 5173, 5173, Some("mini: scholion"))]);
        snapshot.status = Some(Status::failed("port 5173 is already in use here"));
        snapshot.color = true;
        let page = render_list(&snapshot);
        assert!(page.contains("already in use"), "{page}");
        assert!(page.contains(RED), "an error should not read as ordinary");
    }

    // ---- picker rendering ------------------------------------------------

    #[test]
    fn the_picker_shows_the_host_it_is_forwarding_from() {
        let page = render_picker(&plain_picker(vec![listener_row(5173, "node", None)]));
        assert!(page.contains("Forward a port from buildbox"), "{page}");
    }

    #[test]
    fn a_picker_row_flags_a_port_that_is_already_forwarded() {
        let mut forwarded = listener_row(5173, "node", Some("mini: scholion"));
        forwarded.already_forwarded = true;
        let snapshot = plain_picker(vec![
            forwarded,
            listener_row(8787, "workerd", Some("mini: api")),
        ]);
        let page = render_picker(&snapshot);
        assert!(row_line(&page, "5173").contains("forwarded"), "{page}");
        assert!(!row_line(&page, "8787").contains("forwarded"), "{page}");
    }

    #[test]
    fn a_picker_row_names_the_local_port_only_when_it_differs() {
        let mut remapped = listener_row(8787, "workerd", Some("mini: api"));
        remapped.local_port = 18787;
        let snapshot = plain_picker(vec![
            listener_row(5173, "node", Some("mini: scholion")),
            remapped,
        ]);
        let page = render_picker(&snapshot);
        let matched = row_line(&page, "5173");
        let moved = row_line(&page, "18787");
        assert!(!matched.contains("remapped"), "{matched:?}");
        assert!(moved.contains("local 18787 remapped"), "{moved:?}");
    }

    #[test]
    fn marked_rows_are_visible_and_the_selection_is_separate_from_the_mark() {
        let mut rows = vec![
            listener_row(5173, "node", Some("mini: scholion")),
            listener_row(8787, "workerd", Some("mini: api")),
        ];
        rows[1].marked = true;
        let mut snapshot = plain_picker(rows);
        snapshot.selected = 0;
        let page = render_picker(&snapshot);
        assert!(row_line(&page, "5173").starts_with(">  "), "{page}");
        assert!(row_line(&page, "8787").starts_with(" * "), "{page}");
    }

    #[test]
    fn picker_columns_stay_aligned_with_east_asian_labels() {
        let snapshot = plain_picker(vec![
            listener_row(5173, "node", Some("mini: scholion")),
            listener_row(8787, "node", Some("実験用ワークスペース")),
        ]);
        let page = render_picker(&snapshot);
        assert_eq!(
            column_of(row_line(&page, "5173"), "codex"),
            column_of(row_line(&page, "8787"), "codex"),
            "\n{page}"
        );
    }

    #[test]
    fn a_listener_with_no_pane_still_renders_with_a_placeholder() {
        let mut orphan = listener_row(3000, "cloudflared", None);
        orphan.agent = None;
        orphan.pane_id = None;
        let page = render_picker(&plain_picker(vec![orphan]));
        assert!(row_line(&page, "3000").contains('-'), "{page}");
    }

    #[test]
    fn a_process_name_with_spaces_stays_in_its_column() {
        let snapshot = plain_picker(vec![
            listener_row(5173, "Code Helper (Plugin)", Some("mini: scholion")),
            listener_row(8787, "node", Some("mini: api")),
        ]);
        let page = render_picker(&snapshot);
        assert_eq!(
            column_of(row_line(&page, "5173"), "mini"),
            column_of(row_line(&page, "8787"), "mini"),
            "\n{page}"
        );
    }

    // ---- adapters --------------------------------------------------------

    fn pane(id: &str, label: Option<&str>) -> PaneRef {
        PaneRef {
            pane_id: id.into(),
            workspace_id: "w1".into(),
            workspace_label: label.map(Into::into),
            agent: Some("claude".into()),
            shell_pid: Some(4242),
        }
    }

    fn forward(local: u16, remote: u16, pane_id: Option<&str>) -> Forward {
        Forward {
            spec: ForwardSpec {
                host: "buildbox".into(),
                remote_port: remote,
                local_port: local,
                remote_bind: "127.0.0.1".into(),
                origin: Origin::Offered,
                pane_id: pane_id.map(Into::into),
            },
            created_unix_ms: 0,
        }
    }

    #[test]
    fn a_forward_row_takes_its_workspace_and_agent_from_the_pane() {
        let built = ForwardRow::from_forward(
            &forward(5173, 5173, Some("w1:p1")),
            Some(&pane("w1:p1", Some("mini: scholion"))),
            Liveness::Live,
        );
        assert_eq!(built.workspace.as_deref(), Some("mini: scholion"));
        assert_eq!(built.agent.as_deref(), Some("claude"));
        assert_eq!(built.origin, Origin::Offered);
        assert!(!built.is_remapped());
    }

    #[test]
    fn a_pane_with_no_label_falls_back_to_its_workspace_id() {
        let built = ForwardRow::from_forward(
            &forward(5173, 5173, Some("w1:p1")),
            Some(&pane("w1:p1", None)),
            Liveness::Live,
        );
        assert_eq!(built.workspace.as_deref(), Some("w1"));
    }

    #[test]
    fn a_forward_whose_pane_is_gone_keeps_its_recorded_pane_id() {
        let built = ForwardRow::from_forward(
            &forward(15173, 5173, Some("w9:p9")),
            None,
            Liveness::Missing,
        );
        assert_eq!(built.workspace, None);
        assert_eq!(built.pane_id.as_deref(), Some("w9:p9"));
        assert!(built.is_remapped());
    }

    #[test]
    fn finding_a_pane_needs_an_exact_id_and_tolerates_none() {
        let panes = vec![pane("w1:p1", None), pane("w1:p2", None)];
        assert_eq!(
            find_pane(&panes, Some("w1:p2")).map(|p| p.pane_id.as_str()),
            Some("w1:p2")
        );
        assert!(find_pane(&panes, Some("w1:p3")).is_none());
        assert!(find_pane(&panes, None).is_none());
        assert!(find_pane(&[], Some("w1:p1")).is_none());
    }

    #[test]
    fn a_listener_row_carries_the_pane_it_was_attributed_to() {
        let attributed = AttributedListener {
            listener: Listener {
                port: 5173,
                bind: "127.0.0.1".into(),
                pid: 900,
                process: "node".into(),
            },
            pane: Some(pane("w1:p1", Some("mini: scholion"))),
        };
        let built = ListenerRow::from_attributed(&attributed, 5173, false);
        assert_eq!(built.workspace.as_deref(), Some("mini: scholion"));
        assert_eq!(built.pane_id.as_deref(), Some("w1:p1"));
        assert!(!built.marked);
        assert!(!built.is_remapped());

        let unattributed = AttributedListener {
            pane: None,
            ..attributed
        };
        let built = ListenerRow::from_attributed(&unattributed, 15173, true);
        assert_eq!(built.workspace, None);
        assert!(built.already_forwarded);
        assert!(built.is_remapped());
    }

    #[test]
    fn the_chosen_rows_are_the_marked_ones_or_else_the_highlighted_one() {
        let mut snapshot = plain_picker(vec![
            listener_row(5173, "node", None),
            listener_row(8787, "workerd", None),
            listener_row(3000, "next", None),
        ]);
        snapshot.selected = 2;
        let chosen = snapshot.chosen();
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].remote_port, 3000);

        snapshot.rows[0].marked = true;
        snapshot.rows[1].marked = true;
        let chosen = snapshot.chosen();
        assert_eq!(
            chosen.iter().map(|r| r.remote_port).collect::<Vec<_>>(),
            [5173, 8787],
            "marks win over the highlight"
        );

        assert!(plain_picker(Vec::new()).chosen().is_empty());
    }

    // ---- key decoding ----------------------------------------------------

    #[test]
    fn arrow_keys_decode_in_both_cursor_and_application_mode() {
        assert_eq!(parse_keys(b"\x1b[A", true), (vec![Key::Up], 3));
        assert_eq!(parse_keys(b"\x1b[B", true), (vec![Key::Down], 3));
        assert_eq!(parse_keys(b"\x1bOA", true), (vec![Key::Up], 3));
        assert_eq!(parse_keys(b"\x1bOB", true), (vec![Key::Down], 3));
    }

    #[test]
    fn vi_keys_move_the_same_way_as_arrows() {
        assert_eq!(list_command(Key::Char('j')), list_command(Key::Down));
        assert_eq!(list_command(Key::Char('k')), list_command(Key::Up));
        assert_eq!(picker_command(Key::Char('j')), picker_command(Key::Down));
        assert_eq!(picker_command(Key::Char('k')), picker_command(Key::Up));
    }

    #[test]
    fn an_arrow_split_across_two_reads_is_not_mistaken_for_escape() {
        // Mid-stream the parser must wait: the driver's own trade-off is
        // documented on run_list, but the pure parser has to offer the choice.
        assert_eq!(parse_keys(b"\x1b", false), (Vec::new(), 0));
        assert_eq!(parse_keys(b"\x1b[", false), (Vec::new(), 0));
        assert_eq!(parse_keys(b"\x1b[", true), (Vec::new(), 0));
        assert_eq!(parse_keys(b"\x1b[A", false), (vec![Key::Up], 3));
    }

    #[test]
    fn a_lone_escape_at_the_end_of_a_read_closes_the_pane() {
        assert_eq!(parse_keys(b"\x1b", true), (vec![Key::Escape], 1));
        assert_eq!(list_command(Key::Escape), ListCommand::Quit);
        assert_eq!(picker_command(Key::Escape), PickerCommand::Quit);
    }

    #[test]
    fn a_doubled_escape_yields_two_escapes_rather_than_swallowing_one() {
        assert_eq!(
            parse_keys(b"\x1b\x1b", true),
            (vec![Key::Escape, Key::Escape], 2)
        );
    }

    #[test]
    fn ctrl_c_is_distinct_from_a_plain_quit() {
        assert_eq!(parse_keys(b"\x03", true), (vec![Key::Interrupt], 1));
        assert_eq!(list_command(Key::Interrupt), ListCommand::Interrupt);
        assert_ne!(list_command(Key::Interrupt), list_command(Key::Char('q')));
    }

    #[test]
    fn enter_arrives_as_either_carriage_return_or_newline() {
        assert_eq!(parse_keys(b"\r", true), (vec![Key::Enter], 1));
        assert_eq!(parse_keys(b"\n", true), (vec![Key::Enter], 1));
    }

    #[test]
    fn a_burst_of_keys_in_one_read_is_decoded_in_order() {
        let (keys, used) = parse_keys(b"jj\x1b[Aq", true);
        assert_eq!(
            keys,
            vec![Key::Char('j'), Key::Char('j'), Key::Up, Key::Char('q')]
        );
        assert_eq!(used, 6);
    }

    #[test]
    fn a_multibyte_character_split_across_reads_is_held_until_complete() {
        let bytes = "あ".as_bytes();
        assert_eq!(parse_keys(&bytes[..1], true), (Vec::new(), 0));
        assert_eq!(parse_keys(&bytes[..2], true), (Vec::new(), 0));
        assert_eq!(parse_keys(bytes, true), (vec![Key::Char('あ')], 3));
    }

    #[test]
    fn a_stray_byte_is_consumed_rather_than_stalling_the_buffer() {
        // A continuation byte with no lead cannot start anything; leaving it
        // pending would wedge every later key behind it.
        assert_eq!(parse_keys(b"\x80", true), (vec![Key::Unknown], 1));
        assert_eq!(parse_keys(b"\xff", true), (vec![Key::Unknown], 1));
        assert_eq!(
            parse_keys(b"\xc3\x28", true),
            (vec![Key::Unknown, Key::Char('(')], 2)
        );
    }

    #[test]
    fn an_unbound_escape_sequence_is_consumed_whole() {
        // An SGR mouse report is the realistic case: nothing is bound to it,
        // but leaving its bytes behind would make them read as keys.
        assert_eq!(
            parse_keys(b"\x1b[<0;33;15Mq", true),
            (vec![Key::Unknown, Key::Char('q')], 12)
        );
        assert_eq!(parse_keys(b"\x1b[200~", true), (vec![Key::Unknown], 6));
        assert_eq!(parse_keys(b"\x1bb", true), (vec![Key::Unknown], 2));
    }

    #[test]
    fn page_and_home_keys_decode_from_their_tilde_forms() {
        assert_eq!(parse_keys(b"\x1b[5~", true), (vec![Key::PageUp], 4));
        assert_eq!(parse_keys(b"\x1b[6~", true), (vec![Key::PageDown], 4));
        assert_eq!(parse_keys(b"\x1b[1~", true), (vec![Key::Home], 4));
        assert_eq!(parse_keys(b"\x1b[4~", true), (vec![Key::End], 4));
        assert_eq!(parse_keys(b"\x1b[H", true), (vec![Key::Home], 3));
        assert_eq!(parse_keys(b"\x1b[F", true), (vec![Key::End], 3));
    }

    #[test]
    fn an_empty_read_decodes_to_nothing() {
        assert_eq!(parse_keys(b"", true), (Vec::new(), 0));
        assert_eq!(parse_keys(b"", false), (Vec::new(), 0));
    }

    #[test]
    fn the_keys_the_panes_promise_are_all_bound() {
        // Every row of the footer tables, against the binding function itself:
        // a hint naming a key nothing is bound to, or bound to a different
        // command, is a promise the pane does not keep.
        for (hint, key, command) in LIST_HINTS {
            assert_eq!(
                list_command(key),
                command,
                "the list pane promises {hint:?} for {key:?}"
            );
        }
        for (hint, key, command) in PICKER_HINTS {
            assert_eq!(
                picker_command(key),
                command,
                "the picker promises {hint:?} for {key:?}"
            );
        }
        for (key_text, hint, key, command) in CONFIRM_HINTS {
            assert_eq!(
                confirm_command(key),
                command,
                "the confirmation promises {key_text:?} for {hint:?}"
            );
        }
        // Aliases the footers have no room to name, and a key bound to nothing.
        assert_eq!(list_command(Key::Char('x')), ListCommand::Stop);
        assert_eq!(list_command(Key::Char('z')), ListCommand::Ignore);
        assert_eq!(picker_command(Key::Char('q')), PickerCommand::Quit);
        assert_eq!(picker_command(Key::Char('z')), PickerCommand::Ignore);
        assert_eq!(confirm_command(Key::Char('q')), ConfirmCommand::Quit);
        assert_eq!(confirm_command(Key::Char('z')), ConfirmCommand::Ignore);
    }

    #[test]
    fn the_confirmation_only_advertises_rows_it_can_honour() {
        // The footer is built per click, so the wording and the key have to be
        // checked against the table they came from — a row invented in
        // `confirm_hints` would be a promise no key keeps.
        let all: Vec<(&str, &str)> = CONFIRM_HINTS
            .iter()
            .map(|(key, text, _, _)| (*key, *text))
            .collect();
        for state in [
            click(),
            ConfirmState {
                already_forwarded: Some(true),
                ..click()
            },
            ConfirmState {
                host: None,
                problem: Some("no host".to_string()),
                ..click()
            },
        ] {
            for (key, text) in confirm_hints(&state) {
                let known = all.iter().any(|(table_key, table_text)| {
                    *table_key == key
                        // The one wording the table cannot carry: enter means
                        // "open it" once the forward is already up.
                        && (*table_text == text || text == "open in the browser")
                });
                assert!(known, "{key:?} / {text:?} is in no row of CONFIRM_HINTS");
            }
        }
    }

    #[test]
    fn each_populated_pane_prints_the_footer_it_promises() {
        // Spelled out rather than built from the tables: a footer assembled
        // from the same rows it is checked against would agree with any
        // wording, including a verb that contradicts the key — "enter forward"
        // on the pane whose entire job is stopping.
        let list = render_list(&plain_list(vec![row(
            "buildbox",
            5173,
            5173,
            Some("mini: scholion"),
        )]));
        assert!(
            list.contains(
                "up/down move   enter stop   X stop shown   h host   a add   r refresh   q close"
            ),
            "{list}"
        );

        let picker = render_picker(&plain_picker(vec![listener_row(
            5173,
            "node",
            Some("mini: scholion"),
        )]));
        assert!(
            picker.contains("up/down move   space mark   enter forward   r refresh   esc cancel"),
            "{picker}"
        );
    }

    // ---- the confirmation screen ----------------------------------------

    fn click() -> ConfirmState {
        ConfirmState {
            url: "http://localhost:5173/app".to_string(),
            remote_port: 5173,
            local_port: 5173,
            host: Some("buildbox".to_string()),
            local_free: Some(true),
            already_forwarded: Some(false),
            applies_here: true,
            workspace: Some("mini: scholion".to_string()),
            agent: Some("claude".to_string()),
            pane_id: Some("w1:p1".to_string()),
            problem: None,
        }
    }

    fn plain_confirm(state: ConfirmState) -> ConfirmSnapshot {
        ConfirmSnapshot {
            state,
            color: false,
            ..ConfirmSnapshot::default()
        }
    }

    #[test]
    fn the_confirmation_says_what_is_about_to_happen_before_it_happens() {
        // The whole point of the screen: everything the click implied, on
        // screen, with the choice of whether to open a browser as its own key
        // rather than folded into the act of clicking.
        let page = render_confirm(&plain_confirm(click()));
        assert!(page.contains("Forward a port from buildbox"), "{page}");
        assert!(page.contains("mini: scholion"), "{page}");
        assert!(page.contains("claude"), "{page}");
        assert!(page.contains("remote 5173  ->  localhost 5173"), "{page}");
        assert!(page.contains("(free)"), "{page}");
        assert!(page.contains("http://localhost:5173/app"), "{page}");
        assert!(
            page.contains("enter   forward and open in the browser"),
            "{page}"
        );
        assert!(page.contains("f       forward only"), "{page}");
        assert!(
            page.contains("l       see the forwards already up"),
            "{page}"
        );
        assert!(
            page.contains("s       look for other ports on this host"),
            "{page}"
        );
        assert!(page.contains("esc     cancel"), "{page}");
    }

    #[test]
    fn the_confirmations_chrome_stays_ascii_so_a_cjk_locale_cannot_widen_it() {
        // `─`, `→` and `…` are East Asian *ambiguous*: two cells wide in a CJK
        // locale, one in a Western one. A rule or an arrow drawn with them
        // overflows exactly the terminals this tool's users have.
        let page = render_confirm(&plain_confirm(click()));
        for ambiguous in ['─', '→', '…', '▸'] {
            assert!(
                !page.contains(ambiguous),
                "{ambiguous:?} is ambiguous-width chrome: {page}"
            );
        }
        assert!(page.is_ascii(), "{page}");
    }

    #[test]
    fn a_japanese_workspace_label_never_pushes_the_confirmation_past_its_width() {
        // The fixture has to be able to overflow, or the test is a tautology:
        // a ten-kana label and an ASCII agent name in a 48-cell pane left
        // sixteen cells of slack, so it passed just as happily against a
        // renderer that counted bytes. Sixteen kana are thirty-two cells, and
        // the agent name is on the same row, in a pane as narrow as one gets.
        let mut wide = click();
        wide.workspace = Some("実験用ワークスペース・スカリオン".to_string());
        wide.agent = Some("クロード".to_string());
        assert!(
            ref_width("  実験用ワークスペース・スカリオン    クロード") > MIN_WIDTH,
            "the fixture has to be wider than the narrowest pane, or it \
             cannot fail"
        );
        for width in [MIN_WIDTH, 40, 48, DEFAULT_WIDTH] {
            let snapshot = ConfirmSnapshot {
                width,
                ..plain_confirm(wide.clone())
            };
            let page = render_confirm(&snapshot);
            for line in page.lines() {
                assert!(
                    ref_width(line) <= width,
                    "{:?} is {} cells wide in a {width}-cell pane",
                    line,
                    ref_width(line)
                );
            }
            assert!(page.contains("実験用"), "the label still has to be there");
        }
    }

    #[test]
    fn an_escape_sequence_in_a_workspace_label_never_reaches_the_confirmation() {
        // Under `herdr --remote` the label came off the host, travelled through
        // an environment variable, and is drawn on the user's terminal.
        let mut hostile = click();
        hostile.workspace = Some("\u{1b}[2Jmini: scholion".to_string());
        hostile.url = "http://localhost:5173/\u{1b}[31m".to_string();
        let page = render_confirm(&plain_confirm(hostile));
        assert!(!page.contains('\u{1b}'), "escape survived: {page:?}");
    }

    #[test]
    fn a_click_over_a_forward_that_is_already_up_offers_to_open_it_rather_than_redo_it() {
        let mut live = click();
        live.already_forwarded = Some(true);
        // The port reads as taken precisely because our own forward holds it.
        live.local_free = Some(false);
        let page = render_confirm(&plain_confirm(live));
        assert!(page.contains("(already forwarded)"), "{page}");
        assert!(page.contains("enter   open in the browser"), "{page}");
        assert!(
            !page.contains("forward only"),
            "there is nothing left to forward: {page}"
        );
        assert!(
            !page.contains("held by something else"),
            "our own forward is not a collision: {page}"
        );
    }

    #[test]
    fn a_local_port_held_by_something_else_is_named_and_no_remap_is_offered() {
        let mut taken = click();
        taken.local_free = Some(false);
        let page = render_confirm(&plain_confirm(taken));
        assert!(page.contains("(in use)"), "{page}");
        assert!(page.contains("held by something else"), "{page}");
        assert!(
            page.contains("No remap is offered"),
            "the refusal has to say a remap is not on offer: {page}"
        );
        // Invariant 1 in the wording: no other port number appears anywhere.
        assert!(!page.contains("15173"), "{page}");
    }

    #[test]
    fn a_click_whose_host_could_not_be_resolved_offers_nothing_to_forward() {
        let mut orphan = click();
        orphan.host = None;
        orphan.local_free = None;
        orphan.problem = Some("cannot tell which host to use; configured: a, b".to_string());
        let page = render_confirm(&plain_confirm(orphan));
        assert!(page.contains("cannot tell which host to use"), "{page}");
        assert!(page.contains("Nothing will be forwarded"), "{page}");
        assert!(
            !page.contains("forward and open"),
            "a key that cannot work must not be promised: {page}"
        );
        assert!(!page.contains("forward only"), "{page}");
        // The other surfaces are still worth reaching, and cancelling always is.
        assert!(page.contains("see the forwards already up"), "{page}");
        assert!(page.contains("esc     cancel"), "{page}");
    }

    #[test]
    fn on_the_far_side_the_screen_says_who_will_do_it_instead_of_guessing_the_port() {
        let mut remote = click();
        remote.applies_here = false;
        remote.local_free = None;
        let page = render_confirm(&plain_confirm(remote));
        assert!(
            !page.contains("(free)") && !page.contains("(in use)"),
            "this side cannot know, and must not answer: {page}"
        );
        assert!(page.contains("not the machine with the browser"), "{page}");
        assert!(page.contains("local agent applies it"), "{page}");
        assert!(page.contains("opens the page"), "{page}");
    }

    #[test]
    fn a_short_popup_gives_up_its_sentences_before_its_keys() {
        // The bug this exists for: cutting the tail is the obvious way to make
        // a frame fit, and the tail of this frame is `esc cancel` and the
        // status line.
        let rows = || {
            vec![
                essential("mapping"),
                optional(String::new()),
                optional("why it will not work"),
                essential(String::new()),
                essential("esc cancel"),
            ]
        };
        assert_eq!(
            fit_confirm(rows(), 5),
            vec!["mapping", "", "why it will not work", "", "esc cancel"]
        );
        // Blanks first, from the bottom.
        assert_eq!(
            fit_confirm(rows(), 4),
            vec!["mapping", "", "why it will not work", "esc cancel"]
        );
        assert_eq!(
            fit_confirm(rows(), 3),
            vec!["mapping", "why it will not work", "esc cancel"]
        );
        // Then the sentence, never the key.
        assert_eq!(fit_confirm(rows(), 2), vec!["mapping", "esc cancel"]);
        // And with nothing droppable left the cut comes off the head, not the
        // tail. `truncate` here kept the caption and threw away the way out.
        assert_eq!(fit_confirm(rows(), 1), vec!["esc cancel"]);
        assert!(fit_confirm(rows(), 0).is_empty());
    }

    #[test]
    fn a_popup_too_short_for_its_own_keys_keeps_the_last_rows_rather_than_the_first() {
        // The rows that may not be dropped can outnumber the pane by
        // themselves: every key row is essential, and so are the mapping and
        // the status. Cutting the tail took the status line first and
        // `esc cancel` next — the answer and the way out — and left a title.
        let mut crowded = click();
        crowded.local_free = Some(false);
        crowded.applies_here = false;
        let page = render_confirm(&ConfirmSnapshot {
            height: 4,
            status: Some(Status::failed("ssh: connect to host buildbox: refused")),
            ..plain_confirm(crowded)
        });
        assert_eq!(page.lines().count(), 4, "{page}");
        assert!(
            page.contains("esc     cancel"),
            "the way out went off the bottom:\n{page}"
        );
        assert!(
            page.contains("connect to host buildbox"),
            "the reason went off the bottom:\n{page}"
        );
        assert!(
            !page.contains("Forward a port from"),
            "the caption is what a pane this short can afford to lose:\n{page}"
        );
    }

    #[test]
    fn a_notice_wraps_into_the_indent_it_is_drawn_in_rather_than_being_cut() {
        // Every notice is drawn two cells in, so it has `width - 2` to wrap
        // into. Wrapping to the full width instead is invisible at most widths
        // and then, at the ones where the greedy chunk lands in that two-cell
        // band, `clamp_line` cuts the sentence and leaves a `+` — silently, for
        // every explanatory sentence on the screen.
        let mut crowded = click();
        crowded.local_free = Some(false);
        crowded.applies_here = false;
        let sentences: Vec<String> = confirm_notices(&crowded)
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        assert!(sentences.len() >= 4, "{sentences:?}");
        for width in MIN_WIDTH..=DEFAULT_WIDTH {
            let page = render_confirm(&ConfirmSnapshot {
                width,
                // Tall enough that nothing is dropped for room: this is about
                // the wrap, not about the fit.
                height: 60,
                ..plain_confirm(crowded.clone())
            });
            // Undo the wrap the way a reader does, so a sentence split over two
            // rows still reads as one.
            let flowed = page.lines().map(str::trim).collect::<Vec<_>>().join(" ");
            for sentence in &sentences {
                assert!(
                    flowed.contains(sentence.as_str()),
                    "{sentence:?} did not survive a {width}-cell pane:\n{page}"
                );
            }
        }
    }

    #[test]
    fn the_colour_that_marks_a_collision_is_actually_painted() {
        // Every other test on this screen renders without colour, so the one
        // thing colour is here for — telling "in use" apart from "free" at a
        // glance, and a refusal apart from a note — was drawn by nothing any
        // test looked at.
        let coloured = |state: ConfirmState| {
            render_confirm(&ConfirmSnapshot {
                state,
                color: true,
                ..ConfirmSnapshot::default()
            })
        };

        let mut taken = click();
        taken.local_free = Some(false);
        let page = coloured(taken);
        assert!(
            page.contains(&format!("{RED}(in use){RESET}")),
            "a collision has to be visible without reading: {page:?}"
        );
        assert!(
            page.contains(RED) && page.contains("held by something else"),
            "{page:?}"
        );

        let free = coloured(click());
        assert!(
            free.contains(&format!("{DIM}(free){RESET}")),
            "a free port is a note, not an alarm: {free:?}"
        );
        assert!(
            !free.contains(RED),
            "nothing on this screen is wrong: {free:?}"
        );

        let mut live = click();
        live.already_forwarded = Some(true);
        live.local_free = Some(false);
        let page = coloured(live);
        assert!(
            page.contains(&format!("{DIM}(already forwarded){RESET}")),
            "our own forward holding the port is not a collision: {page:?}"
        );
        assert!(!page.contains(RED), "{page:?}");

        // And the same screen without colour carries no escapes at all, so a
        // pane whose output is captured is still readable.
        assert!(!render_confirm(&plain_confirm(click())).contains('\u{1b}'));
    }

    #[test]
    fn a_sentence_wraps_where_a_reader_sees_it_wrap() {
        assert_eq!(
            wrap_to_width("the local agent applies it", 12),
            ["the local", "agent", "applies it"]
        );
        // Cells, not characters: five kana are ten cells wide.
        assert_eq!(wrap_to_width("あいうえお かき", 10), ["あいうえお", "かき"]);
        // A word with nowhere to fit still gets a line rather than vanishing.
        assert_eq!(
            wrap_to_width("supercalifragilistic", 4),
            ["supercalifragilistic"]
        );
        assert!(wrap_to_width("anything", 0).is_empty());
    }

    #[test]
    fn the_tallest_confirmation_still_fits_the_popup_the_manifest_declares() {
        // Every notice at once: a conflict, an existing forward, and the far
        // side. If this ever needs more rows than CONFIRM_HEIGHT the manifest
        // has to grow with it, or `esc cancel` falls off the bottom.
        let mut crowded = click();
        crowded.local_free = Some(false);
        crowded.already_forwarded = Some(true);
        crowded.applies_here = false;
        let page = render_confirm(&ConfirmSnapshot {
            status: Some(Status::failed("ssh: connect to host buildbox: refused")),
            ..plain_confirm(crowded)
        });
        assert!(
            page.lines().count() <= CONFIRM_HEIGHT,
            "{} lines in a {CONFIRM_HEIGHT}-row popup:\n{page}",
            page.lines().count()
        );
        assert!(page.contains("esc     cancel"), "{page}");
        assert!(page.contains("connect to host buildbox"), "{page}");
    }

    #[test]
    fn a_refusal_too_long_for_one_line_is_wrapped_rather_than_cut_off() {
        // The refusal that matters most on this screen is the local-port
        // collision, and it is several clauses long on purpose: it names the
        // holder, says nothing was changed, and says what a remap would break.
        // Clamped to the pane edge, the user reads the first clause and a `+`.
        let long = "local port 5173 is already in use by another process on this \
                    machine; forwarding buildbox port 5173 there would collide with \
                    it. Nothing was changed";
        let page = render_confirm(&ConfirmSnapshot {
            width: 56,
            status: Some(Status::failed(long)),
            ..plain_confirm(click())
        });
        assert!(page.contains("Nothing was changed"), "{page}");
        assert!(
            !page.contains("proce+") && !page.contains("collide+"),
            "the reason was cut instead of wrapped: {page}"
        );
        for line in page.lines() {
            assert!(ref_width(line) <= 56, "{line:?}");
        }
    }

    #[test]
    fn a_stop_all_key_is_not_reachable_by_a_single_lowercase_press() {
        // Shift is the guard: `x` stops one forward, and the pane must not let
        // a mistyped one take out every forward on screen.
        assert_ne!(list_command(Key::Char('x')), ListCommand::StopShown);
        assert_ne!(list_command(Key::Char('a')), ListCommand::StopShown);
        // And the filter key is not one of the keys that act, either.
        assert_ne!(list_command(Key::Char('h')), ListCommand::StopShown);
        assert_ne!(list_command(Key::Char('h')), ListCommand::Stop);
    }

    #[test]
    fn the_host_filter_key_collides_with_nothing_the_footer_already_promises() {
        // The pane's other keys were chosen with the same care; a filter key
        // that shadowed one would take a stop or a close with it.
        for taken in [
            Key::Enter,
            Key::Char('X'),
            Key::Char('a'),
            Key::Char('r'),
            Key::Char('q'),
            Key::Up,
            Key::Down,
            Key::Left,
            Key::Right,
        ] {
            assert_ne!(
                list_command(taken),
                ListCommand::CycleHost,
                "{taken:?} already does something else on this pane"
            );
        }
        assert_eq!(list_command(Key::Char('h')), ListCommand::CycleHost);
    }

    // ---- driving the panes ----------------------------------------------

    #[derive(Default)]
    struct FakeList {
        rows: Vec<ForwardRow>,
        stopped: Vec<(String, u16)>,
        /// What each `X` was handed, so a test can tell "everything" from
        /// "everything the filter is showing".
        stop_shown_calls: Vec<Vec<(String, u16)>>,
        rows_calls: usize,
        stop_error: Option<String>,
        rows_error: Option<String>,
    }

    impl ListActions for FakeList {
        fn stop(&mut self, row: &ForwardRow) -> std::result::Result<String, String> {
            self.stopped.push((row.host.clone(), row.remote_port));
            if let Some(message) = &self.stop_error {
                return Err(message.clone());
            }
            self.rows
                .retain(|kept| kept.remote_port != row.remote_port || kept.host != row.host);
            Ok(format!("stopped {} {}", row.host, row.remote_port))
        }

        fn stop_shown(&mut self, rows: &[ForwardRow]) -> std::result::Result<String, String> {
            self.stop_shown_calls.push(
                rows.iter()
                    .map(|row| (row.host.clone(), row.remote_port))
                    .collect(),
            );
            let count = rows.len();
            self.rows
                .retain(|kept| !rows.iter().any(|gone| gone == kept));
            Ok(format!("stopped {count} forwards"))
        }

        fn rows(&mut self) -> std::result::Result<Vec<ForwardRow>, String> {
            self.rows_calls += 1;
            match &self.rows_error {
                Some(message) => Err(message.clone()),
                None => Ok(self.rows.clone()),
            }
        }
    }

    #[derive(Default)]
    struct FakePicker {
        rows: Vec<ListenerRow>,
        forwarded: Vec<Vec<u16>>,
        rows_calls: usize,
        forward_error: Option<String>,
        rows_error: Option<String>,
    }

    impl PickerActions for FakePicker {
        fn forward(&mut self, rows: &[ListenerRow]) -> std::result::Result<String, String> {
            self.forwarded
                .push(rows.iter().map(|row| row.remote_port).collect());
            match &self.forward_error {
                Some(message) => Err(message.clone()),
                None => Ok(format!("forwarded {} port(s)", rows.len())),
            }
        }

        fn rows(&mut self) -> std::result::Result<Vec<ListenerRow>, String> {
            self.rows_calls += 1;
            match &self.rows_error {
                Some(message) => Err(message.clone()),
                None => Ok(self.rows.clone()),
            }
        }
    }

    fn drive_list(app: &mut FakeList, keys: &[u8]) -> (Outcome, String) {
        drive_list_from(app, plain_list(app.rows.clone()), keys)
    }

    /// [`drive_list`] over a snapshot the test built, for the opening states
    /// `main` can hand the pane that its own rows cannot describe.
    fn drive_list_from(
        app: &mut FakeList,
        snapshot: ListSnapshot,
        keys: &[u8],
    ) -> (Outcome, String) {
        let mut input = keys;
        let mut out: Vec<u8> = Vec::new();
        let outcome = run_list(app, snapshot, &mut input, &mut out).expect("streams never fail");
        (outcome, String::from_utf8_lossy(&out).into_owned())
    }

    fn drive_picker(app: &mut FakePicker, keys: &[u8]) -> (Outcome, String) {
        drive_picker_from(app, plain_picker(app.rows.clone()), keys)
    }

    fn drive_picker_from(
        app: &mut FakePicker,
        snapshot: PickerSnapshot,
        keys: &[u8],
    ) -> (Outcome, String) {
        let mut input = keys;
        let mut out: Vec<u8> = Vec::new();
        let outcome = run_picker(app, snapshot, &mut input, &mut out).expect("streams never fail");
        (outcome, String::from_utf8_lossy(&out).into_owned())
    }

    /// Records what the confirmation asked for, so a test can tell "forward and
    /// open" from "forward only" — which is the whole reason the screen exists.
    #[derive(Default)]
    struct FakeConfirm {
        forwards: Vec<bool>,
        error: Option<String>,
    }

    impl ConfirmActions for FakeConfirm {
        fn forward(&mut self, open_browser: bool) -> std::result::Result<String, String> {
            self.forwards.push(open_browser);
            match &self.error {
                Some(message) => Err(message.clone()),
                None => Ok(if open_browser {
                    "forwarded and opened".to_string()
                } else {
                    "forwarded".to_string()
                }),
            }
        }
    }

    fn drive_confirm(app: &mut FakeConfirm, state: ConfirmState, keys: &[u8]) -> (Outcome, String) {
        let snapshot = plain_confirm(state);
        let mut input = keys;
        let mut out: Vec<u8> = Vec::new();
        let outcome = run_confirm(app, snapshot, &mut input, &mut out).expect("streams never fail");
        (outcome, String::from_utf8_lossy(&out).into_owned())
    }

    #[test]
    fn enter_forwards_and_opens_while_f_forwards_and_opens_nothing() {
        // The decision the screen was added for: the browser is a choice on the
        // screen rather than something a click implies.
        let mut app = FakeConfirm::default();
        let (outcome, _) = drive_confirm(&mut app, click(), b"\r");
        assert_eq!(app.forwards, [true]);
        assert_eq!(outcome.exit, Exit::Closed);
        assert_eq!(outcome.handoff, None);
        assert_eq!(
            outcome.status.map(|status| status.text).as_deref(),
            Some("forwarded and opened")
        );

        let mut app = FakeConfirm::default();
        let (outcome, _) = drive_confirm(&mut app, click(), b"f");
        assert_eq!(app.forwards, [false]);
        assert_eq!(outcome.exit, Exit::Closed);
    }

    #[test]
    fn a_refused_forward_keeps_the_screen_up_carrying_the_reason() {
        // Closing on a failure would take the explanation with it, and the
        // explanation is the whole product here.
        let mut app = FakeConfirm {
            error: Some("local port 5173 is already in use by another process".to_string()),
            ..FakeConfirm::default()
        };
        let (outcome, screen) = drive_confirm(&mut app, click(), b"\r");
        assert_eq!(app.forwards, [true], "it must have been attempted");
        assert_eq!(outcome.exit, Exit::Closed, "only EOF ended it");
        let last = frames(&screen).last().copied().unwrap_or_default();
        assert!(last.contains("already in use"), "{last}");
    }

    #[test]
    fn escape_forwards_nothing_at_all() {
        let mut app = FakeConfirm::default();
        let (outcome, _) = drive_confirm(&mut app, click(), b"\x1b");
        assert!(app.forwards.is_empty(), "esc is the way to say no");
        assert_eq!(outcome.exit, Exit::Closed);
        assert_eq!(outcome.handoff, None);
    }

    #[test]
    fn a_click_with_no_host_refuses_the_forward_keys_and_says_why() {
        let mut orphan = click();
        orphan.host = None;
        orphan.problem = Some("cannot tell which host to use".to_string());
        let mut app = FakeConfirm::default();
        let (_, screen) = drive_confirm(&mut app, orphan, b"\rf");
        assert!(
            app.forwards.is_empty(),
            "there is no host, so nothing may be forwarded"
        );
        let last = frames(&screen).last().copied().unwrap_or_default();
        assert!(last.contains("no host to forward from"), "{last}");
    }

    #[test]
    fn the_cross_link_keys_leave_the_pane_naming_the_one_to_open_next() {
        // The pane never opens anything itself: it is a popup, and Herdr keeps
        // one popup slot which this pane is holding until its process exits.
        for (key, expected) in [(b'l', Handoff::List), (b's', Handoff::Picker)] {
            let mut app = FakeConfirm::default();
            let (outcome, _) = drive_confirm(&mut app, click(), &[key]);
            assert_eq!(outcome.handoff, Some(expected), "{}", key as char);
            assert_eq!(outcome.exit, Exit::Closed);
            assert!(app.forwards.is_empty(), "a cross-link forwards nothing");
        }
    }

    #[test]
    fn the_forward_manager_reaches_the_picker() {
        // One click into any of these surfaces should reach the rest.
        let mut app = FakeList {
            rows: vec![row("buildbox", 5173, 5173, Some("mini: scholion"))],
            ..FakeList::default()
        };
        let (outcome, _) = drive_list(&mut app, b"a");
        assert_eq!(outcome.handoff, Some(Handoff::Picker));
        assert!(app.stopped.is_empty(), "`a` must not stop anything");
    }

    #[test]
    fn an_empty_forward_manager_still_offers_the_way_to_add_one() {
        // The pane a user reaches with nothing forwarded is exactly where the
        // way to forward something matters most, and its footer is not the
        // populated one.
        let page = render_list(&plain_list(Vec::new()));
        assert!(page.contains("a add"), "{page}");
        assert!(page.contains("press a to pick one"), "{page}");
    }

    /// Hands over one keystroke per read, the way a terminal does.
    ///
    /// A slice yields every byte in one read, so a whole burst of keys is
    /// applied between two frames — which hides anything the pane forgets from
    /// one redraw to the next.
    struct Keystrokes<'a> {
        keys: &'a [u8],
    }

    impl Read for Keystrokes<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.keys.split_first() {
                Some((first, rest)) if !buf.is_empty() => {
                    buf[0] = *first;
                    self.keys = rest;
                    Ok(1)
                }
                _ => Ok(0),
            }
        }
    }

    /// The frames a pane drew, one per repaint.
    fn frames(screen: &str) -> Vec<&str> {
        screen.split("\x1b[2J").skip(1).collect()
    }

    #[test]
    fn walking_up_a_long_table_moves_the_highlight_before_it_scrolls_the_pane() {
        // End, then Up, in a pane that shows five of twenty rows. Anchoring the
        // window to the selection alone put the highlight on the bottom line and
        // left it there, so every Up scrolled the whole table under a highlight
        // that never moved.
        let rows: Vec<ForwardRow> = (0..20)
            .map(|i| row("buildbox", 9000 + i, 9000 + i, Some("mini: scholion")))
            .collect();
        let mut app = FakeList {
            rows: rows.clone(),
            ..FakeList::default()
        };
        let snapshot = ListSnapshot {
            rows,
            height: 12,
            color: false,
            ..ListSnapshot::default()
        };
        let mut input = Keystrokes { keys: b"Gkk" };
        let mut out: Vec<u8> = Vec::new();
        run_list(&mut app, snapshot, &mut input, &mut out).expect("streams never fail");
        let screen = String::from_utf8_lossy(&out).into_owned();
        let drawn = frames(&screen);
        assert_eq!(drawn.len(), 4, "one frame per key, plus the first");

        let marked = |frame: &str| {
            body_lines(frame)
                .into_iter()
                .find(|line| line.starts_with("> "))
                .unwrap_or_else(|| panic!("no highlighted row in:\n{frame}"))
                .to_string()
        };
        assert!(marked(drawn[1]).contains("9019"), "{}", drawn[1]);
        assert!(marked(drawn[2]).contains("9018"), "{}", drawn[2]);
        assert!(marked(drawn[3]).contains("9017"), "{}", drawn[3]);
        for frame in &drawn[1..] {
            for port in 9015..=9019 {
                assert!(
                    frame.contains(&port.to_string()),
                    "row {port} left the window while the highlight had room to \
                     climb:\n{frame}"
                );
            }
            assert!(
                !frame.contains("9014"),
                "the table scrolled instead of moving the highlight:\n{frame}"
            );
        }
    }

    #[test]
    fn the_picker_scrolls_the_way_the_manager_does() {
        // The same offset, kept by the other driver: two loops, two places to
        // forget it.
        let rows: Vec<ListenerRow> = (0..20)
            .map(|i| listener_row(9000 + i, "node", Some("mini: scholion")))
            .collect();
        let mut app = FakePicker {
            rows: rows.clone(),
            ..FakePicker::default()
        };
        let snapshot = PickerSnapshot {
            host: "buildbox".into(),
            rows,
            height: 12,
            color: false,
            ..PickerSnapshot::default()
        };
        let mut input = Keystrokes { keys: b"Gk" };
        let mut out: Vec<u8> = Vec::new();
        run_picker(&mut app, snapshot, &mut input, &mut out).expect("streams never fail");
        let screen = String::from_utf8_lossy(&out).into_owned();
        let drawn = frames(&screen);
        let last = drawn.last().copied().unwrap_or_default();
        assert!(
            last.contains("9019") && !last.contains("9014"),
            "the table scrolled instead of moving the highlight:\n{last}"
        );
    }

    fn two_forwards() -> Vec<ForwardRow> {
        vec![
            row("buildbox", 5173, 5173, Some("mini: scholion")),
            row("buildbox", 8787, 8787, Some("mini: api")),
        ]
    }

    #[test]
    fn enter_stops_the_highlighted_forward_and_the_pane_stays_open() {
        let mut app = FakeList {
            rows: two_forwards(),
            ..FakeList::default()
        };
        let (outcome, screen) = drive_list(&mut app, b"j\r");
        assert_eq!(app.stopped, [("buildbox".to_string(), 8787)]);
        assert_eq!(outcome.exit, Exit::Closed, "end of input closes it");
        assert!(screen.contains("stopped buildbox 8787"), "{screen}");
        assert!(app.rows_calls >= 1, "the table must refresh after a stop");
    }

    #[test]
    fn stopping_a_forward_removes_it_from_the_next_frame() {
        let mut app = FakeList {
            rows: two_forwards(),
            ..FakeList::default()
        };
        let (_, screen) = drive_list(&mut app, b"\r");
        let last_frame = screen.rsplit("\x1b[2J").next().unwrap_or_default();
        assert!(last_frame.contains("(1 live, all hosts)"), "{last_frame}");
        assert!(!last_frame.contains("mini: scholion"), "{last_frame}");
        assert!(last_frame.contains("mini: api"), "{last_frame}");
    }

    #[test]
    fn a_refused_stop_is_reported_in_place_instead_of_closing_the_pane() {
        let mut app = FakeList {
            rows: two_forwards(),
            stop_error: Some("ssh refused: port not found".into()),
            ..FakeList::default()
        };
        // Enter alone: the frame carrying the refusal is drawn before the
        // pane sees end of input, which is what an interactive user would see.
        let (outcome, screen) = drive_list(&mut app, b"\r");
        assert_eq!(outcome.exit, Exit::Closed);
        let status = outcome.status.expect("a refusal must leave a status");
        assert!(status.error, "a refusal is not an ordinary message");
        assert!(status.text.contains("port not found"));
        assert!(screen.contains("port not found"), "{screen}");
        assert!(screen.contains("5173"), "the row must survive a refusal");
    }

    #[test]
    fn shift_x_stops_every_forward_at_once() {
        let mut app = FakeList {
            rows: two_forwards(),
            ..FakeList::default()
        };
        let (_, screen) = drive_list(&mut app, b"X");
        assert_eq!(app.stop_shown_calls.len(), 1);
        assert_eq!(
            app.stop_shown_calls[0],
            [
                ("buildbox".to_string(), 5173),
                ("buildbox".to_string(), 8787)
            ],
            "with no filter on, `shown` is every row"
        );
        assert!(app.stopped.is_empty(), "stop-all must not stop one by one");
        assert!(screen.contains("stopped 2 forwards"), "{screen}");
    }

    #[test]
    fn enter_stops_the_row_on_the_row_s_own_host() {
        // The pane lists two machines; the key has to act on the one under the
        // highlight rather than on whatever host the invocation resolved to.
        let mut app = FakeList {
            rows: two_hosts(),
            ..FakeList::default()
        };
        let (_, screen) = drive_list(&mut app, b"j\r");
        assert_eq!(app.stopped, [("mac-studio-honkoma".to_string(), 5178)]);
        assert!(
            screen.contains("stopped mac-studio-honkoma 5178"),
            "{screen}"
        );
    }

    #[test]
    fn h_narrows_to_one_host_and_then_back_to_every_host() {
        let mut app = FakeList {
            rows: two_hosts(),
            ..FakeList::default()
        };
        let mut input = Keystrokes { keys: b"hhh" };
        let mut out: Vec<u8> = Vec::new();
        run_list(&mut app, plain_list(two_hosts()), &mut input, &mut out)
            .expect("streams never fail");
        let screen = String::from_utf8_lossy(&out).into_owned();
        let drawn = frames(&screen);
        assert_eq!(drawn.len(), 4, "one frame per key, plus the first");
        assert!(drawn[0].contains("(2 live, all hosts)"), "{}", drawn[0]);
        assert!(
            drawn[1].contains("host mac-mini-honkoma only") && !drawn[1].contains("5178"),
            "{}",
            drawn[1]
        );
        assert!(
            drawn[2].contains("host mac-studio-honkoma only") && !drawn[2].contains("8788"),
            "{}",
            drawn[2]
        );
        assert!(drawn[3].contains("(2 live, all hosts)"), "{}", drawn[3]);
        assert!(
            app.stopped.is_empty(),
            "a filter changes nothing that is up"
        );
    }

    #[test]
    fn a_filter_key_with_one_host_says_so_instead_of_redrawing_the_same_frame() {
        let mut app = FakeList {
            rows: two_forwards(),
            ..FakeList::default()
        };
        let (_, screen) = drive_list(&mut app, b"h");
        assert!(screen.contains("every forward is on buildbox"), "{screen}");
    }

    #[test]
    fn shift_x_stops_what_the_filter_is_showing_and_nothing_behind_it() {
        // What `X` means once the pane shows every host: everything on screen.
        // The footer says "stop shown" and this is the sentence it makes true.
        let mut app = FakeList {
            rows: two_hosts(),
            ..FakeList::default()
        };
        let mut input = Keystrokes { keys: b"hX" };
        let mut out: Vec<u8> = Vec::new();
        run_list(&mut app, plain_list(two_hosts()), &mut input, &mut out)
            .expect("streams never fail");
        assert_eq!(
            app.stop_shown_calls,
            [vec![("mac-mini-honkoma".to_string(), 8788)]],
            "the forward on the other host was not on screen"
        );
    }

    /// The last thing painted, which is what the user is looking at.
    fn last_frame(screen: &str) -> String {
        frames(screen)
            .last()
            .copied()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn no_key_replaces_the_reason_a_listing_failed_with_a_report_about_hosts() {
        // `h` was the one that shipped: it overwrote the status with "there is
        // nothing to filter" whenever the snapshot held no rows, so one
        // keystroke turned a screen that said `unknown host "stale-name"` into
        // one that said the tool had looked and found nothing to narrow. Every
        // key that finds no rows is the same shape, so every key is checked.
        for keys in [&b"h"[..], b"\r", b"x", b"X", b"j", b"G"] {
            let mut app = FakeList {
                rows_error: Some("the pane must not have refetched".into()),
                ..FakeList::default()
            };
            let (outcome, screen) = drive_list_from(&mut app, unlisted_list(), keys);
            let frame = last_frame(&screen);
            assert!(
                frame.contains(NO_LISTING),
                "{keys:?} lost the reason the table is empty:\n{frame}"
            );
            for claim in [
                "there is nothing to filter",
                "nothing to stop",
                "No forwards are recorded on any host.",
                "all hosts",
            ] {
                assert!(
                    !frame.contains(claim),
                    "{keys:?} answered {claim:?} for hosts it never reached:\n{frame}"
                );
            }
            assert_eq!(
                outcome.status.map(|status| status.text).as_deref(),
                Some(NO_LISTING),
                "the status the pane hands back to the plugin log changed too, on {keys:?}"
            );
            assert!(app.stopped.is_empty() && app.stop_shown_calls.is_empty());
            assert_eq!(app.rows_calls, 0, "{keys:?} refetched nothing to act on");
        }
    }

    #[test]
    fn r_over_a_failed_listing_replaces_it_with_the_listing_and_drops_the_reason() {
        // The other half: the reason survives until something changes it, and a
        // refresh that worked is that something. Leaving it up would caption a
        // real table with the failure that preceded it.
        let mut app = FakeList {
            rows: two_hosts(),
            ..FakeList::default()
        };
        let (outcome, screen) = drive_list_from(&mut app, unlisted_list(), b"r");
        let frame = last_frame(&screen);
        assert_eq!(app.rows_calls, 1);
        assert!(frame.contains("8788") && frame.contains("5178"), "{frame}");
        assert!(frame.contains("(2 live, all hosts)"), "{frame}");
        assert!(
            !frame.contains(NO_LISTING),
            "a listing arrived; the reason it had not is stale:\n{frame}"
        );
        assert_eq!(outcome.status, None);
    }

    #[test]
    fn a_refresh_that_fails_again_keeps_saying_why() {
        let mut app = FakeList {
            rows_error: Some("ssh: connect to host mini port 22: no route".into()),
            ..FakeList::default()
        };
        let (_, screen) = drive_list_from(&mut app, unlisted_list(), b"r");
        let frame = last_frame(&screen);
        assert!(frame.contains("no route"), "{frame}");
        assert!(frame.contains("Could not list the forwards."), "{frame}");
        assert!(!frame.contains("all hosts"), "{frame}");
    }

    #[test]
    fn enter_on_a_picker_that_could_not_scan_keeps_the_reason() {
        // The picker has the same fallback-to-empty shape, and the same key
        // that assumes the listing succeeded.
        let mut app = FakePicker::default();
        let (_, screen) = drive_picker_from(&mut app, unlisted_picker(), b"\r ");
        let frame = last_frame(&screen);
        assert!(frame.contains(NO_LISTING), "{frame}");
        assert!(
            !frame.contains("nothing to forward"),
            "a scan that never ran established no such thing:\n{frame}"
        );
        assert!(!frame.contains("belongs to a Herdr pane"), "{frame}");
        assert!(app.forwarded.is_empty());
    }

    #[test]
    fn r_over_a_failed_scan_replaces_it_with_the_listeners() {
        let mut app = FakePicker {
            rows: vec![listener_row(5173, "node", Some("mini: scholion"))],
            ..FakePicker::default()
        };
        let (outcome, screen) = drive_picker_from(&mut app, unlisted_picker(), b"r");
        let frame = last_frame(&screen);
        assert!(frame.contains("5173") && frame.contains("node"), "{frame}");
        assert!(!frame.contains(NO_LISTING), "{frame}");
        assert_eq!(outcome.status, None);
    }

    #[test]
    fn stopping_with_nothing_to_stop_calls_nothing() {
        let mut app = FakeList::default();
        let (_, screen) = drive_list(&mut app, b"\rX");
        assert!(app.stopped.is_empty());
        assert!(app.stop_shown_calls.is_empty());
        assert!(screen.contains("nothing to stop"), "{screen}");
    }

    #[test]
    fn q_escape_and_ctrl_c_all_close_but_ctrl_c_says_so() {
        for (keys, exit) in [
            (&b"q"[..], Exit::Closed),
            (&b"\x1b"[..], Exit::Closed),
            (&b"\x03"[..], Exit::Interrupted),
        ] {
            let mut app = FakeList {
                rows: two_forwards(),
                ..FakeList::default()
            };
            let (outcome, _) = drive_list(&mut app, keys);
            assert_eq!(outcome.exit, exit, "keys {keys:?}");
            assert!(app.stopped.is_empty(), "closing must not stop anything");
        }
    }

    #[test]
    fn end_of_input_closes_the_pane_without_touching_anything() {
        let mut app = FakeList {
            rows: two_forwards(),
            ..FakeList::default()
        };
        let (outcome, screen) = drive_list(&mut app, b"");
        assert_eq!(outcome.exit, Exit::Closed);
        assert_eq!(app.rows_calls, 0);
        assert!(screen.contains("5173"), "one frame is still drawn");
    }

    #[test]
    fn a_failed_refresh_keeps_the_rows_already_on_screen() {
        let mut app = FakeList {
            rows: two_forwards(),
            rows_error: Some("host unreachable".into()),
            ..FakeList::default()
        };
        let (outcome, screen) = drive_list(&mut app, b"r");
        assert!(outcome.status.map(|s| s.error).unwrap_or(false));
        let last_frame = screen.rsplit("\x1b[2J").next().unwrap_or_default();
        assert!(
            last_frame.contains("5173"),
            "rows were blanked:\n{last_frame}"
        );
        assert!(last_frame.contains("host unreachable"), "{last_frame}");
    }

    #[test]
    fn the_pane_restores_the_cursor_on_the_way_out() {
        let mut app = FakeList::default();
        let (_, screen) = drive_list(&mut app, b"q");
        assert!(
            screen.contains("\x1b[?25l"),
            "the cursor should be hidden while drawing"
        );
        assert!(screen.ends_with("\x1b[?2026l\x1b[?25h\r\n"), "{screen:?}");
    }

    #[test]
    fn frames_use_carriage_returns_because_raw_mode_needs_them() {
        let mut app = FakeList {
            rows: two_forwards(),
            ..FakeList::default()
        };
        let (_, screen) = drive_list(&mut app, b"q");
        assert!(screen.contains("\r\n"), "{screen:?}");
        assert!(
            !screen.contains("live)\n"),
            "a bare newline would stairstep every row: {screen:?}"
        );
    }

    #[test]
    fn crlf_only_rewrites_line_endings() {
        assert_eq!(crlf("a\nb"), "a\r\nb");
        assert_eq!(crlf(""), "");
        assert_eq!(crlf("no newline"), "no newline");
    }

    #[test]
    fn the_picker_forwards_the_highlighted_row_and_closes() {
        let mut app = FakePicker {
            rows: vec![
                listener_row(5173, "node", Some("mini: scholion")),
                listener_row(8787, "workerd", Some("mini: api")),
            ],
            ..FakePicker::default()
        };
        let (outcome, _) = drive_picker(&mut app, b"j\r");
        assert_eq!(app.forwarded, [vec![8787]]);
        assert_eq!(outcome.exit, Exit::Closed);
        assert_eq!(
            outcome.status.map(|s| s.text),
            Some("forwarded 1 port(s)".to_string())
        );
    }

    #[test]
    fn the_picker_forwards_every_marked_row_at_once() {
        let mut app = FakePicker {
            rows: vec![
                listener_row(5173, "node", Some("mini: scholion")),
                listener_row(8787, "workerd", Some("mini: api")),
                listener_row(3000, "next", Some("mini: web")),
            ],
            ..FakePicker::default()
        };
        // mark the first, move down twice, mark the third, confirm
        let (_, _) = drive_picker(&mut app, b" jj \r");
        assert_eq!(app.forwarded, [vec![5173, 3000]]);
    }

    #[test]
    fn marking_and_unmarking_the_same_row_leaves_it_out() {
        let mut app = FakePicker {
            rows: vec![
                listener_row(5173, "node", None),
                listener_row(8787, "workerd", None),
            ],
            ..FakePicker::default()
        };
        let (_, _) = drive_picker(&mut app, b"  j\r");
        assert_eq!(
            app.forwarded,
            [vec![8787]],
            "an unmarked row must not tag along"
        );
    }

    #[test]
    fn a_refused_forward_keeps_the_popup_open_with_the_reason() {
        let mut app = FakePicker {
            rows: vec![listener_row(5173, "node", Some("mini: scholion"))],
            forward_error: Some("local port 5173 is already in use".into()),
            ..FakePicker::default()
        };
        let (outcome, screen) = drive_picker(&mut app, b"\r");
        assert_eq!(app.forwarded, [vec![5173]]);
        assert_eq!(outcome.exit, Exit::Closed);
        assert!(screen.contains("already in use"), "{screen}");
        assert!(
            app.rows_calls >= 1,
            "the offer list should refresh after a refusal"
        );
    }

    #[test]
    fn confirming_an_empty_picker_forwards_nothing() {
        let mut app = FakePicker::default();
        let (_, screen) = drive_picker(&mut app, b"\r");
        assert!(app.forwarded.is_empty());
        assert!(screen.contains("nothing to forward"), "{screen}");
    }

    #[test]
    fn ctrl_c_in_the_picker_forwards_nothing() {
        let mut app = FakePicker {
            rows: vec![listener_row(5173, "node", None)],
            ..FakePicker::default()
        };
        let (outcome, _) = drive_picker(&mut app, b"\x03");
        assert_eq!(outcome.exit, Exit::Interrupted);
        assert!(
            app.forwarded.is_empty(),
            "invariant 7: no forward without a choice"
        );
    }

    #[test]
    fn moving_past_the_ends_of_the_picker_never_selects_a_missing_row() {
        let mut app = FakePicker {
            rows: vec![
                listener_row(5173, "node", None),
                listener_row(8787, "n", None),
            ],
            ..FakePicker::default()
        };
        // down four times in a two-row list, then confirm
        let (_, _) = drive_picker(&mut app, b"jjjj\r");
        assert_eq!(
            app.forwarded,
            [vec![5173]],
            "four wraps land back on the first"
        );
    }

    #[test]
    fn a_paste_of_keys_is_applied_in_order_and_still_ends_cleanly() {
        let mut app = FakeList {
            rows: two_forwards(),
            ..FakeList::default()
        };
        let (outcome, _) = drive_list(&mut app, b"jkjq\x1b[A");
        assert_eq!(outcome.exit, Exit::Closed);
        assert!(app.stopped.is_empty());
    }

    // ---- the terminal edge ----------------------------------------------

    #[test]
    fn raw_mode_is_skipped_when_the_stream_is_not_a_terminal() {
        // /dev/null is a character device but not a tty, which is what a
        // captured plugin command looks like.
        let file = std::fs::File::open("/dev/null").expect("/dev/null exists");
        let guard = RawMode::enter(file.as_raw_fd()).expect("a non-tty is not an error");
        assert!(guard.is_none(), "nothing to restore, so nothing to enter");
    }

    #[test]
    fn window_size_of_a_non_terminal_is_unknown_rather_than_zero() {
        let file = std::fs::File::open("/dev/null").expect("/dev/null exists");
        assert_eq!(window_size(file.as_raw_fd()), None);
    }

    #[test]
    fn terminal_size_always_answers_with_something_usable() {
        let (cols, rows) = terminal_size();
        assert!(cols > 0 && rows > 0);
    }
}
