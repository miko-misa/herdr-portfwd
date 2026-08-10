# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.1] - 2026-08-10

### Fixed

- `list` and the manager pane showed only the host the invocation resolved to,
  so a forward on any other host was invisible — and unstoppable, because `stop`
  narrowed the same way and answered *"nothing to stop on <that one host>"*
  while the port stayed up. Both now cover every host. Two machines forwarding a
  port each was enough to meet this, and the pane's own `HOST` column had been
  there for the case all along.

  The pane's header said `Port forwards (1 live)` either way, so nothing on
  screen told "there is one" from "you are being shown one". It now reads
  `(3 live, all hosts)`, or `(1 of 3 live, host buildbox only)` when a filter is
  on.

- `--host`, and the `HERDR_PORTFWD_HOST` it reads, narrow the manager pane's
  *filter* rather than the listing behind it. The pane built its rows from a
  narrowed listing while its own filter — the thing the header describes — was
  still "all hosts", so `HERDR_PORTFWD_HOST=mini herdr-portfwd pane-ui list`
  drew one machine's row under `(1 live, all hosts)` and `h` had no other host
  left to reveal. The pane holds every row now and opens on the filter the flag
  asked for, which makes the flag and the `h` key the same mechanism: the header
  is right at every step of the cycle, and `X` still stops exactly what is
  drawn.

- `enter` on a row stops that row's forward, on that row's host. It matches the
  record by host and both ports, so a row whose record was rewritten between the
  frame being drawn and the key being pressed stops nothing rather than
  cancelling whatever took its place.

- `stop <port>` searches every host. A port on two hosts is refused with both
  named rather than guessed at, and a `stop --host <one>` that finds nothing now
  says which host does have that port instead of reporting an empty result.

- The manager and picker panes no longer render a listing they could not get as
  an empty world. `HERDR_PORTFWD_HOST=stale-name herdr-portfwd pane-ui list`
  drew `(0 live, all hosts)` over *"No forwards are live on any host."* while
  `herdr-portfwd list` in the same moment found two forwards on two machines:
  the host name resolved to nothing, the listing failed, and the pane fell back
  to an empty table it then described as the world. It now says *"Could not list
  the forwards."* under a header claiming no count and no scope, with the reason
  — `unknown host "stale-name"; configured: ...` — on the status line and `r` to
  try again. `list --host gone-host` has always refused rather than printed a
  world; the pane refuses to speak for hosts it never reached.

- `herdr-portfwd stop-all --help` and `--host`'s own help describe what they now
  do. `stop-all` still read *"Stop every forward for the selected host"* after
  it had started stopping every host's, and `--host` still read *"Host to act
  on"* after it had become a narrowing — the destructive command describing a
  blast radius smaller than the one it has. The manifest description, both
  READMEs and the clap doc comment are now checked against each other and
  against a measured `stop-all`, so no one of them can be corrected on its own
  again.

- A record file that could not be read is reported instead of being read as a
  world with nothing in it. `chmod 000 forwards.json` with a forward live on
  each of two hosts left every manager frame saying `(0 live, all hosts)` over
  *"No forwards are live on any host."* — a positive claim about every machine,
  from a process that had read nothing — and the same went for a file truncated
  mid-write and for a record carrying an `origin` this binary is too old to
  know. `state` degraded all three to an empty document with a warning on
  stderr, which the first frame's clear-screen erased and which under Herdr goes
  to the plugin log instead, so the reason reached nobody either. A read now
  answers with the records or with why it has none: `list`, `status`, `stop`,
  `reconcile` and the panes refuse and name the file, and the manager pane opens
  on *"Could not list the forwards."* with the reason on its status line, where
  `h`, `enter`, `X` and `r` all leave it standing.

- A pass that could not read the records no longer sets them aside. Quarantine —
  moving a file to `<name>.corrupt` and starting over — is for bytes that were
  read and understood not to be ours; an unreadable file's bytes were never
  seen, and starting over on top of them is what turned a permission problem
  into two lost forwards. Move such a file aside yourself, or fix its
  permissions and it comes straight back.

