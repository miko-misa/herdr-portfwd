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

4. **A default `--remote` attach runs with no custom command bindings on either
   side.** `herdr --remote` defaults to `--remote-keybindings local`
   (`extract_remote_args` in `src/remote/attach.rs`), so the client sends its
   *local* profile (`requested_keybindings` → `ClientKeybindings::Local`) built
   by `local_keybindings_profile_toml`, which omits every `[[keys.command]]`;
   the server clears the list again on arrival (`parse_client_keybindings`) and
   that profile *replaces* the server's own for the client, so the remote
   `config.toml`'s bindings are not in effect either. `--remote-keybindings
   server` is the single switch that changes this: the server keeps its own
   profile, and plugin-action keys in the *remote* `config.toml` fire.

   Every action must therefore be reachable without a keybinding.
   `[[link_handlers]]` are resolved from the server's plugin registry on a
   modified click, not from a keybinding profile, and
   `herdr plugin action invoke <id>` runs one by name. Those two are the only
   entry points that work in every configuration.

5. **`pane.process_info` returns `shell_pid`** plus the foreground process list.
   This is what makes port attribution possible.

6. **Manifest `[[events]]` cannot hook terminal output.** `pane.output_changed`
   is deliberately excluded from `PLUGIN_HOOK_EVENT_KINDS`. Detecting a dev
   server from its printed banner requires either polling or a long-lived
   subscriber, not an event hook.

7. **Link handlers fire on Ctrl+click**, on every platform including macOS,
   because captured terminal mouse reports cannot distinguish Cmd+click from a
   plain click.

8. **A plugin can ask Herdr to open one of its own declared panes**:
   `herdr plugin pane open --plugin <id> --entrypoint <id>`. This is the only
   way an action reaches a user, because an action command runs headless: its
   stdout and stderr are pipes into the plugin log, there is no pane in front
   of it, and its stdin may never reach EOF. Note that a pane's command and an
   action's command may be the same argv — `pane-ui list` is both the `list`
   pane and the "Port: manage forwards" action — so the argv cannot be what
   decides which one is running.

   The call also takes `--placement`, overriding the manifest's, and repeated
   `--env KEY=VALUE` words that become the pane process's environment. Herdr
   protects its own variables (`HERDR_PLUGIN_ID`, `HERDR_PLUGIN_CONTEXT_JSON`,
   `HERDR_BIN_PATH` and friends) against being set this way; anything else gets
   through, with only NUL bytes and an empty key rejected. **A pane command
   receives no arguments beyond the argv its manifest entry declares**, so this
   is one of only two ways to tell a pane anything: see "A pane that has to be
   told something" below.

9. **Which kind of command is running is only readable from the injected
   environment.** `HERDR_SOCKET_PATH`, `HERDR_BIN_PATH` and `HERDR_PLUGIN_ID`
   arrive for every plugin command; `HERDR_PLUGIN_ACTION_ID` marks an action,
   `HERDR_PLUGIN_ENTRYPOINT_ID` a pane, `HERDR_PLUGIN_EVENT` a startup or event
   hook.

   `is_terminal()` answers a different question, and it is **not** reliably
   false: `start_plugin_command` sets only stdout and stderr to `Stdio::piped()`
   and leaves stdin inherited from the server process. The auto-spawned daemon
   nulls all three, but a server the user started in the foreground —
   `herdr server`, or `herdr --no-session` — hands its own tty to every plugin
   command it spawns. A rule that consults the terminal before the injected
   variables therefore draws a pane into the plugin log and reads keys off a
   terminal Herdr is already reading.

10. **There is exactly one popup slot, and a popup pane cannot close itself.**
    `spawn_popup_command` refuses a second popup with `popup already open`
    (surfacing as `plugin_pane_open_failed`), and Herdr closes a popup only when
    its process dies — `AppEvent::PaneDied`. A popup pane command is also
    launched with `PaneLaunchEnv::without_pane_identity`, so it has no
    `HERDR_PANE_ID` to name to `herdr pane close`, and `popup.close` has no CLI.

    `ui_busy` is a **different** refusal, and the plugin docs' mention of it for
    this case is misleading: it fires when `state.mode != Mode::Terminal` —
    Settings, Copy mode, a context menu — and opening a plugin popup sets the
    mode back to `Terminal`. So a popup asking for a popup is not blocked by a
    modal; it is blocked by the slot it is itself holding.

    An overlay pane is not a popup and is not blocked by either: it is an
    ordinary split pane, so Herdr opens it *behind* a live popup and reveals it
    when that popup's process exits.

## A pane that has to be told something

`confirm` is the screen a Ctrl+click lands on. Unlike `list` and `picker` it
cannot rebuild its own contents: it is about *one* click, and by the time it
runs, the thing that knew which click has exited. Fact 8 leaves two ways to tell
it — `--env`, or a file both sides agree on — and it uses the environment.

One variable, `HERDR_PORTFWD_CONFIRM`, carrying one line of JSON: the validated
URL, the host, both ports, whether the local port is free, whether the forward
already exists, which side will apply it, the owning workspace and agent, and
the pane the URL was printed in. `HERDR_PORTFWD_HOST` rides along so the pane
resolves the host this click already resolved rather than re-deriving it from a
context that has moved on.

Why not a file: it would need a name, an owner and a deletion, and each of those
is a way for the next click to render the previous click's URL; two clicks in
quick succession are two environments but one path written twice; and the state
directory rules below exist precisely because "somewhere the pane can find
without being told" has more than one wrong answer. Nothing in the payload is a
secret — the URL is already in Herdr's own `HERDR_PLUGIN_CLICKED_URL`.

The pane trusts none of it. Under `--remote` the URL was written on the host,
put in a variable on the host, and handed back, so `open_url_for` re-validates
it with the same parser `open-url` used and forwards from the parsed parts.
Invariant 11, one hop further out.

### One pane reaching another

`confirm` offers `l` (the `list` overlay) and `s` (the `picker`); `list` offers
`a` (the `picker`). Fact 10 forces the ordering: a pane cannot exit and *then*
ask, because a process that has exited asks nothing, and it cannot close itself
first. So it asks while its own pane is still up, and the UI returns the request
rather than making it — the pane loop ends, the caller asks Herdr, and only then
does the process exit and take the pane with it.

That works directly for every pair except popup → popup, which asks for the one
slot the caller is holding. There the request carries `--placement overlay`:
Herdr opens it behind the popup and reveals it the moment the popup goes. This
is the only reason `--placement` is ever passed, since overriding it elsewhere
would give one pane two shapes depending on who opened it.

This is not the recursion invariant 10 forbids. That rule is about a command
that *was not told enough* asking for a pane whose command would ask again; this
is a key the user pressed, and the graph — `confirm` → `list` | `picker`,
`list` → `picker` — has no cycle.

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
                                │  writes back how far it got, over the same
                                v  master
                     requests.acked (remote)
                                │  folded into every read of the queue, so the
                                v  plugin's own view agrees
                     list / status (remote)
                    └──────────────────────────────────────┘
```

