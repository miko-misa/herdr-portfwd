# Architecture

This document is the implementation contract for `herdr-portfwd`. It records
*why* the design is shaped this way, not just what it does, because most of the
constraints come from facts about Herdr that are not obvious from the outside.

## The problem

A coding agent running on a remote host starts a dev server. It prints
`http://localhost:5173`. That URL is meaningless on the machine you are sitting
at. You want the page in your local browser, and you want to stop caring about
`ssh -L` incantations.

## Facts about Herdr this design depends on

These were verified against Herdr 0.8.0 source and a live session. They are
load-bearing; if they change, this design changes.

1. **Plugin commands run where the Herdr *server* runs.** Herdr injects
   context and spawns the manifest command on the server host. A plugin cannot
   assume it is running on the machine with the user's browser.

2. **`herdr --remote <host>` puts the server on the remote.** The local process
   is a thin client (`run_client_process` in `src/remote/attach.rs`) bridged
   over SSH to a remote server. Therefore, under `--remote`, plugin actions,
   `[[events]]` hooks, `[[startup]]` hooks and the TUI's own `open_url` all
   execute **on the remote host**. Clicking a URL there tries to open a browser
   on the remote machine.

3. **`herdr-mirror` puts the server on the local machine.** Mirrored panes are
   local panes streaming a remote terminal. A plugin invoked from a mirror pane
   runs locally and has full access to the user's machine.

4. **`[[keys.command]]` bindings are not sent to a `--remote` server.** The
   client ships a keybinding profile that strips every custom command binding,
   including `type = "plugin_action"`. Under `--remote`, plugin keybindings must
   be configured in the *remote* `config.toml`.

5. **`pane.process_info` returns `shell_pid`** plus the foreground process list.
   This is what makes port attribution possible.

6. **Manifest `[[events]]` cannot hook terminal output.** `pane.output_changed`
   is deliberately excluded from `PLUGIN_HOOK_EVENT_KINDS`. Detecting a dev
   server from its printed banner requires either polling or a long-lived
   subscriber, not an event hook.

7. **Link handlers fire on Ctrl+click**, on every platform including macOS,
   because captured terminal mouse reports cannot distinguish Cmd+click from a
   plain click.

## Consequence: request and apply must be separable

Opening a listening socket on the user's machine can only be done by a process
*on* that machine. Under `--remote` the plugin is not on that machine. So the
tool splits into two halves that talk through a serialized request:

```
                    ┌──────────── mirror mode ─────────────┐
  plugin (local) ──> requests.json (local) ──> apply (local) ──> ssh -L
                    └──────────────────────────────────────┘

                    ┌──────── herdr --remote mode ─────────┐
  plugin (remote) ─> requests.json (remote)
                                │  polled over the local agent's own SSH master
                     local-agent (local) ─────────────────> ssh -L
                    └──────────────────────────────────────┘
```

The same request record drives both paths. Only the transport differs, and only
`local_agent` knows about the difference.

### Why the local agent owns its own SSH ControlMaster

Under `--remote`, Herdr itself holds a master at
`$TMPDIR/herdr-ssh-<pid>-<n>/ctl`. We deliberately do **not** use it:

- it is an internal implementation detail with no stability guarantee,
- `RemoteSsh::drop` sends `-O exit` when the attach ends,
- `[remote].manage_ssh_config = false` disables it entirely.

`herdr-portfwd attach <host>` starts its own master, then execs
`herdr --remote <host>`. One extra SSH connection is cheap and the lifetime is
ours to control.

**ControlPath must stay under 104 bytes** (`sun_path` limit). Sockets live at
`$XDG_STATE_HOME/herdr-portfwd/<host>.ctl`, never under a plugin state dir,
whose path can be arbitrarily long.

## Port attribution: which listener belongs to whom

Blindly forwarding every remote listener is wrong. A real host's listener list
is mostly infrastructure — a tunnel daemon, a VM manager, an SSH mux socket —
and none of it belongs to an agent's dev server.

