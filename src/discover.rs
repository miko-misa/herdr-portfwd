//! Finding remote listeners and deciding whose they are.
//!
//! Two parsers, both pure over `&str`:
//! - `lsof -nP -iTCP -sTCP:LISTEN` on macOS, whose `COMMAND` column is padded
//!   and truncated and can contain spaces, and whose `NAME` column carries the
//!   bind address as `127.0.0.1:8501`, `*:53`, or `[::1]:8501`;
//! - `ss -ltnH` on Linux, whose address column uses `0.0.0.0:5173`, `*:5173`,
//!   and `[::]:5173`, and whose `users:(("node",pid=123,fd=20))` field carries
//!   the owning pid.
//!
//! Then attribution: `ps -eo pid=,ppid=` gives a parent map, and a listener
//! belongs to a pane when walking its ancestry reaches that pane's `shell_pid`.
//!
//! The ancestry walk must terminate on any input. PID reuse can produce cycles
//! and a malformed `ps` can produce a self-parent, so the walk is both
//! cycle-guarded and depth-capped. This is invariant 5 in `ARCHITECTURE.md`.

use std::collections::{HashMap, HashSet};

use crate::types::{AttributedListener, Listener, PaneRef};

/// Remote command for macOS hosts.
///
/// Field output rather than the human table: `-F` prints one value per line, so
/// a `COMMAND` containing spaces stays a single value, and it is not truncated
/// to nine characters the way the table's column is.
pub const LSOF_LISTENERS_COMMAND: &str = "lsof -nP -iTCP -sTCP:LISTEN -F pcn";

/// Remote command for Linux hosts. `-p` is what carries the owning pid; without
/// it every listener parses as unowned and can never be attributed.
pub const SS_LISTENERS_COMMAND: &str = "ss -ltnpH";

/// Remote command for the parent map, on both platforms. `=` on each column
/// suppresses the header, so there is nothing to skip.
pub const PS_TREE_COMMAND: &str = "ps -eo pid=,ppid=";

/// Hops the ancestry walk will take before giving up.
///
/// Deep process trees are already pathological at a fraction of this; the cap
/// exists so a `ps` snapshot with a long chain, or a graph the visited set
/// somehow admits, cannot turn into unbounded work. Paired with the visited set
/// rather than trusted alone: see invariant 5.
pub const MAX_ANCESTRY_DEPTH: usize = 64;

/// Pid used for a listener whose owning process could not be determined, which
/// `ss` reports when it is run without privileges to see another user's socket.
/// Real pid 0 is the kernel, so it can never collide with a pane's shell.
pub const UNKNOWN_PID: u32 = 0;

/// A pid-to-parent map, as read out of `ps`.
///
/// Deliberately not a tree in the data-structure sense: the input is a snapshot
/// of a live system and may not be acyclic. Every traversal is guarded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessTree {
    parents: HashMap<u32, u32>,
}

impl ProcessTree {
    /// Build from `(pid, ppid)` pairs. The first entry for a pid wins: a pid
    /// repeated in one snapshot means the snapshot raced with a reparent, and
    /// ignoring the later line keeps parsing independent of trailing garbage.
    pub fn from_pairs<I>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (u32, u32)>,
    {
        let mut parents = HashMap::new();
        for (pid, ppid) in pairs {
            parents.entry(pid).or_insert(ppid);
        }
        Self { parents }
    }

    pub fn parent(&self, pid: u32) -> Option<u32> {
        self.parents.get(&pid).copied()
    }

    pub fn len(&self) -> usize {
        self.parents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    /// `pid` followed by its ancestors, nearest first.
    ///
    /// Terminates on any input: a pid is visited at most once, and at most
    /// [`MAX_ANCESTRY_DEPTH`] hops are taken, so the result holds at most
    /// `MAX_ANCESTRY_DEPTH + 1` entries.
    pub fn ancestry(&self, pid: u32) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut current = pid;
        while chain.len() <= MAX_ANCESTRY_DEPTH {
            if !seen.insert(current) {
                break;
            }
            chain.push(current);
            match self.parents.get(&current) {
                Some(&parent) => current = parent,
                None => break,
            }
        }
        chain
    }

    /// Whether `pid` is `ancestor` or is reachable from it by walking parents.
    /// A process descends from itself, which is what lets a shell that opened
    /// the socket directly be attributed to its own pane.
    pub fn descends_from(&self, pid: u32, ancestor: u32) -> bool {
        self.ancestry(pid).contains(&ancestor)
    }
}