- The manager pane counts forwards that are actually live, and marks the ones
  that are not. `herdr-portfwd list` reported `8788 live` and `54321 missing` in
  the same moment the pane above it said `(2 live, all hosts)`, drew both rows
  identically and offered the stop key on the dead one: the pane built its rows
  from `ForwardStatus` and dropped the `live` field, so a count of *records* was
  printed under the word `live`. A master dies under a user whenever a laptop
  sleeps or a link drops — that is the ordinary way a record goes stale, and it
  is why liveness is computed at all. The header now reads
  `(1 live, 1 not live, all hosts)`, the stale row is marked `not live`, and the
  same correction reaches every other sentence built the same way: what a filter
  is hiding (`1 live, 1 not live on other hosts`), the filtered header's total
  (`1 live of 3 recorded, host mini only` — the hidden rows are records, not
  live forwards), and the empty table, which now says *"No forwards are recorded
  on any host."* because the absence of a record is a fact about the file that
  was read and not about any port.

  Where this Herdr server is not the machine that binds the port — under
  `herdr --remote`, where the local agent opens it — nothing here can check, and
  the screen says `not checked` rather than choosing between the other two
  answers. That third state is why the wording is generated from a tally instead
  of written out: a sentence with a slot for "live" and one for "not live" has
  nowhere to put it.

- `herdr-portfwd status` reports a record file it could not read instead of
  becoming the failure. Its own doc comment says it "reports failures instead of
  becoming one" and both READMEs send users here first when something is already
  wrong — which is exactly when `forwards.json` may be the thing that is wrong —
  but a damaged one made it exit with nothing printed: not the path of the
  broken file, not the configured hosts, not the request queue that was
  perfectly readable. Each failure is now a field (`state_error`,
  `request_error`) and everything else is still gathered and printed, in
  `--json` as well. What could not be read is left blank rather than reported as
  zero: `forwards unknown` beside the host, `pending: unknown`, `null` in the
  JSON. The exit code is unchanged — non-zero, like every other command that
  could not do what it was asked — because reporting a failure and being one are
  different things and only the second loses the report.

- A status explaining why a pane's table is empty survives keys that could not
  change it. `h` overwrote it with *"there is nothing to filter"* whenever the
  snapshot held no rows, so one keystroke turned a screen that named the wrong
  host into one that only said there was nothing to narrow. `enter` and `X`
  ("nothing to stop") and the picker's `enter` ("nothing to forward") were the
  same shape: each reports on a listing, and there was none. `r` is the one key
  that can change the answer, and a refresh that succeeds retires both the
  failure and its message.

### Added

- `h` on the manager pane shows one host at a time, cycling through every host
  that has a forward and back to all of them. The header names the active
  filter, and a filtered view with nothing in it says how many forwards are live
  on the other hosts rather than reading as an empty world.

### Changed

- `X` on the manager pane, and the `stop-all` action, now mean **everything
  shown**: every forward on every host, or on the filtered host when the filter
  is on. The footer says `X stop shown` and the manifest description says the
  same, because "every forward belonging to the invoking host" stopped being
  something a user could point at once the pane showed more than one host.
  `herdr-portfwd stop-all --host <name>` is the old behaviour.

- `herdr-portfwd list` prints a first line saying how much of the world it is —
  `2 forward(s) on 2 host(s)`, or `... on <host> only (--host); other hosts not
  shown` — with the per-host header, master state and placement kept underneath,
  one section per host. `--json` changes shape with it: `scope` and `hosts`
  replace the old top-level `host`, `master_running`, `forwards`, `pending` and
  `placement`, which now live in each `hosts` entry.

- `herdr-portfwd status --json` gains `state_error` and `request_error`, and
  `pending_requests` and each `hosts[].forwards` become nullable: `null` where a
  file could not be read, so a reader can tell "nothing recorded" from "nothing
  learned". Both are `null` only when the matching `*_error` says why.

- A recorded forward whose host has left `hosts.toml` is listed and can be
  stopped rather than being hidden until the next `reconcile`. It is an open
  port either way, and the listing is the one screen that can do something about
  it.

## [0.2.0] - 2026-08-10

### Added