The same request record drives both paths. Only the transport differs, and only
`local_agent` knows about the difference.

### The browser is on the applier's side too

A Ctrl+click asks for two things: the forward, and the page. Under `--remote`
the plugin that handles the click can do neither, so the request carries an
optional URL for whoever applies it, and the agent opens it once the forward is
live. That optionality is also what makes the `confirm` pane's "forward only"
key mean anything on the far side: the pane records the same forward and leaves
the URL out, because carrying it and hoping the agent ignores it would open the
window the user pressed a key specifically to avoid. Everything else is unchanged, which is why the field is optional in both
directions: an agent older than the field applies the forward exactly as before
and opens nothing, and a newer agent reading an older writer's record simply has
no URL to open.

The URL is a hint, never an instruction. It was written on the remote, into a
file on the remote, and read back over `ssh`, so the agent re-validates it as
loopback with the same parser the plugin used, requires it to name the local
port that pass actually bound, re-renders it from the validated parts, and hands
it to the desktop opener as one argv word. That is invariant 11 at the other end
of the click: a remote decides *that* a forward is wanted, never where it goes —
and never what this machine's browser visits. A request the agent did not apply
opens nothing at all, because a URL over a port nothing is listening on is an
error page the user did not ask for.

### Settling a request

A request is *settled* once the applier knows what became of it: applied,
skipped as somebody else's, or expired. Only the applier can know that, and
under `--remote` the applier is not the process that wrote the request. So the
mark saying how far it has got travels back to the writer, in a second file
beside the queue.

- **`requests.json`** is written under this crate's lock, only by processes that
  link this crate. It carries the queue and its own high-water mark.
- **`requests.acked`** is one decimal integer, replaced by a rename. The local
  agent reaches a remote state directory only as a shell command over an SSH
  master, where it cannot take that lock — but a single integer under a single
  writer can be replaced safely from a shell, and a truncated read of one is
  impossible to misread. The request file itself is never rewritten from over
  there: a lost enqueue is a Ctrl+click that silently did nothing.

