# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

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
  local agent, then hands the terminal to `herdr --remote <host>`; a plugin
  action running on the remote records a request that the agent applies on your
  machine.
- `reconcile`, wired to the `[[startup]]` and `[[events]]` hooks: it drops
  records whose host or pane is gone and re-adds forwards that vanished, so a
  stale record never shadows a live port.
- Two plugin panes: a `list` overlay of live forwards with stop keys, and a
  `picker` popup over attributed listeners.
- `status`, which reports hosts, config sources, control masters, state files
  and pending requests rather than failing when something is already wrong.
- `--json` on every command, for scripting and for reading back out of a plugin
  command log.
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

### Fixed

- The `Port: forward a port` and `Port: stop a forward` actions do something.
  A plugin action runs headless, so both took the "nobody to ask" refusal path
  from every keybinding and every command-palette entry, and the declared
  `picker` pane was never opened by anything. An action that was not told which
  port to act on now asks Herdr to open the pane that can ask —
  `plugin pane open --plugin portfwd --entrypoint picker` for `add`, `list` for
  `stop`. A pane's own command never asks, so a pane cannot open another one.
  Whether an invocation may ask is decided from `HERDR_PLUGIN_ACTION_ID`,
  `HERDR_PLUGIN_ENTRYPOINT_ID` and `HERDR_BIN_PATH`, not from the terminal;
  a shell invocation and a startup or event hook still get the refusal.
- The `Port: manage forwards` action does something. It invokes the `list`
  pane's own argv, and an action runs headless with both output streams
  captured, so selecting it from the command palette drew one frame of the
  overlay into the plugin log and exited successfully without opening anything.
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
- `stop` with no port suggested `herdr-portfwd add 5173`. Each command names
  itself in its own hint.
- Both READMEs had the `hosts.toml` precedence backwards: the herdr-portfwd
  file wins over the herdr-mirror one, which is what `status` reports as
  shadowed. They also now document `status`, the global `--host` flag, and the
  `HERDR_PORTFWD_MODE` override in a troubleshooting section.
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