- A `confirm` pane, and a Ctrl+click now lands on it. The screen names the host,
  the workspace and agent the URL was attributed to, the remote and local ports,
  and — where this is the machine that would bind the port — whether that port
  is free; from there `enter` forwards and opens the page, `f` forwards without
  opening anything, `l` opens the forward manager, `s` opens the port picker,
  and `esc` does nothing at all. Whether a browser opens is a key on that screen
  rather than something a click implies.

  Three cases the screen states rather than papering over: the port is already
  forwarded, so `enter` only opens the page; the local port is held by something
  else, which is said out loud and still never remapped on its own; and the URL
  belongs to a host this invocation cannot resolve, in which case nothing is
  offered to forward and the reason is on screen instead of in a plugin log. The
  first two are questions about the machine that would bind the port, so under
  `herdr --remote` the screen answers neither — it says that this Herdr server
  is not the machine with the browser and that the local agent applies the
  forward, rather than inventing a "free" or an "in use".

  Herdr keeps one popup slot, so a click that arrives while the picker or a
  previous confirmation is up cannot have a screen. It is acted on rather than
  dropped, with the reason on stderr and in a Herdr notification: a click whose
  only trace was a plugin log nobody reads was worse than the forward the user
  asked for.

- `confirm_on_click` in `~/.config/herdr-portfwd/config.toml`, default `true`.
  Set it `false` to restore the previous behaviour, where a click forwarded the
  port and opened the browser in one motion.

- An `a` key on the `list` overlay, which opens the picker. With the `confirm`
  pane's `l` and `s`, one click now reaches every surface this plugin has.

### Changed

- `open-url` opens the confirmation instead of forwarding immediately. It
  follows the rule `scan` already followed, terminal included: run as an action
  it asks Herdr for the `confirm` pane, run at a terminal it draws the screen
  there itself, and only an invocation with neither — `--json`, or a shell
  invocation whose output is redirected, with no Herdr to ask — acts on the
  click rather than asking for a pane nobody would see. A pane command never
  asks for another pane.

- Under `herdr --remote` the confirmation opens on the remote, where the plugin
  runs, and "forward" there still means "record a request the local agent
  applies". The browser is still opened by the agent and never by the pane —
  and "forward only" now leaves the URL out of the request, so it stays a
  forward with no page.

## [0.1.0] - 2026-08-10

### Added

- Initial repository scaffolding: manifest, CI, release workflow, contributor
  documentation, and the architecture contract in `docs/ARCHITECTURE.md`.
- Port forwarding over an SSH ControlMaster this tool owns: `add`, `stop`,
  `stop-all`, and `list`. The local port always matches the remote one unless
  `--local-port` asks for something else; a conflict is reported, never
  silently remapped.
- `open-url`, the link-handler target. A clicked URL is re-validated instead of
  being trusted to the manifest pattern: only `http`/`https`, only an exactly
  loopback host, and only a plain decimal port in `1..=65535` get through.
- `scan`, which offers only remote listeners whose process ancestry reaches the
  `shell_pid` of a Herdr pane, so a host's infrastructure ports stay out of the
  list.
- `herdr --remote` support. `attach <host>` brings up a control master and a
  local agent, then hands the terminal to `herdr --remote <host>` and waits for
  it, so that one process outlives the session and can stop the agent, cancel
  the forwards and take the master down however the session ends. A plugin
  action running on the remote records a request that the agent applies on your
  machine, within one poll of it. The tool has to be installed on both
  machines, and the remote needs its own `hosts.toml`: both READMEs now say so.
- `reconcile`, wired to the `[[startup]]` and `[[events]]` hooks: it drops
  records whose host or pane is gone and re-adds forwards that vanished, so a
  stale record never shadows a live port.
- Two plugin panes: a `list` overlay of live forwards with stop keys, and a
  `picker` popup over attributed listeners.
- `status`, which reports hosts, config sources, control masters, state files
  and pending requests rather than failing when something is already wrong.
- `--json` on every command that answers with a report, for scripting and for
  reading back out of a plugin command log. The two `pane-ui` entrypoints are
  the exception: they draw a pane rather than print one, so `--json` there is
  taken as "not a person at a keyboard" and the pane is refused instead.
- End-to-end tests that run the binary with stand-ins for every external
  command, over the argv list read out of `herdr-plugin.toml` itself, so the
  manifest and the CLI cannot drift apart.

### Security

- Every forward now names its bind address (`ssh -L localhost:...`), and the
  control master is started with `GatewayPorts=no`. Without both, a user with
  `GatewayPorts yes` in `ssh_config` had each forwarded dev server bound to the
  wildcard address and reachable from their network, contradicting SECURITY.md's
  unconditional loopback promise.