Every read of the queue folds the mark in. That is what keeps `list` and
`status` — which run wherever the *plugin* runs, so under `--remote` on the far
side from the agent — from reporting an applied request as still waiting, and
what keeps a restarted agent from re-opening a port the user has since stopped.

The mark only ever advances, and only over an unbroken run of settled requests.
A request the agent could not apply — the local port is held by a different
forward, the master refused it — stays pending and is retried, until its TTL
makes it expired, which *is* a settlement. Marking a request the agent merely
tried to apply is the one move that loses a forward with nothing said.

### Why the local agent owns its own SSH ControlMaster

Under `--remote`, Herdr itself holds a master at
`$TMPDIR/herdr-ssh-<pid>-<n>/ctl`. We deliberately do **not** use it:

- it is an internal implementation detail with no stability guarantee,
- `RemoteSsh::drop` sends `-O exit` when the attach ends,
- `[remote].manage_ssh_config = false` disables it entirely.

`herdr-portfwd attach <host>` starts its own master, spawns the local agent in
the background, then runs `herdr --remote <host>` on the terminal it inherited
and waits for it. Not `execve`, despite the shorthand: a replaced process cannot
stop the agent, cancel the forwards, or take the master down afterwards, and
every exit path — return, error, signal, panic — has to go through that one
teardown. One extra SSH connection is cheap and the lifetime is ours to control.

**ControlPath must stay under 104 bytes** (`sun_path` limit). Sockets live at
`$XDG_STATE_HOME/herdr-portfwd/<host>.ctl` (falling back to
`~/.local/state/herdr-portfwd`), never under a plugin state dir, whose path can
be arbitrarily long.

The durable records live in that same directory, chosen by that same rule, and
`HERDR_PLUGIN_STATE_DIR` is deliberately not consulted for them either. Herdr
injects it for plugin commands only, so obeying it split the records along a
line the user cannot see: a forward made by a keybinding went into Herdr's
per-plugin directory and the `herdr-portfwd status` run in a shell a second
later read the XDG one and reported nothing, while the ports were open. The
socket is shared between the two halves whatever the records say, which is what
made that split a lie rather than a separation. The agent still recognises the
older locations on the *remote* host, where the plugin it is polling may be
older than it is.

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
| L1 explicit | Ctrl+click on a URL → the `confirm` pane, which forwards and optionally opens; or `add <port>` | the user pointed at it, then confirmed it | on |
| L2 offer | new attributed listener → Herdr notification naming the port, the process, the pane, and the `add` that would forward it | the user runs it | on |
| L3 auto | matches a user-declared allowlist for that workspace | the user configured it | off |

Silent forwarding is never the default. L2's candidate set is already filtered
by attribution, so infrastructure ports never reach a notification.

**A click is an L1 trigger, not an L1 action.** It opens the `confirm` pane and
nothing else happens until a key is pressed there, because a click that both
opened a port and launched a browser gave the user no place to see what it was
about to do — including the three things that screen now says out loud: the port
is already forwarded, the local port is held by something else, or the URL
belongs to a host this invocation cannot resolve. `confirm_on_click = false`
restores the one-motion behaviour for people who preferred it. Either way the
forward itself is the same L1 action; what changed is who gets asked first.

**There is no accept key, and L2 cannot have one.** Herdr's notification API is
`herdr notification show <title> --body <body>` — a toast, with no callback, no
choices, and no way to bind a key to the offer that raised it. An offer is
therefore a sentence, and accepting one is an L1 action the user performs, which
is why the notification carries the exact command rather than a hint. Each port
is offered once — a toast per poll would be worse than silence — and again only
once its listener has gone away and come back. `offer_new_listeners = false`
silences the toasts without disabling discovery. Invariant 7's "accepted L2
offer" is therefore the same event as an L1 `add`; what L2 contributes is that
the user was told which port to type.