/// Parse `ps -eo pid=,ppid=`: two integer columns, arbitrarily indented.
///
/// A line that is not exactly two integers is dropped rather than guessed at;
/// a header that slipped through, a warning, or a truncated final line all land
/// there.
pub fn parse_ps(text: &str) -> ProcessTree {
    let mut pairs = Vec::new();
    for line in text.lines() {
        let mut columns = line.split_whitespace();
        let (Some(pid), Some(ppid), None) = (columns.next(), columns.next(), columns.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        pairs.push((pid, ppid));
    }
    ProcessTree::from_pairs(pairs)
}

/// Parse `lsof -nP -iTCP -sTCP:LISTEN`, in either the `-F pcn` field format or
/// the human table, whichever the input looks like.
///
/// The field format is what this crate asks for; the table is handled because a
/// host may have an `lsof` old enough to ignore `-F`, and because a user
/// pasting output into a bug report will paste the table.
///
/// Every malformed record is skipped. There is no failure mode here that should
/// cost the caller the listeners it *could* read.
pub fn parse_lsof(text: &str) -> Vec<Listener> {
    if text.lines().any(is_field_pid_line) {
        parse_lsof_fields(text)
    } else {
        parse_lsof_table(text)
    }
}

/// A `-F` pid line is the letter `p` followed by digits and nothing else. No
/// row of the human table can look like that: every one of them has columns.
fn is_field_pid_line(line: &str) -> bool {
    match line.strip_prefix('p') {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

fn parse_lsof_fields(text: &str) -> Vec<Listener> {
    let mut out = Dedup::default();
    let mut pid: Option<u32> = None;
    let mut command = String::new();

    for line in text.lines() {
        let Some((tag, value)) = split_field(line) else {
            continue;
        };
        match tag {
            // A `p` line opens a new process set; everything after it belongs
            // to that process until the next one.
            'p' => {
                pid = value.parse::<u32>().ok();
                command.clear();
            }
            'c' => command = value.to_string(),
            'n' => {
                let (Some(pid), Some((bind, port))) = (pid, split_addr_port(value)) else {
                    continue;
                };
                out.push(Listener {
                    port,
                    bind,
                    pid,
                    process: command.clone(),
                });
            }
            _ => {}
        }
    }
    out.into_vec()
}

fn split_field(line: &str) -> Option<(char, &str)> {
    let mut chars = line.chars();
    let tag = chars.next()?;
    Some((tag, chars.as_str()))
}

/// Parse the human table.
///
/// Columns are counted from the *right*, because `COMMAND` is the one field
/// that can contain spaces and it is leftmost. A listening TCP row always ends
/// `... TCP <addr> (LISTEN)`, which pins the last three, and the eight fixed
/// columns before `NAME` put `PID` at `len - 9`. Whatever is left of that is
/// the command, spaces included.
fn parse_lsof_table(text: &str) -> Vec<Listener> {
    const FIELDS_AFTER_COMMAND: usize = 9;

    let mut out = Dedup::default();
    for line in text.lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() <= FIELDS_AFTER_COMMAND {
            continue;
        }
        let tail = columns.len() - FIELDS_AFTER_COMMAND;
        if columns[columns.len() - 1] != "(LISTEN)" || columns[columns.len() - 3] != "TCP" {
            continue;
        }
        let Ok(pid) = columns[tail].parse::<u32>() else {
            continue;
        };
        let Some((bind, port)) = split_addr_port(columns[columns.len() - 2]) else {
            continue;
        };
        out.push(Listener {
            port,
            bind,
            pid,
            process: columns[..tail].join(" "),
        });
    }
    out.into_vec()
}

/// Parse `ss -ltnpH`.
///
/// A row whose `users:(...)` field is missing still yields a listener, owned by
/// [`UNKNOWN_PID`]: the socket really is listening, and dropping it would hide
/// a port the user can still choose to forward explicitly. It simply can never
/// be attributed to a pane.
pub fn parse_ss(text: &str) -> Vec<Listener> {
    let mut out = Dedup::default();
    for line in text.lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        // State, Recv-Q, Send-Q, Local Address:Port, and usually more.
        if columns.len() < 4 || !columns[0].eq_ignore_ascii_case("LISTEN") {
            continue;
        }
        let Some((bind, port)) = split_addr_port(columns[3]) else {
            continue;
        };
        // Searched over the whole line, not one column: the process field is
        // last and space-free, but its position moves between ss versions.
        let (pid, process) = parse_ss_users(line).unwrap_or((UNKNOWN_PID, String::new()));
        out.push(Listener {
            port,
            bind,
            pid,
            process,
        });
    }
    out.into_vec()
}

/// First `("name",pid=N,...)` entry of an `ss` users field. A socket shared by
/// several processes lists them all; the first is the one that opened it.
fn parse_ss_users(line: &str) -> Option<(u32, String)> {
    const OPEN: &str = "users:((";
    let rest = &line[line.find(OPEN)? + OPEN.len()..];
    let process = rest
        .strip_prefix('"')
        .and_then(|r| r.split_once('"'))
        .map(|(name, _)| name.to_string())
        .unwrap_or_default();
    let digits = &rest[rest.find("pid=")? + "pid=".len()..];
    let pid: u32 = digits
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|d| !d.is_empty())?
        .parse()
        .ok()?;
    Some((pid, process))
}