- The far end of a forward is decided locally instead of read out of the request
  record. A remote could previously set `remote_bind` to any address and have
  the local agent tunnel `127.0.0.1:<port>` on your machine to a third host —
  its cloud metadata service, or a neighbour's SSH port. Anything but loopback
  is now replaced with `127.0.0.1` and the substitution is logged.
- State files and control sockets no longer fall back to `/tmp/herdr-portfwd`
  when `HOME` and the XDG variables are all unset; the command fails and says
  what to set. A state directory owned by another user, or one anyone can write
  in, is refused rather than used, and a state directory this tool creates is
  `0700`. The temp file behind every atomic write is created with `O_EXCL` under
  an unpredictable name, so a planted symlink cannot redirect the write.
- The control-socket directory is checked before a running master is trusted,
  not after, so a socket planted by another user cannot skip the check.
- `reconcile`, `list` and `status` sanitize remote-controlled text — ssh's
  stderr, pane ids, host names — the way the pane renderers already did, so a
  report read in a terminal cannot drive it.

### Changed

- Records live at `$XDG_STATE_HOME/herdr-portfwd` (falling back to
  `~/.local/state/herdr-portfwd`) whoever is running the command.
  `HERDR_PLUGIN_STATE_DIR` is no longer consulted: Herdr injects it for plugin
  commands only, so obeying it kept two sets of records that could not see each
  other. `forwards.json`, `requests.json` and `requests.acked` now sit in the
  same directory as the control sockets, chosen by the same rule, and it is that
  path `herdr-portfwd status` prints. Nothing needs migrating — this is
  pre-release, and any file left in a per-plugin directory by an earlier build
  can be deleted. The local agent still looks in the old locations on a *remote*
  host, so a machine running an older plugin over `herdr --remote` keeps working
  while you update it.

### Fixed

- Ctrl+click actually opens a browser under `herdr --remote`. The plugin runs on
  the remote host there, so it queued the forward and showed a notification
  saying the URL "opens once the local agent applies the forward" — and then
  nothing opened it, because the opener was wired only into the process the
  plugin runs in. The request now carries the URL to the local agent, which is
  the half of this tool on the machine with the browser. The field is optional
  in both directions, so an older agent ignores it and forwards the port exactly
  as before. The agent opens a URL only for a request it has applied, only after
  re-validating it as loopback on this side — it arrived in a file written on
  the remote — only when it names the local port the agent actually bound, and
  only once per URL.
- The `Port: scan remote listeners` action shows the user something. It is the
  flagship action, and the manifest and both READMEs describe it as listing
  attributed listeners and forwarding the ones you pick; it printed a text
  report into the captured plugin log instead, where nobody saw it. It now asks
  Herdr to open the `picker` pane, the way `add`, `stop` and `list` already did.
  `--json` and a plain shell invocation still print the report — `scan`, unlike
  the others, has a complete answer without asking anybody.
- A request the local agent has applied stops being pending. Nothing consumed
  the request queue, so `list` and `status` reported a forward that was already
  live as "waiting for the local agent" until `requests.json` was deleted by
  hand, and an agent that started inside the ten-minute TTL re-opened a port the
  user had since stopped. The agent now records how far it has got in a
  `requests.acked` file beside the queue — one integer, written by a rename, the
  only thing it can update safely on a host it reaches through a shell — and
  reads that mark back before it applies anything. A request is settled only
  once the agent knows what happened to it, so a forward that failed is still
  retried on the next pass.
- An explicit `add`, or a Ctrl+click, over a forward the agent had opened by
  itself now pins that forward. The record kept its original pane, so the next
  `pane.exited` reconcile cancelled the port the user had just asked to keep.
- A request whose local port a *different* forward already holds is reported
  instead of answered with that other forward. `add 3000 --local-port 5173` on
  the far side matched the record for remote 5173 on the local port alone: 3000
  was never opened and nothing said so, and the pin the request carried landed
  on the forward it had collided with. The local agent now applies the same test
  `add` applies on this machine — same remote port and same remote bind, not
  merely the same local port — tells the user once, and retries until the
  request expires.
- A remapped forward is no longer offered — or, with an `[[auto]]` rule, opened
  a second time — under the port the remote listens on. Discovery looked the
  record up by that number, and a record is keyed by the local port it was
  given.
- A refusal from `herdr` on a remote host is reported as itself. Only the first
  line of ssh's stderr survives the hop, so anything that spoke first — a
  control socket ssh fell back from, the login shell's rc files — was reported
  in its place: "herdr refused the request: Warning: Permanently added ...".
  The remote call now carries its own status and stderr back on stdout.