An L1 action over a port L2 or L3 already opened is not a no-op: it **pins** the
forward. The record becomes explicit, so the `reconcile` that follows the owning
pane exiting keeps a port the user asked to keep. Never the reverse — a
detection over a pinned forward leaves the pin alone.

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
| `confirm` | the click payload the `confirm` pane is opened with | pure serde; env read at edge |
| `context` | `HERDR_PLUGIN_CONTEXT_JSON` → host/workspace/pane resolution | pure; env read at edge |
| `herdr` | Herdr CLI wrapper via `HERDR_BIN_PATH` | `HerdrClient` trait |
| `ssh` | ControlMaster lifecycle, `-O forward/cancel/check` | `SshRunner` trait |
| `discover` | `lsof`/`ss`/`ps` parsing, ancestry attribution | pure functions over fixtures |
| `state` | `forwards.json`, advisory locking, atomic replace | filesystem, `tempfile` in tests |
| `request` | `requests.json` + `requests.acked`, the mode-bridging record | pure serde + file edge |
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
   atomically replace. This binds callers too, not only `state`: a pass that
   reads, decides across seconds of SSH, and then writes — `reconcile` — merges
   its decision into whatever the file holds under the final lock. It may
   overrule only records still identical to the ones it read, so an `add` that
   landed mid-pass keeps its record and a `stop` that landed mid-pass is not
   undone.
3. Every forward in `forwards.json` is either live on its master or reconciled
   away on the next pass; a stale record never shadows a real port.
4. Cancelling a forward that was never created is a no-op, not an error.
5. Ancestry walking terminates on any input, including a PID graph with cycles.
6. Attribution never reports a pane that does not exist in the current snapshot.
7. Nothing is forwarded without an L1 action, an accepted L2 offer, or an L3
   allowlist match.
8. Paths that become Unix socket addresses are length-checked before use.
9. No user-controlled string reaches a shell. Commands started on this machine
   are argv arrays. `ssh` hands a remote command to the remote login shell, so
   over there the rule is kept from the other side instead: the snippets that
   locate the request file and move the applier's mark are fixed text whose only
   interpolation is a `u64`, and a `herdr` argv is quoted word by word so it
   survives exactly one round of shell parsing.
10. A command that was not told enough goes to the one place that can ask: a
    plugin action asks Herdr to open the pane (fact 8), a terminal draws it
    itself, and anything else refuses with what was missing named. The injected
    variables of fact 9 are consulted **first** and decide on their own — a
    pane command never asks, or the pane it opens would ask again; an action
    always asks, whatever it inherited on stdin. `is_terminal()` is only the
    tie-breaker for what is left, and it has to be true of stdin *and* stdout,
    since a pane reads keys from one and paints frames to the other. This binds
    a command that names a pane too: `pane-ui list` run as an action asks rather
    than drawing, because the argv alone does not say which kind is running.
    `scan` is bound by the same rule for a different reason: it is never missing
    an argument, but its answer is a list to pick from, and an action's stdout is
    a plugin log rather than a person, so as an action it asks for the picker.
    `--json`, and a shell invocation with no Herdr to ask, still get the printed
    report — `scan` alone has a complete answer without asking anybody.
    `open-url` follows `scan`'s shape exactly: as an action it asks for the
    `confirm` pane, at a terminal it draws it, and only an invocation with
    nowhere to draw *and* no Herdr to ask — `--json`, or a shell whose output is
    redirected — acts on the click instead, because a screen nobody can see is
    worse than the forward the user asked for. A shell invocation at a terminal
    is not that case: it draws the screen.

    What this rule does **not** forbid is a pane opening another pane on a key
    the user pressed: see "One pane reaching another". The recursion it guards
    against is a command that was not told enough asking for a pane whose
    command asks again, and a cross-link is neither missing anything nor
    cyclic.
11. Both ends of a forward are chosen on the machine that opens it: the
    listening end is bound to loopback explicitly rather than left to
    `GatewayPorts`, and a request's `remote_bind` is clamped to loopback rather
    than obeyed. A remote decides *that* a forward is wanted, never *where* it
    goes.
12. A request is settled — applied, skipped, or expired — exactly once, and only
    by the applier, which is the only party that knows what became of it. A
    settled request is never applied again: not by a re-read, not by being
    written back, not by a restarted agent. One the applier could not apply is
    not settled; it stays pending and is retried until its TTL expires it.
13. A record answers a request only when it describes the same forward, not
    merely when it sits on the same local port. Two forwards can want one local
    port, and invariant 1 governs that collision wherever it is met: `add` on
    this machine and the local agent applying a request must both report it
    rather than treat the other forward as the one that was asked for.

## Non-goals for v1

- Windows hosts (Herdr's own `--remote` does not support them either).
- Reverse forwards (local → remote).
- Forwarding to anything but loopback on the remote side. The apply side
  enforces this rather than trusting the request record, which is written on the
  remote: see invariant 11.
- Any transport other than SSH ControlMaster multiplexing.
