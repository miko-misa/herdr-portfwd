# herdr-portfwd

[![CI](https://github.com/miko-misa/herdr-portfwd/actions/workflows/ci.yml/badge.svg)](https://github.com/miko-misa/herdr-portfwd/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A [Herdr](https://herdr.dev) plugin that brings dev-server ports from your
remote hosts to `localhost`, and lets you start, stop, and inspect those
forwards without leaving Herdr.

*[日本語版 README](README.ja.md)*

Your agent runs `npm run dev` on a build box and prints
`http://localhost:5173`. Ctrl+click it. The port is forwarded under the same
number to the machine you are actually sitting at, and that machine's browser
opens the page — immediately with herdr-mirror, and within one poll of the local
agent (5 seconds by default) under `herdr --remote`, where the forward can only
be made by the half of this tool that runs on your machine.

## What makes it different from `ssh -L`

**It knows which port belongs to which agent.** A real host's listening ports
are mostly infrastructure — a tunnel daemon, a VM manager, an SSH mux socket.
`herdr-portfwd` walks each listener's process ancestry up to the `shell_pid` of
a Herdr pane, so the port list you are offered looks like this:

```
mini: scholion   claude   :5173   node (vite)
mini: api        codex    :8787   workerd
```

not like this:

```
127.0.0.1:20241  cloudflared    127.0.0.1:49313  limactl    *:53  limactl
```

Ports it cannot attribute to a pane are not offered by default.

**It does not silently renumber.** The local port matches the remote port,
because the printed URL is the point. On a conflict — another process on this
machine, or a forward of some other remote port — you get a report naming what
holds the port, and nothing is changed. The explicit choice is `--local-port`,
on `add`:

```bash
herdr-portfwd add 5173 --local-port 15173
```

That is the only way to remap: a Ctrl+click never renumbers, and the picker
offers no remap either. Taking it breaks anything that embeds 5173 — Vite HMR,
OAuth `redirect_uri`, and any absolute URL the app prints — because the server
on the other end still believes it is on 5173.

**It never forwards behind your back.** A newly detected port becomes a Herdr
notification naming the port, the process, the pane it belongs to, and the
command that would forward it — `herdr-portfwd add 5173`. There is no accept
key: a Herdr notification is a toast, so accepting an offer means running that
command, or picking the port out of `scan`. Fully automatic forwarding exists,
but it is opt-in per workspace.

## Requirements

On the machine you sit at:

- Herdr **0.8.0** or newer.
- macOS or Linux, x86_64 or aarch64.
- OpenSSH's `ssh`, with ControlMaster multiplexing (`-O forward`, `-O check`).
  Every forward is made on a master this tool owns.
- Non-interactive SSH key auth to each host:
  `ssh -o BatchMode=yes <host> true` must succeed without a prompt.
- `open` (macOS) or `xdg-open` (Linux). This is the browser handoff, and it is
  always done here, because this is where the browser is.
- Under `herdr --remote`, `herdr-portfwd` itself: `attach` and the local agent
  are local commands. See the two-machine setup below.

On each remote host:

- Herdr **0.8.0** or newer — under `herdr --remote` it is the server, and every
  plugin command runs there.
- `lsof` (macOS) or `ss` (Linux): discovery runs
  `lsof -nP -iTCP -sTCP:LISTEN -F pcn` or `ss -ltnpH` on that machine.
- `ps`. Attribution walks the parent map from `ps -eo pid=,ppid=`, so without it
  no listener can be traced to a pane and `scan` has nothing to offer.
- A POSIX login shell, which is what `ssh` hands the few remote commands to.
- Under `herdr --remote`, the plugin installed there too, with its own
  `hosts.toml`. Again, see below.

`herdr plugin install` itself needs `git` (Herdr clones the plugin) and `bash`
(the manifest's build step is `bash scripts/install.sh`), plus `curl` and either
`sha256sum` or `shasum` for the binary it fetches, on whichever machine you
install on.

## Install

```bash
herdr plugin install miko-misa/herdr-portfwd
herdr server reload-config
```

The install step also *tries* to put the CLI at `~/.local/bin/herdr-portfwd`, so
`herdr-portfwd <cmd>` works from a shell and keybindings have one stable path. It
is a copy rather than a symlink, because Herdr moves the plugin checkout after
the build step has run.

That copy is best-effort and never fails the install. If `HOME` is unset, if
`~/.local/bin` cannot be created or written, or if something is already sitting
at that name as a directory, the step is skipped with a line on stderr naming
the reason, and the install still succeeds:

```
herdr-portfwd: could not install the CLI on PATH: cannot write to /home/you/.local/bin
```

Being skipped costs you less than it sounds like. Actions, panes and the
Ctrl+click handler all run the binary in the plugin checkout by path, so
everything Herdr itself starts keeps working. What is missing is every command
you type yourself: `herdr-portfwd status`, the `herdr-portfwd add <port>` an
offer notification tells you to run, and `herdr-portfwd attach <host>` — which
is the whole `herdr --remote` mode. To get them, fix the path (usually
`mkdir -p ~/.local/bin`) and re-run
`herdr plugin install miko-misa/herdr-portfwd`, as the message itself says. The
copy always lands in that one directory, so make sure it is on your `PATH` too.

Hosts are read from the **first** of these files that exists, on the machine the
command runs on:

1. `~/.config/herdr-portfwd/hosts.toml`
2. `~/.config/herdr-mirror/hosts.toml`

The format is the same, so [herdr-mirror](https://github.com/nikok6/herdr-mirror)
users need no extra configuration. If both files exist the herdr-portfwd one
wins and the herdr-mirror one is ignored entirely — they are not merged.
`herdr-portfwd status` prints the file it loaded and lists the other as
`shadowed`, so a host that seems to have gone missing is one line away.

To configure hosts yourself, create `~/.config/herdr-portfwd/hosts.toml`:

```toml
[hosts.buildbox]
target = "buildbox"   # anything ssh accepts: alias, user@host, ssh://host:2222
prefix = "build"      # matches the "build: " prefix on mirrored workspaces
```

## Which mode are you in?

Herdr can put its server on either side of the connection, and that decides
where a plugin runs. `herdr-portfwd` supports both, but setup differs.

### With herdr-mirror (server is local)

The plugin runs on your machine, so it can do the whole job itself. Install it
locally, bind keys in your local `~/.config/herdr/config.toml`, and reload. A
Ctrl+click forwards the port and opens the browser in the same breath.

One thing does not run in this mode: the polling that notices new listeners,
raises the L2 offers, and honours the `[[auto]]` allowlist lives in the local
agent, and the agent is started by `attach`, which belongs to `herdr --remote`.
With herdr-mirror, ports get forwarded by Ctrl+click, `scan`, and `add`.

### With `herdr --remote` (server is remote)

Under `--remote` your local Herdr is a thin client; the server, and therefore
every plugin, runs on the remote host. A plugin there cannot open a port on your
laptop, and cannot reach your browser either. So the plugin declares *what*
should be forwarded, and a small local agent on your machine performs it and
opens the page. That means the tool is installed and configured on **both**
machines.

On the remote host:

1. Install the plugin there — `herdr plugin install miko-misa/herdr-portfwd`
   followed by `herdr server reload-config`, on that machine. Every action,
   every plugin pane, and the Ctrl+click link handler run there.
2. Create `~/.config/herdr-portfwd/hosts.toml` **there too**. Every action
   resolves its host from the `hosts.toml` of the machine it runs on, so without
   this file the remote plugin fails with *"no hosts are configured"* and
   nothing is ever queued for the agent:

   ```toml
   [hosts.buildbox]
   target = "buildbox"
   ```

   `target` is required by the file format, but the remote never dials it — the
   remote does not open the tunnel. What has to line up is the **name**: the
   agent applies a request only when the request names the host it is serving,
   matched against that host's name or its target. Use the same name you attach
   with.

On your machine:

3. Install it here as well — the same `herdr plugin install` is what puts the
   `herdr-portfwd` CLI in `~/.local/bin`, which is all this side needs, so check
   that `herdr-portfwd --version` actually runs before going further; see
   [Install](#install) if it does not — and have
   `~/.config/herdr-portfwd/hosts.toml` name the same host with a `target` your
   `ssh` can actually reach. `attach buildbox` alone will fall back to treating
   the name as the ssh target, but `status`, `list` and `stop` here resolve
   through that file, so write it.
4. Start the session through the wrapper instead of `herdr --remote` directly:

   ```bash
   herdr-portfwd attach buildbox
   ```

   `attach` brings up an SSH ControlMaster of its own for that host, starts the
   local agent in the background — its output goes to `<socket>.agent.log`,
   beside the control socket — and then runs `herdr --remote buildbox` on your
   terminal and waits for it. It does not `exec` the client, despite looking
   like it should: something has to outlive the session to stop the agent,
   cancel the forwards, and shut the master down, which it does on exit, on a
   signal, and on a panic. Arguments after the host are passed through:
   `herdr-portfwd attach buildbox --session work`.

Then there are the keybindings, which are the part that surprises people: see
[Keybindings](#keybindings) below, because under a plain `herdr --remote` a key
bound to a plugin action does not fire on either side.

## Usage

**Ctrl+click a URL.** Any `http://localhost:PORT`, `http://127.0.0.1:PORT` or
`http://[::1]:PORT` in a pane — `https` too — routes to the plugin: the port is
forwarded, then opened on your machine. Ctrl+click rather than Cmd+click, on
every platform, because captured terminal mouse reports cannot tell Cmd+click
from a plain click.

What happens next differs by mode. With herdr-mirror the plugin is already on
your machine: it forwards the port and hands the URL to your browser, in one
step. Under `herdr --remote` the click is handled on the remote, which puts up a
notification and queues the forward with the URL attached; your local agent
makes the forward and opens the browser on its next poll — up to `poll_seconds`
later, 5 seconds by default. Nothing opens if no agent is running, which is what
`herdr-portfwd attach` is for.

**Pick from a list.** The `scan` action shows the attributed listeners on the
selected host in a picker pane and forwards the ones you select.

**Manage what is running.** The `list` action opens an overlay with every live
forward for the selected host, its owning pane, and a key to stop it. Both act on
one host at a time — the one the invocation resolves to, or the one `--host`
names.

### Actions

| Action | Does |
| --- | --- |
| `portfwd.scan` | picker over attributed remote listeners; forwards the chosen ones |
| `portfwd.add` | forward a port; opens the picker when no port was named |
| `portfwd.list` | overlay of live forwards, with stop keys |
| `portfwd.stop` | stop a selected forward |
| `portfwd.stop-all` | stop every forward for the current host |
| `portfwd.open-url` | link-handler target; forward, then open locally |

### Keybindings

Actions ship with no default keys. Where you bind them depends on which
keybindings the session is actually running, and that is not the same question
as where the plugin runs.

**With herdr-mirror**, bind them in your local `~/.config/herdr/config.toml`:

```toml
# f for "forward". Herdr's defaults already use most letters — prefix+p is
# previous_tab and prefix+shift+p is rename_pane — so check yours before
# picking: `herdr --default-config | grep prefix+`.
[[keys.command]]
key = "prefix+f"
type = "plugin_action"
command = "portfwd.scan"
description = "forward a remote port"

[[keys.command]]
key = "prefix+shift+f"
type = "plugin_action"
command = "portfwd.list"
description = "manage port forwards"
```

Then `herdr server reload-config`.

A binding to an action id that does not exist is **not** reported: config reload
accepts it silently and the key does nothing when pressed. If a key seems dead,
check the id against `herdr plugin action list --plugin portfwd`.

**With a plain `herdr --remote`, no custom command binding fires at all** —
neither yours nor the remote's. `herdr --remote` defaults to
`--remote-keybindings local`, so the client sends *your* keybinding profile to
the remote server, and that profile is built without any `[[keys.command]]`,
including `type = "plugin_action"` (those commands would run on the remote, so
Herdr does not send them, and the server clears the list again on arrival).
Because your profile *replaces* the server's for the duration of the attach, the
remote's own `[[keys.command]]` are not in effect either. A key bound in either
file simply never happens. Two things work instead:

- **Ask for the remote's keybindings**, and bind on the remote:

  ```bash
  herdr-portfwd attach buildbox --remote-keybindings server
  ```

  Now the remote server keeps its own profile, and the block above works in the
  remote `~/.config/herdr/config.toml` (`herdr server reload-config` there
  afterwards). The trade is the whole profile: your prefix key and every other
  binding come from the remote's config too, for that session.

- **Or use no keybinding.** Ctrl+click needs none — link handlers come from the
  plugin manifest and are resolved by the server the plugin is installed on,
  never from a keybinding profile. And any action can be invoked by name from a
  pane in that session:

  ```bash
  herdr plugin action list --plugin portfwd
  herdr plugin action invoke portfwd.scan
  herdr-portfwd add 5173     # what the offer notification tells you to run
  ```

## Configuration

`~/.config/herdr-portfwd/config.toml`, read on whichever machine the command
runs on. Under `herdr --remote` that means the agent's settings are the ones on
*your* machine, and `scan`'s are the ones on the remote:

```toml
# How often the local agent polls for requests and for new listeners. 0 disables
# discovery; requests are still picked up every 5s. Read where the agent runs.
poll_seconds = 5

# Offer newly detected ports through a Herdr notification. Default: true.
# Read where the agent runs; the notification is shown in your Herdr session.
offer_new_listeners = true

# Only consider listeners that belong to a Herdr pane. Default: true.
# Read by the agent, and by `scan` wherever `scan` runs.
require_pane_attribution = true

# Opt-in automatic forwarding, matched per workspace. Empty by default, and
# acted on only by the agent.
[[auto]]
workspace = "scholion"
ports = [5173, 8787]
```

## Troubleshooting

### Start with `status`

```bash
herdr-portfwd status
```

It answers instead of failing, which is the point: it is what to run when
something is already wrong. It prints which `hosts.toml` was loaded and which
files that shadows, where the state and request files live, which host this
invocation resolves to **and by which rule**, whether a control master is up
per host, the forwards recorded for each, and how many requests are still
waiting for the local agent. `--json` if you want to grep it.

**One machine, one directory.** Records live in `$XDG_STATE_HOME/herdr-portfwd`,
or `~/.local/state/herdr-portfwd` — the same directory the control sockets are
in — no matter who is running the command: a keybinding, a plugin pane, an event
hook, the local agent, or your shell. `HERDR_PLUGIN_STATE_DIR` is deliberately
not consulted, because Herdr injects it for plugin commands and nothing else:
obeying it gave the keybinding one set of records and your shell another, so
`herdr-portfwd status` could report no forwards while the ports were open. The
path it prints is the one everything on that machine uses.

**Two machines are still two directories**, and that is exactly what
`herdr --remote` is. `forwards.json` is written by whoever opens the port, which
is the local agent on your machine; `requests.json` is written by the plugin,
which is on the remote. So ask the right machine:

- On your machine, `herdr-portfwd status` is the truth about which ports are
  actually forwarded and whether the master is up.
- On the remote — a pane of that session, or the `list` action — `status` shows
  what the plugin has queued and not yet had settled. Recorded forwards are
  empty there, because nothing over there opens a port.

"Waiting" counts only what nobody has dealt with yet. The local agent records
how far it has got in a `requests.acked` file beside `requests.json`, and every
read of the queue folds that mark in, so a request the agent has already applied
stops being counted — on the remote host too, which is where `status` runs under
`herdr --remote`. A queue that never drains means the agent is not running, or
cannot write beside the request file it is reading.

Two logs are worth knowing about. The local agent's own output goes to
`<control socket>.agent.log` next to the socket — that is where to look when a
Ctrl+click queued a forward and no browser appeared. The log of what Herdr ran
is `herdr plugin log list --plugin portfwd`, on the machine running the server,
which under `herdr --remote` is the remote one.

### `--host`: act on a host other than the one that was picked

Every command takes it, and it beats every other signal — the mirror prefix on
the invoking workspace, `default_host`, and the only-one-host-configured rule:

```bash
herdr-portfwd list --host buildbox
```

`HERDR_PORTFWD_HOST` does the same from a shell. Reach for it when `status`
reports the wrong host, or none: *"cannot tell which host to use"* means several
hosts are configured and nothing about the invocation pointed at one.

### `HERDR_PORTFWD_MODE`: override which side the tool thinks it is on

Only a process on *your* machine can open a port on it, so the plugin has to
know which side of the SSH connection it is running on. Nothing Herdr injects
says so, so it works it out: a mirrored workspace prefix, then a live control
master here, then a mirrored pane on this server, and otherwise it assumes it is
on the remote and records a request for the local agent. `status` reports both
the answer and the reason, for example `local (live control master)`.

When that inference is wrong, say so:

```bash
HERDR_PORTFWD_MODE=local herdr-portfwd add 5173    # open the port from here
HERDR_PORTFWD_MODE=remote herdr-portfwd add 5173   # record a request for the local agent
```

The value wins over every rule above, and `status` then reads
`local (HERDR_PORTFWD_MODE)`. Anything other than `local` or `remote` is
ignored rather than fatal. Set it per command rather than exporting it: `local`
where the tool is not on your machine binds a port nobody can reach, and
`remote` on your own machine leaves a request that only
`herdr-portfwd attach <host>` starts an agent to apply.

## Security

The plugin runs SSH as you, with your keys and your agent, and only ever binds
to loopback on your machine. Every command it starts here is an argv array,
never a string handed to a shell. `ssh` gives a remote command to the remote
login shell, so the few things that run over there are held to the same rule
from the other side: the snippets that locate the request file and record the
agent's progress are fixed text with a number as the only thing ever
interpolated, and a `herdr` call is quoted word by word so it survives exactly
one round of shell parsing.

The browser handoff is the one thing a remote can reach on this machine without
you clicking anything here: under `herdr --remote` a queued request can carry a
URL, and the local agent opens it. It is re-validated as loopback on this side,
has to name the local port that pass actually bound, and reaches
`open`/`xdg-open` as one argv word. [SECURITY.md](SECURITY.md) spells out what
that does and does not stop, and how to report a vulnerability.

## Development

```bash
just check      # fmt + clippy + tests
just test
cargo build --release
herdr plugin link .          # link this working tree
herdr server reload-config
```

No test invokes real `ssh`, real `herdr`, a real browser, or the network. Those
three programs are each reached through a trait — `SshRunner`, `HerdrClient`,
`UrlOpener` — with a test fake in its place. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and its invariants,
and [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

## License

MIT — see [LICENSE](LICENSE).