Attribution resolves this without guessing from port numbers:

1. `herdr pane list` → every pane on that host.
2. `herdr pane process-info --pane <id>` → each pane's `shell_pid`.
3. `lsof -nP -iTCP -sTCP:LISTEN` (macOS) or `ss -ltnpH` (Linux) → listeners with
   owning PIDs.
4. `ps -eo pid=,ppid=` → parent map; walk each listener's ancestry.
5. A listener whose ancestry reaches a pane's `shell_pid` belongs to that pane,
   and therefore to a known workspace, tab, and agent.

Listeners that reach no pane are **not** offered by default. Ancestry walks must
be cycle-guarded and depth-capped: PID reuse can produce a loop.

## Automation levels

| Level | Trigger | Who resolves ambiguity | Default |
| --- | --- | --- | --- |
| L1 explicit | Ctrl+click on a URL, or `add <port>` | the user pointed at it | on |
| L2 offer | new attributed listener → Herdr notification → one key to accept | the user approves | on |
| L3 auto | matches a user-declared allowlist for that workspace | the user configured it | off |

Silent forwarding is never the default. L2's candidate set is already filtered
by attribution, so infrastructure ports never reach a notification.

## Port numbering: same number, or fail loudly

The local port defaults to the same number as the remote port, because the
printed URL is the whole point. Remapping silently breaks things that embed the
port: Vite HMR, OAuth `redirect_uri`, and any absolute URL the app emits.

On a local conflict the tool **reports the conflict** and offers an explicit
remap, annotated as potentially breaking. It never remaps on its own.

## Modules

Leaf modules have no dependencies on each other and are independently testable.

| Module | Responsibility | Key seam |
| --- | --- | --- |
| `cli` | argument parsing, subcommand dispatch | pure; parse-only tests |
| `config` | `hosts.toml` (herdr-mirror compatible) + own settings | pure parse from `&str` |
| `context` | `HERDR_PLUGIN_CONTEXT_JSON` → host/workspace/pane resolution | pure; env read at edge |
| `herdr` | Herdr CLI wrapper via `HERDR_BIN_PATH` | `HerdrClient` trait |
| `ssh` | ControlMaster lifecycle, `-O forward/cancel/check` | `SshRunner` trait |
| `discover` | `lsof`/`ss`/`ps` parsing, ancestry attribution | pure functions over fixtures |
| `state` | `forwards.json`, advisory locking, atomic replace | filesystem, `tempfile` in tests |
| `request` | `requests.json`, the mode-bridging record | pure serde + file edge |
| `actions` | add / stop / stop-all / scan / open-url / list | composed over traits |
| `local_agent` | `--remote` polling loop, offer/apply | composed over traits |
| `ui` | manager pane rendering and key handling | pure render from a snapshot |

**Every module that shells out does so behind a trait.** `SshRunner` and
`HerdrClient` have real implementations and test fakes. No test may invoke real
`ssh`, real `herdr`, or touch a real network.

## Invariants

These are the properties tests must defend.

1. A forward is never created for a local port already in use, unless the user
   explicitly asked for that remap.
2. `state` never loses a record on concurrent writes: lock, read, mutate,
   atomically replace.
3. Every forward in `forwards.json` is either live on its master or reconciled
   away on the next pass; a stale record never shadows a real port.
4. Cancelling a forward that was never created is a no-op, not an error.
5. Ancestry walking terminates on any input, including a PID graph with cycles.
6. Attribution never reports a pane that does not exist in the current snapshot.
7. Nothing is forwarded without an L1 action, an accepted L2 offer, or an L3
   allowlist match.
8. Paths that become Unix socket addresses are length-checked before use.
9. No user-controlled string reaches a shell. Commands are argv arrays.

## Non-goals for v1

- Windows hosts (Herdr's own `--remote` does not support them either).
- Reverse forwards (local → remote).
- Forwarding to anything but loopback on the remote side.
- Any transport other than SSH ControlMaster multiplexing.