- The `Port: forward a port` and `Port: stop a forward` actions do something.
  A plugin action runs headless, so both took the "nobody to ask" refusal path
  from every keybinding and every `herdr plugin action invoke`, and the declared
  `picker` pane was never opened by anything. An action that was not told which
  port to act on now asks Herdr to open the pane that can ask —
  `plugin pane open --plugin portfwd --entrypoint picker` for `add`, `list` for
  `stop`. A pane's own command never asks, so a pane cannot open another one.
  Whether an invocation may ask is decided from `HERDR_PLUGIN_ACTION_ID`,
  `HERDR_PLUGIN_ENTRYPOINT_ID` and `HERDR_BIN_PATH`, not from the terminal;
  a shell invocation and a startup or event hook still get the refusal.
- The `Port: manage forwards` action does something. It invokes the `list`
  pane's own argv, and an action runs headless with both output streams
  captured, so invoking it drew one frame of the overlay into the plugin log and
  exited successfully without opening anything.
  It now asks Herdr for the pane, the way `add` and `stop` do. A `pane-ui`
  invocation with neither a terminal to draw on nor an action to ask on behalf
  of refuses and says so, instead of emitting a screenful of escape sequences
  and a zero exit.
- Whether a command opens a pane here or asks Herdr for one is now decided from
  the injected variables *before* the terminal is consulted. Herdr pipes an
  action's stdout and stderr but leaves stdin inherited, so a server started in
  the foreground (`herdr server`, `herdr --no-session`) hands its own terminal
  to every action it spawns: `add` from a keybinding drew the picker into the
  plugin log and then read keys off the terminal Herdr was reading, taking the
  user's keystrokes and never exiting.
- A pane is only drawn when stdin *and* stdout are terminals. `herdr-portfwd
  add > add.txt` from a shell used to put the terminal in raw mode and paint a
  blind picker into the redirect target at a guessed 80x24; it now falls
  through to the refusal that names what was missing.
- Walking up a long table in the `list` or `picker` pane moves the highlight
  before it scrolls. The visible window was derived from the selection alone,
  which pinned the highlight to the bottom row and scrolled the whole table on
  every keypress. The pane now remembers where it is scrolled to and moves that
  offset by the least it takes to keep the selection on screen, including after
  a stop or a refresh has made the table shorter.
- No pane line outgrows the pane. A long status line — an ssh error can be any
  length — wrapped, and the wrapped line cost a row the height budget had
  already spent, pushing the footer off the bottom. Lines are now cut to width
  with their escape sequences copied through whole, so a cut can neither leave
  half a sequence on screen nor leak a colour into the rest of the pane.
- The `remapped` note on a forward and the `forwarded` note on a listener are
  paid for by the column layout instead of hung past the right edge, where they
  wrapped for the same cost.
- A selected row stays highlighted across its whole width. Reverse video was
  closed by the first painted cell in the row — the yellow local port of a
  remapped forward — because a reset clears every attribute, so everything to
  its right rendered as an ordinary row.
- `stop` with no port suggested `herdr-portfwd add 5173`. Each command names
  itself in its own hint.
- Both READMEs had the `hosts.toml` precedence backwards: the herdr-portfwd
  file wins over the herdr-mirror one, which is what `status` reports as
  shadowed. They also now document `status`, the global `--host` flag, and the
  `HERDR_PORTFWD_MODE` override in a troubleshooting section.
- Both READMEs told users to bind keys in the *remote* `config.toml` under
  `herdr --remote`, which produced keys that never fire. The full rule is that a
  default attach (`--remote-keybindings local`) sends your local profile with
  every `[[keys.command]]` stripped out, *and* that profile replaces the
  server's for the client — so neither side's plugin-action bindings are in
  effect. They now say that, and give the two things that do work:
  `--remote-keybindings server` plus a binding in the remote config, or no
  keybinding at all — Ctrl+click is a link handler rather than a binding, and
  `herdr plugin action invoke portfwd.<id>` runs any action by name.
  `docs/ARCHITECTURE.md` fact 4 said the same wrong thing and now says this.
- Both READMEs promised that a Ctrl+click forwards the port and opens the page,
  without saying that under `herdr --remote` the browser is opened by the local
  agent on its next poll, and only if an agent is running. Each mode's behaviour
  and its lag are now spelled out.
