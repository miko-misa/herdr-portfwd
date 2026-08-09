# herdr-portfwd

[![CI](https://github.com/hacnosuke/herdr-portfwd/actions/workflows/ci.yml/badge.svg)](https://github.com/hacnosuke/herdr-portfwd/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A [Herdr](https://herdr.dev) plugin that brings dev-server ports from your
remote hosts to `localhost`, and lets you start, stop, and inspect those
forwards without leaving Herdr.

*[日本語版 README](README.ja.md)*

Your agent runs `npm run dev` on a build box and prints
`http://localhost:5173`. Ctrl+click it. The port is forwarded and the page opens
in your browser, on the same port number, on the machine you are actually
sitting at.

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
because the printed URL is the point. Renumbering breaks Vite HMR, OAuth
`redirect_uri`, and any absolute URL the app emits. On a conflict you get a
clear report and an explicit choice, never a surprise port.

**It never forwards behind your back.** Detected ports are *offered* through a
Herdr notification and applied on your approval. Fully automatic forwarding
exists, but it is opt-in per workspace.

## Requirements

- Herdr **0.8.0** or newer on both ends.
- macOS or Linux, x86_64 or aarch64.
- Non-interactive SSH key auth to each host:
  `ssh -o BatchMode=yes <host> true` must succeed without a prompt.
- `lsof` (macOS) or `ss` (Linux) on the remote host, for port discovery.

## Install

```bash
herdr plugin install hacnosuke/herdr-portfwd
herdr server reload-config
```

This also links the CLI at `~/.local/bin/herdr-portfwd`.

Hosts are read from `~/.config/herdr-mirror/hosts.toml` when it exists, so
[herdr-mirror](https://github.com/nikok6/herdr-mirror) users need no extra
configuration. Otherwise create `~/.config/herdr-portfwd/hosts.toml`:

```toml
[hosts.buildbox]
target = "buildbox"   # anything ssh accepts: alias, user@host, ssh://host:2222
prefix = "build"      # matches the "build: " prefix on mirrored workspaces
```

## Which mode are you in?

Herdr can put its server on either side of the connection, and that decides
where a plugin runs. `herdr-portfwd` supports both, but setup differs.

### With herdr-mirror (server is local)

Nothing extra. Install the plugin locally, bind keys in your local
`~/.config/herdr/config.toml`, done.

### With `herdr --remote` (server is remote)

Under `--remote` your local Herdr is a thin client; the server, and therefore
every plugin, runs on the remote host. A plugin there cannot open a port on your
laptop. So the plugin declares *what* should be forwarded and a small local
agent performs it.

Start your session through the wrapper instead of `herdr --remote` directly:

```bash
herdr-portfwd attach buildbox     # starts the local agent, then execs herdr --remote buildbox
```

Two more differences to know about:

- Install the plugin **on the remote host** (`herdr plugin install ...` there).
- Bind keys in the **remote** `config.toml`. Herdr strips every
  `[[keys.command]]` binding — including `type = "plugin_action"` — from the
  profile it sends to a remote server, so local bindings will not reach it.

## Usage

**Ctrl+click a URL.** Any `http://localhost:PORT` or `http://127.0.0.1:PORT` in
a pane routes to the plugin: the port is forwarded, then opened locally.
Ctrl+click rather than Cmd+click, on every platform, because captured terminal
mouse reports cannot tell Cmd+click from a plain click.

**Pick from a list.** The `scan` action shows attributed listeners per host and
forwards the ones you select.

**Manage what is running.** The `list` action opens an overlay with every live
forward, its owning pane, and a key to stop it.

### Actions

| Action | Does |
| --- | --- |
| `portfwd.scan` | list attributed remote listeners and forward the chosen ones |
| `portfwd.add` | forward a port you type |
| `portfwd.list` | overlay of live forwards, with stop keys |
| `portfwd.stop` | stop a selected forward |
| `portfwd.stop-all` | stop every forward for the current host |
| `portfwd.open-url` | link-handler target; forward, then open locally |

### Keybindings

Actions ship with no default keys. Bind them in `config.toml` — the **remote**
one under `herdr --remote`, the local one with herdr-mirror:

```toml
[[keys.command]]
key = "prefix+p"
type = "plugin_action"
command = "portfwd.scan"
description = "forward a remote port"

[[keys.command]]
key = "prefix+shift+p"
type = "plugin_action"
command = "portfwd.list"
description = "manage port forwards"
```

Then `herdr server reload-config`.

## Configuration

`~/.config/herdr-portfwd/config.toml`:

```toml
# How often the agent looks for new listeners. 0 disables discovery entirely.
poll_seconds = 5

# Offer newly detected ports through a Herdr notification. Default: true.
offer_new_listeners = true

# Only offer listeners that belong to a Herdr pane. Default: true.
require_pane_attribution = true

# Opt-in automatic forwarding, matched per workspace. Empty by default.
[[auto]]
workspace = "scholion"
ports = [5173, 8787]
```

## Security

The plugin runs SSH as you, with your keys and your agent, and only ever binds
to loopback on your machine. It shells out with argv arrays, never through a
shell. See [SECURITY.md](SECURITY.md) for the reporting process.

## Development

```bash
just check      # fmt + clippy + tests
just test
cargo build --release
herdr plugin link .          # link this working tree
herdr server reload-config
```

No test invokes real `ssh`, real `herdr`, or the network — both are behind
traits with test fakes. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the
design and its invariants, and [CONTRIBUTING.md](CONTRIBUTING.md) before opening
a pull request.

## License

MIT — see [LICENSE](LICENSE).