/// Split a bind address into an address and a port.
///
/// Handles `127.0.0.1:5173`, `*:53`, `0.0.0.0:5173`, `[::1]:8501`, `[::]:5173`
/// and the interface-scoped forms `*%lo:5173` and `[fe80::1%eth0]:22`.
///
/// The address is returned *unbracketed*, matching how [`Listener::bind`] is
/// documented: `[::1]:8501` yields `::1` and `[::]:5173` yields `::`.
fn split_addr_port(addr: &str) -> Option<(String, u16)> {
    // `a:b->c:d` is an established connection, not a listening socket. It
    // should already be filtered by `-sTCP:LISTEN`, but the parser is the last
    // place that can tell the difference.
    if addr.contains("->") {
        return None;
    }
    let (host, port) = match addr.strip_prefix('[') {
        Some(rest) => {
            let (host, after) = rest.split_once(']')?;
            (host, after.strip_prefix(':')?)
        }
        None => addr.rsplit_once(':')?,
    };
    let port: u16 = port.parse().ok()?;
    // Port 0 means "any port" on a bind that has not completed; there is
    // nothing on the far side to forward to.
    if port == 0 {
        return None;
    }
    let host = match host.split_once('%') {
        Some((address, _zone)) => address,
        None => host,
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

/// Keeps the first occurrence of each `(pid, bind, port)`.
///
/// One process listening on one socket appears once per open file descriptor in
/// `lsof`, so the same listener arrives several times. Order of first
/// appearance is preserved, because the picker shows these in the order the
/// host reported them.
#[derive(Default)]
struct Dedup {
    seen: HashSet<(u32, String, u16)>,
    listeners: Vec<Listener>,
}

impl Dedup {
    fn push(&mut self, listener: Listener) {
        if self
            .seen
            .insert((listener.pid, listener.bind.clone(), listener.port))
        {
            self.listeners.push(listener);
        }
    }

    fn into_vec(self) -> Vec<Listener> {
        self.listeners
    }
}

/// Resolve each listener to the pane that owns it.
///
/// A listener belongs to a pane when walking its process ancestry reaches that
/// pane's `shell_pid`. When the walk passes through more than one pane's shell
/// — a nested Herdr, or a pane whose shell spawned another pane's — the
/// *nearest* one wins, because that is the pane the user would point at.
///
/// Only panes from `panes` are ever reported, which is invariant 6. Listeners
/// that reach no pane come back with `pane: None`; per `ARCHITECTURE.md` those
/// are not offered by default.
pub fn attribute(
    listeners: &[Listener],
    tree: &ProcessTree,
    panes: &[PaneRef],
) -> Vec<AttributedListener> {
    let mut by_shell: HashMap<u32, &PaneRef> = HashMap::new();
    for pane in panes {
        // Pid 0 is the kernel, never a shell. Treating it as one would attach
        // every unowned listener to whichever pane reported it.
        if let Some(shell_pid) = pane.shell_pid.filter(|pid| *pid != UNKNOWN_PID) {
            // Two panes claiming one shell is contradictory input; keeping the
            // first makes the outcome depend only on the snapshot order.
            by_shell.entry(shell_pid).or_insert(pane);
        }
    }

    listeners
        .iter()
        .map(|listener| AttributedListener {
            listener: listener.clone(),
            pane: tree
                .ancestry(listener.pid)
                .into_iter()
                .find_map(|pid| by_shell.get(&pid).copied().cloned()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real macOS table from a live host, header included.
    const REAL_LSOF_TABLE: &str = "\
COMMAND    PID     USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME
cloudflar 1718 hiroyuki   11u  IPv4 0xa640a3ee9bdc0152      0t0  TCP 127.0.0.1:20241 (LISTEN)
python3.1 1761 hiroyuki    6u  IPv4 0x17835b1194e582d0      0t0  TCP 127.0.0.1:8501 (LISTEN)
limactl   2244 hiroyuki   13u  IPv4 0xdcaa7052c2f7ff29      0t0  TCP 127.0.0.1:49313 (LISTEN)
limactl   2245 hiroyuki    8u  IPv6 0xe2050ba8a4f16122      0t0  TCP *:53 (LISTEN)
ssh       3241 hiroyuki   11u  IPv4 0x1de08fd9762c5f7b      0t0  TCP *:5433 (LISTEN)
";

    fn pane(id: &str, shell_pid: Option<u32>) -> PaneRef {
        PaneRef {
            pane_id: id.into(),
            workspace_id: "w1".into(),
            workspace_label: None,
            agent: None,
            shell_pid,
        }
    }

    fn listener(pid: u32, port: u16) -> Listener {
        Listener {
            port,
            bind: "127.0.0.1".into(),
            pid,
            process: "node".into(),
        }
    }

    fn ports(listeners: &[Listener]) -> Vec<u16> {
        listeners.iter().map(|l| l.port).collect()
    }

    // ---- lsof, field format -------------------------------------------------

    #[test]
    fn lsof_fields_carry_the_pid_down_to_every_socket_of_one_process() {
        let out = parse_lsof("p605\ncrapportd\nf10\nn*:61848\nf19\nn*:52420\n");
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|l| l.pid == 605 && l.process == "rapportd"));
        assert_eq!(ports(&out), [61848, 52420]);
    }

    #[test]
    fn lsof_fields_keep_a_command_name_containing_spaces_intact() {
        let out = parse_lsof("p1296\ncCode Helper (Plugin)\nf72\nn127.0.0.1:44207\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].process, "Code Helper (Plugin)");
        assert_eq!(out[0].pid, 1296);
    }

    #[test]
    fn lsof_fields_report_ipv6_binds_without_brackets() {
        let out = parse_lsof("p10\ncnode\nf3\nn[::1]:8501\nf4\nn[::]:5173\n");
        assert_eq!(
            out.iter().map(|l| l.bind.as_str()).collect::<Vec<_>>(),
            ["::1", "::"]
        );
    }

    #[test]
    fn lsof_fields_collapse_one_socket_seen_on_several_descriptors() {
        let out = parse_lsof("p605\ncrapportd\nf10\nn*:61848\nf11\nn*:61848\n");
        assert_eq!(out.len(), 1, "the same pid/bind/port is one listener");
    }

    #[test]
    fn lsof_fields_keep_the_same_port_on_two_different_processes() {
        let out = parse_lsof("p1\nca\nn*:8080\np2\ncb\nn*:8080\n");
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].process, "b");
    }

    #[test]
    fn lsof_fields_drop_an_address_that_precedes_any_process_line() {
        assert!(parse_lsof("f3\nn127.0.0.1:8501\np1\ncnode\n").is_empty());
    }

    #[test]
    fn lsof_fields_drop_addresses_under_an_unparseable_pid() {
        // `p` opens a process set even when its value is junk, so the sockets
        // that follow must not be credited to the previous process.
        let out = parse_lsof("p1\ncnode\nn*:8080\npXYZ\ncghost\nn*:9090\n");
        assert_eq!(ports(&out), [8080]);
    }

    #[test]
    fn lsof_fields_ignore_a_connection_row_that_survived_the_state_filter() {
        assert!(parse_lsof("p1\ncnode\nn127.0.0.1:5173->127.0.0.1:52000\n").is_empty());
    }

    #[test]
    fn lsof_fields_keep_what_was_complete_when_the_stream_was_truncated() {
        let out = parse_lsof("p1\ncnode\nn*:8080\np2\ncvite\nn*:5");
        assert_eq!(ports(&out), [8080, 5], "a short port is still a port");
        let out = parse_lsof("p1\ncnode\nn*:8080\np2\ncvi");
        assert_eq!(ports(&out), [8080]);
    }

    #[test]
    fn lsof_fields_skip_unknown_field_tags_and_blank_lines() {
        let out = parse_lsof("p1\ncnode\nL hiroyuki\n\nR0\nn*:8080\n");
        assert_eq!(ports(&out), [8080]);
    }

    #[test]
    fn lsof_field_format_is_recognised_even_after_leading_noise() {
        let out = parse_lsof("lsof: WARNING: skipping /dev\np1\ncnode\nn*:8080\n");
        assert_eq!(ports(&out), [8080]);
    }

    // ---- lsof, human table --------------------------------------------------

    #[test]
    fn lsof_table_parses_the_real_macos_output() {
        let out = parse_lsof(REAL_LSOF_TABLE);
        assert_eq!(out.len(), 5, "header must not become a listener");
        assert_eq!(ports(&out), [20241, 8501, 49313, 53, 5433]);
        assert_eq!(
            out.iter().map(|l| l.pid).collect::<Vec<_>>(),
            [1718, 1761, 2244, 2245, 3241]
        );
        assert_eq!(out[0].process, "cloudflar");
        assert_eq!(out[0].bind, "127.0.0.1");
        assert_eq!(out[3].bind, "*");
    }

    #[test]
    fn lsof_table_recovers_a_command_name_containing_spaces() {
        let line = "Code Helper (Plugin) 1296 hiroyuki 72u IPv4 0x1de0 0t0 TCP 127.0.0.1:44207 \
                    (LISTEN)";
        let out = parse_lsof(line);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].process, "Code Helper (Plugin)");
        assert_eq!(out[0].pid, 1296);
        assert_eq!(out[0].port, 44207);
    }

    #[test]
    fn lsof_table_skips_rows_that_are_not_tcp_listen() {
        let text = "\
node 1 me 3u IPv4 0x1 0t0 TCP 127.0.0.1:5173 (ESTABLISHED)
node 2 me 4u IPv4 0x2 0t0 UDP *:5353 (LISTEN)
node 3 me 5u IPv4 0x3 0t0 TCP 127.0.0.1:5174 (LISTEN)
";
        assert_eq!(ports(&parse_lsof(text)), [5174]);
    }

    #[test]
    fn lsof_table_skips_rows_with_the_wrong_number_of_columns() {
        let text = "\
node 1 me 3u TCP 127.0.0.1:5173 (LISTEN)
node 2 me 4u IPv4 0x2 0t0 TCP 127.0.0.1:5174 (LISTEN)
";
        assert_eq!(
            ports(&parse_lsof(text)),
            [5174],
            "short row must be dropped"
        );
    }

    #[test]
    fn lsof_table_skips_a_row_whose_pid_column_is_not_a_number() {
        let text = "node ??? me 3u IPv4 0x1 0t0 TCP 127.0.0.1:5173 (LISTEN)\n";
        assert!(parse_lsof(text).is_empty());
    }

    #[test]
    fn empty_lsof_output_is_no_listeners_not_an_error() {
        assert!(parse_lsof("").is_empty());
        assert!(parse_lsof("\n\n   \n").is_empty());
    }

    // ---- port boundaries ----------------------------------------------------

    #[test]
    fn ports_at_and_past_the_u16_boundary() {
        assert_eq!(ports(&parse_lsof("p1\ncx\nn*:65535\n")), [65535]);
        assert!(parse_lsof("p1\ncx\nn*:65536\n").is_empty());
        assert!(parse_lsof("p1\ncx\nn*:0\n").is_empty());
        assert!(parse_lsof("p1\ncx\nn*:-1\n").is_empty());
        assert!(parse_lsof("p1\ncx\nn*:http\n").is_empty());
    }

    #[test]
    fn an_address_without_a_port_or_a_host_is_dropped() {
        assert!(parse_lsof("p1\ncx\nn*\n").is_empty());
        assert!(parse_lsof("p1\ncx\nn:5173\n").is_empty());
        assert!(parse_lsof("p1\ncx\nn[::1]5173\n").is_empty());
        assert!(parse_lsof("p1\ncx\nn\n").is_empty());
    }

    // ---- ss -----------------------------------------------------------------

    #[test]
    fn ss_parses_address_owner_and_port() {
        let text = "LISTEN 0      511          0.0.0.0:5173      0.0.0.0:*    \
                    users:((\"node\",pid=123,fd=20))\n";
        let out = parse_ss(text);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pid, 123);
        assert_eq!(out[0].process, "node");
        assert_eq!(out[0].bind, "0.0.0.0");
        assert_eq!(out[0].port, 5173);
    }

    #[test]
    fn ss_normalises_every_wildcard_spelling() {
        let text = "\
LISTEN 0 511 0.0.0.0:5173 0.0.0.0:* users:((\"a\",pid=1,fd=3))
LISTEN 0 511 *:5174 *:* users:((\"b\",pid=2,fd=3))
LISTEN 0 511 [::]:5175 [::]:* users:((\"c\",pid=3,fd=3))
LISTEN 0 511 [::1]:5176 [::]:* users:((\"d\",pid=4,fd=3))
";
        let out = parse_ss(text);
        assert_eq!(
            out.iter().map(|l| l.bind.as_str()).collect::<Vec<_>>(),
            ["0.0.0.0", "*", "::", "::1"]
        );
    }

    #[test]
    fn ss_strips_an_interface_zone_from_the_address() {
        let text = "\
LISTEN 0 511 *%lo:5173 *:* users:((\"a\",pid=1,fd=3))
LISTEN 0 128 [fe80::1%eth0]:22 [::]:* users:((\"sshd\",pid=2,fd=3))
";
        let out = parse_ss(text);
        assert_eq!(
            out.iter().map(|l| l.bind.as_str()).collect::<Vec<_>>(),
            ["*", "fe80::1"]
        );
    }

    #[test]
    fn ss_reports_a_listener_with_no_visible_owner_as_unowned() {
        let out = parse_ss("LISTEN 0 511 0.0.0.0:5173 0.0.0.0:*\n");
        assert_eq!(out.len(), 1, "an unowned socket is still listening");
        assert_eq!(out[0].pid, UNKNOWN_PID);
        assert_eq!(out[0].process, "");
    }

    #[test]
    fn ss_takes_the_first_process_of_a_shared_socket() {
        let text = "LISTEN 0 511 0.0.0.0:80 0.0.0.0:* \
                    users:((\"nginx\",pid=11,fd=6),(\"nginx\",pid=12,fd=6))\n";
        let out = parse_ss(text);
        assert_eq!(out[0].pid, 11);
    }

    #[test]
    fn ss_skips_rows_that_are_not_listening() {
        let text = "\
ESTAB 0 0 10.0.0.1:22 10.0.0.2:51234 users:((\"sshd\",pid=9,fd=3))
UNCONN 0 0 0.0.0.0:68 0.0.0.0:* users:((\"dhcp\",pid=8,fd=3))
LISTEN 0 511 0.0.0.0:5173 0.0.0.0:* users:((\"node\",pid=7,fd=3))
";
        assert_eq!(ports(&parse_ss(text)), [5173]);
    }

    #[test]
    fn ss_skips_a_header_row_and_short_rows() {
        let text = "\
State Recv-Q Send-Q Local Address:Port Peer Address:Port Process
LISTEN 0 511
LISTEN 0 511 0.0.0.0:5173 0.0.0.0:* users:((\"node\",pid=7,fd=3))
";
        assert_eq!(ports(&parse_ss(text)), [5173]);
    }

    #[test]
    fn ss_ignores_a_users_field_with_no_pid() {
        let out = parse_ss("LISTEN 0 511 0.0.0.0:5173 0.0.0.0:* users:((\"node\",fd=20))\n");
        assert_eq!(out[0].pid, UNKNOWN_PID);
    }

    #[test]
    fn ss_survives_a_truncated_users_field() {
        let out = parse_ss("LISTEN 0 511 0.0.0.0:5173 0.0.0.0:* users:((\"node\",pid=\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pid, UNKNOWN_PID);
    }

    #[test]
    fn ss_collapses_a_repeated_row() {
        let row = "LISTEN 0 511 0.0.0.0:5173 0.0.0.0:* users:((\"node\",pid=7,fd=3))";
        let out = parse_ss(&format!("{row}\n{row}\n"));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn empty_ss_output_is_no_listeners() {
        assert!(parse_ss("").is_empty());
    }

    // ---- ps -----------------------------------------------------------------

    #[test]
    fn ps_parses_two_indented_integer_columns() {
        let tree = parse_ps("    1     0\n  501     1\n61788 60416\n");
        assert_eq!(tree.len(), 3);
        assert_eq!(tree.parent(61788), Some(60416));
        assert_eq!(tree.parent(1), Some(0));
        assert_eq!(tree.parent(999), None);
    }

    #[test]
    fn ps_skips_a_header_and_any_non_numeric_row() {
        let tree = parse_ps("  PID  PPID\n    1     0\nnot a row\n");
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.parent(1), Some(0));
    }

    #[test]
    fn ps_skips_rows_with_an_unexpected_column_count() {
        let tree = parse_ps("1\n2 1 extra\n3 1\n");
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.parent(3), Some(1));
    }

    #[test]
    fn ps_keeps_the_first_entry_for_a_repeated_pid() {
        let tree = parse_ps("10 1\n10 2\n");
        assert_eq!(tree.parent(10), Some(1));
    }

    #[test]
    fn ps_tolerates_a_line_truncated_mid_write() {
        let tree = parse_ps("1 0\n2 1\n30");
        assert_eq!(tree.len(), 2, "the half-written last row is dropped");
    }

    #[test]
    fn empty_ps_output_is_an_empty_tree() {
        let tree = parse_ps("");
        assert!(tree.is_empty());
        assert_eq!(
            tree.ancestry(1),
            [1],
            "an unknown pid is still its own root"
        );
    }

    // ---- invariant 5: the ancestry walk terminates on any input --------------

    #[test]
    fn ancestry_terminates_on_a_two_node_cycle() {
        let tree = ProcessTree::from_pairs([(10, 20), (20, 10)]);
        assert_eq!(tree.ancestry(10), [10, 20]);
        assert_eq!(tree.ancestry(20), [20, 10]);
    }

    #[test]
    fn ancestry_terminates_on_a_self_parent() {
        let tree = ProcessTree::from_pairs([(10, 10)]);
        assert_eq!(tree.ancestry(10), [10]);
    }

    #[test]
    fn ancestry_terminates_on_a_long_cycle_reached_from_outside_it() {
        // A tail that walks into a loop: the visited set has to catch the entry
        // point, not just an immediate repeat.
        let mut pairs = vec![(1, 2), (2, 3), (3, 4), (4, 5), (5, 3)];
        pairs.push((0, 1));
        let tree = ProcessTree::from_pairs(pairs);
        assert_eq!(tree.ancestry(0), [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn ancestry_stops_when_a_parent_is_missing_from_the_snapshot() {
        let tree = ProcessTree::from_pairs([(100, 50)]);
        assert_eq!(tree.ancestry(100), [100, 50]);
    }

    #[test]
    fn ancestry_stops_at_the_roots_of_a_real_tree() {
        let tree = ProcessTree::from_pairs([(500, 1), (1, 0), (0, 0)]);
        assert_eq!(tree.ancestry(500), [500, 1, 0]);
    }

    #[test]
    fn ancestry_is_capped_on_a_chain_longer_than_the_cap() {
        let pairs: Vec<(u32, u32)> = (1..=(MAX_ANCESTRY_DEPTH as u32 + 10))
            .map(|pid| (pid, pid + 1))
            .collect();
        let tree = ProcessTree::from_pairs(pairs);
        let chain = tree.ancestry(1);
        assert_eq!(chain.len(), MAX_ANCESTRY_DEPTH + 1);
        assert_eq!(chain[chain.len() - 1], MAX_ANCESTRY_DEPTH as u32 + 1);
    }

    #[test]
    fn descends_from_includes_the_process_itself() {
        let tree = ProcessTree::from_pairs([(2, 1)]);
        assert!(tree.descends_from(2, 2));
        assert!(tree.descends_from(2, 1));
        assert!(!tree.descends_from(1, 2));
    }

    // ---- attribution --------------------------------------------------------

    #[test]
    fn a_listener_below_a_pane_shell_is_attributed_to_that_pane() {
        let tree = ProcessTree::from_pairs([(300, 200), (200, 100), (100, 1)]);
        let panes = [pane("w1:p1", Some(100))];
        let out = attribute(&[listener(300, 5173)], &tree, &panes);
        assert_eq!(
            out[0].pane.as_ref().map(|p| p.pane_id.as_str()),
            Some("w1:p1")
        );
    }

    #[test]
    fn a_shell_listening_directly_is_attributed_to_its_own_pane() {
        let tree = ProcessTree::from_pairs([(100, 1)]);
        let panes = [pane("w1:p1", Some(100))];
        let out = attribute(&[listener(100, 5173)], &tree, &panes);
        assert!(out[0].pane.is_some(), "distance zero still counts");
    }

    #[test]
    fn nested_pane_shells_attribute_to_the_nearest_one() {
        // 500 -> 400 (near pane) -> 300 (far pane). The far pane is listed
        // first, so anything that picks by snapshot order fails here.
        let tree = ProcessTree::from_pairs([(500, 400), (400, 300), (300, 1)]);
        let far = pane("w1:outer", Some(300));
        let near = pane("w1:inner", Some(400));

        let out = attribute(&[listener(500, 5173)], &tree, &[far.clone(), near.clone()]);
        assert_eq!(
            out[0].pane.as_ref().map(|p| p.pane_id.as_str()),
            Some("w1:inner"),
            "the nearest shell owns the listener"
        );

        // Same answer with the slice in the other order.
        let out = attribute(&[listener(500, 5173)], &tree, &[near, far]);
        assert_eq!(
            out[0].pane.as_ref().map(|p| p.pane_id.as_str()),
            Some("w1:inner")
        );
    }

    #[test]
    fn a_listener_whose_pid_is_in_no_snapshot_is_attributed_to_nothing() {
        let tree = ProcessTree::from_pairs([(100, 1)]);
        let panes = [pane("w1:p1", Some(100))];
        let out = attribute(&[listener(9999, 5173)], &tree, &panes);
        assert_eq!(out[0].pane, None);
    }

    #[test]
    fn an_unowned_listener_is_never_attributed() {
        let tree = ProcessTree::from_pairs([(0, 0)]);
        let panes = [pane("w1:p1", Some(UNKNOWN_PID))];
        let out = attribute(&[listener(UNKNOWN_PID, 5173)], &tree, &panes);
        assert_eq!(out[0].pane, None, "pid 0 is the kernel, not a shell");
    }

    #[test]
    fn a_pane_with_no_shell_pid_never_claims_a_listener() {
        let tree = ProcessTree::from_pairs([(300, 200), (200, 100)]);
        let panes = [pane("w1:p1", None)];
        let out = attribute(&[listener(300, 5173)], &tree, &panes);
        assert_eq!(out[0].pane, None);
    }

    #[test]
    fn attribution_only_reports_panes_from_the_given_snapshot() {
        // Invariant 6: the pane that used to own this pid is gone from the
        // snapshot, so the listener must come back unattributed.
        let tree = ProcessTree::from_pairs([(300, 100), (100, 1)]);
        let out = attribute(&[listener(300, 5173)], &tree, &[]);
        assert_eq!(out[0].pane, None);
    }

    #[test]
    fn two_panes_claiming_one_shell_resolve_deterministically() {
        let tree = ProcessTree::from_pairs([(300, 100)]);
        let panes = [pane("w1:first", Some(100)), pane("w1:second", Some(100))];
        let out = attribute(&[listener(300, 5173)], &tree, &panes);
        assert_eq!(
            out[0].pane.as_ref().map(|p| p.pane_id.as_str()),
            Some("w1:first")
        );
    }

    #[test]
    fn attribution_terminates_when_the_ancestry_loops_below_the_pane() {
        let tree = ProcessTree::from_pairs([(300, 200), (200, 300)]);
        let panes = [pane("w1:p1", Some(100))];
        let out = attribute(&[listener(300, 5173)], &tree, &panes);
        assert_eq!(
            out[0].pane, None,
            "the loop must end the walk, not the test"
        );
    }

    #[test]
    fn a_pane_shell_at_and_past_the_depth_cap() {
        let pairs: Vec<(u32, u32)> = (1..=(MAX_ANCESTRY_DEPTH as u32 + 10))
            .map(|pid| (pid, pid + 1))
            .collect();
        let tree = ProcessTree::from_pairs(pairs);

        let at_cap = [pane("w1:at", Some(MAX_ANCESTRY_DEPTH as u32 + 1))];
        assert!(attribute(&[listener(1, 5173)], &tree, &at_cap)[0]
            .pane
            .is_some());

        let past_cap = [pane("w1:past", Some(MAX_ANCESTRY_DEPTH as u32 + 2))];
        assert_eq!(
            attribute(&[listener(1, 5173)], &tree, &past_cap)[0].pane,
            None
        );
    }

    #[test]
    fn attribution_preserves_the_listener_order_and_count() {
        let tree = ProcessTree::from_pairs([(10, 1), (20, 1)]);
        let listeners = [listener(10, 5173), listener(20, 8080), listener(30, 3000)];
        let out = attribute(&listeners, &tree, &[]);
        assert_eq!(
            out.iter().map(|a| a.listener.port).collect::<Vec<_>>(),
            [5173, 8080, 3000]
        );
    }

    /// The behaviour the whole feature rests on: a real host's listener list is
    /// infrastructure, and none of it descends from a Herdr pane.
    #[test]
    fn the_real_macos_listeners_belong_to_no_pane() {
        let ps = "\
    1     0
 1718     1
 1761     1
 2244     1
 2245     1
 3241  2244
60416     1
61788 60416
";
        let tree = parse_ps(ps);
        // shell_pid and the agent pid from a live `pane process-info`.
        let panes = [pane("w1:p1", Some(60416))];
        let listeners = parse_lsof(REAL_LSOF_TABLE);
        assert_eq!(listeners.len(), 5);

        let out = attribute(&listeners, &tree, &panes);
        assert!(
            out.iter().all(|a| a.pane.is_none()),
            "infrastructure ports must never reach an offer: {:?}",
            out.iter()
                .filter(|a| a.pane.is_some())
                .map(|a| a.listener.port)
                .collect::<Vec<_>>()
        );

        // And the same snapshot with one dev server under the pane's shell does
        // attribute, so the test above is not passing for want of any match.
        let mut listeners = listeners;
        listeners.push(listener(61788, 5173));
        let out = attribute(&listeners, &tree, &panes);
        assert_eq!(
            out.iter().filter(|a| a.pane.is_some()).count(),
            1,
            "the agent's own port is the only one that belongs to the pane"
        );
        assert_eq!(out[5].listener.port, 5173);
    }

    #[test]
    fn the_remote_commands_interpolate_nothing() {
        // Invariant 9: these reach a remote shell verbatim, so they must stay
        // constants with no user-controlled input in them.
        for command in [
            LSOF_LISTENERS_COMMAND,
            SS_LISTENERS_COMMAND,
            PS_TREE_COMMAND,
        ] {
            assert!(!command.contains('{'));
            assert!(!command.contains('$'));
            assert!(!command.contains('`'));
        }
    }
}