- Both READMEs described the `herdr --remote` setup as two extra differences
  from the mirror case. It is a two-machine setup: `attach` and the agent are
  local commands, so the tool is needed on your machine as well as the remote,
  and every action on the remote resolves hosts from the *remote* `hosts.toml`,
  which nothing told users to create — leaving every action failing with "no
  hosts are configured". Both READMEs now walk through both machines.
- Both READMEs said detected ports are "applied on your approval", and
  `docs/ARCHITECTURE.md` said an offer came with "one key to accept". There is
  no accept key: a Herdr notification is a toast with no callback, so the offer
  names the port, the process, the pane and the `herdr-portfwd add <port>` that
  would forward it, and nothing is forwarded until the user runs it.
- Both READMEs listed `lsof`/`ss` on the remote as the only tool requirement.
  Attribution also needs `ps` on the remote — without it no listener can be
  traced to a pane — and this machine needs `ssh` with ControlMaster support and
  `open`/`xdg-open` for the browser handoff. Installing needs `git`, `bash`,
  `curl` and `sha256sum` or `shasum`. All of it is listed now, per machine.
- Both READMEs said `attach` "execs `herdr --remote`". It spawns the client and
  waits, deliberately: the process that started the master, the agent and the
  forwards has to outlive the session to take them down again.
- The Troubleshooting section read `herdr-portfwd status` from a shell as the
  truth about a forward some keybinding had made, which the split state
  directory made false. It is true on one machine now, and the section says what
  is still two-sided under `herdr --remote`: the queue lives with the plugin on
  the remote, the records with the agent on your machine. It also points at the
  agent's own log next to the control socket.
- Both READMEs claimed the tool never goes through a shell at all. `ssh` hands a
  remote command to the remote login shell, so it always did over there; what is
  true, and what they say now, is that commands started here are argv arrays,
  the remote snippets are fixed text whose only interpolation is a number, and a
  remote `herdr` argv is quoted word by word. `docs/ARCHITECTURE.md` invariant 9
  says the same.
- `SECURITY.md`'s threat model still said browser navigation required an
  affirmative action on this machine. Under `herdr --remote` it does not: the
  local agent opens a URL that arrived in a request file written on the remote.
  The document now describes what actually gates that — an agent the user
  started with `attach`, a request that agent applied, re-validation as loopback
  on this side, a port match against what that pass bound, one argv word to the
  opener, once per URL — and says plainly what a compromised remote can still
  cause: your browser opening a loopback URL of its choosing, on a port it got
  forwarded, with no click here.
- Both READMEs promised "an explicit choice" when a local port conflicts without
  ever naming it. `--local-port` is now documented where the conflict is
  described, including that it is the only way to remap — a Ctrl+click and the
  picker never do — and what taking it breaks.
- Both READMEs said the install "puts the CLI at `~/.local/bin/herdr-portfwd`".
  That step is best-effort and never fails the install; it can be skipped with a
  message. They now say what still works when it is (everything Herdr runs by
  path: actions, panes, the link handler), what does not (every command you type
  yourself, `herdr-portfwd attach` included, so all of `herdr --remote` mode),
  and how to get it back.
- Three smaller claims in both READMEs: "both are behind traits" named two while
  listing three handoffs — `ssh`, `herdr` and the browser opener, which are
  `SshRunner`, `HerdrClient` and `UrlOpener`; `scan` was described as listing
  listeners "per host" and `list` as showing "every live forward", when each acts
  on the one host the invocation resolves to; and `bash` was missing from the
  install requirements although the manifest's build step is
  `bash scripts/install.sh`.
- `reconcile` no longer overwrites the whole state file from a snapshot it took
  before its ssh round trips. A forward recorded by a concurrent `add`,
  `open-url` or the local agent used to be silently wiped, leaving it live with
  no record; a forward stopped mid-pass used to come back. Records written
  during a pass are merged, and a record removed during one is reported as
  dropped for a new reason, `superseded`.
- `scan` on the far side (`herdr --remote`) no longer probes this machine's
  ports and reports the answer as though it were the user's machine, where it
  was wrong for every candidate. `local_port_free` is now `null` in `--json`
  and the "local port in use" note is omitted rather than always shown.
- A forward to IPv6 loopback is written as `[::1]`, so the `-L` specification
  says what it means.
