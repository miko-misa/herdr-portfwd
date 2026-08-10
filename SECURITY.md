# Security Policy

## Supported versions

The latest release is supported. Fixes are shipped in a new release rather than
backported.

## Reporting a vulnerability

Please report privately, not in a public issue:

- GitHub → **Security** → **Report a vulnerability** (preferred), or
- email `hacnosuke@gmail.com`

Include what an attacker can do, the steps to reproduce it, and the affected
version. You can expect an initial reply within a week.

## Threat model

`herdr-portfwd` runs as you, with your SSH keys and your `ssh-agent`. It has the
same reach you do. What it promises:

- **Loopback only.** The listening end of every forward names its bind address
  explicitly (`ssh -L localhost:...`), and the control master is started with
  `GatewayPorts=no`, so a `GatewayPorts yes` in your `ssh_config` cannot widen
  it. A forwarded dev server is not exposed to your network. The far end is
  clamped to loopback here too, rather than read out of the request record.
- **No shell.** Every external command started on this machine is an argv array.
  `ssh` hands a remote command to the remote login shell, so over there the rule
  is kept from the other side: those snippets are fixed text whose only
  interpolation is a `u64`, and a `herdr` argv is quoted word by word.
- **No new authentication path.** It reuses OpenSSH and your existing
  configuration. It never prompts for, stores, or forwards credentials.
- **No forwarding without intent.** A forward requires an explicit action, an
  accepted offer, or a match against an allowlist you wrote.

### What can open your browser

This is the part that is not gated by a local affirmative action, and it is
worth reading before you decide the tool is safe for your setup.

Under `herdr --remote` the plugin runs on the remote host and the browser is
here, so a queued forward request carries an optional URL for whoever applies
it. The local agent on this machine — started only by
`herdr-portfwd attach <host>` — is what hands that URL to `open`/`xdg-open`.
Nobody clicks anything locally at that moment. What gates it is
`Agent::open_requested_url` and what has to have happened before it runs:

1. **An agent has to be running for that host.** No `attach`, no agent, no
   browser: nothing else in this tool opens a URL that arrived over the wire.
2. **The request has to be one the agent applied.** One for another host, one
   past its ten-minute TTL, and one whose local port a different forward already
   holds all open nothing — a URL over a port nothing is listening on is an
   error page nobody asked for.
3. **The URL is re-parsed here, never trusted.** It was written on the remote,
   into a file on the remote, and read back over `ssh`, so the agent runs it
   through the same validator the plugin used: only `http`/`https`, only a host
   that is exactly `localhost`, `127.0.0.1` or `[::1]`, only a plain decimal port
   in `1..=65535`. Userinfo (`http://localhost@evil.example/`), non-ASCII,
   control characters, whitespace and backslashes in the authority are all
   rejected.
4. **It has to name the local port that pass actually bound.** Any other port is
   reported and not opened.
5. **What reaches the opener is re-rendered from the validated parts**, handed
   over as one argv word rather than a shell string, and each URL is opened at
   most once.

So, honestly: a remote you have granted SSH access to — or malware running as
you on it — can, while an agent is running for that host, make your browser open
`http(s)://localhost:<port><path>`, where `<port>` is a port it got forwarded
and the path, query and fragment are copied verbatim from the request. It does
not need you to click anything, because it can write the request file itself.
Browsers scope cookies by host and not by port, so a page served to you through
that tunnel shares `localhost` cookies with anything else you run on loopback.

It cannot make this machine open a non-loopback host, any other scheme (no
`file:`, no `javascript:`, no custom app scheme), a port it was not forwarded, or
a URL belonging to a request that was never applied — and it cannot reach a
shell through the URL, because the opener is an argv array.

What is explicitly out of scope:

- A remote host you have already granted SSH access to is trusted with what that
  access already gives it. A malicious remote can print any URL it likes into a
  pane, and under `herdr --remote` it can queue forward requests; the section
  above is what bounds where those can reach. With herdr-mirror there is no
  request queue and no agent — the plugin is here, the Ctrl+click is yours, and
  the URL goes through the same validator before anything is forwarded or
  opened.
- Herdr plugins are unsandboxed by design. Installing this plugin means running
  its code, the same as any editor or shell extension.

## Notes for reviewers

Areas worth extra scrutiny in a security review:

- Local port selection and bind address, in `ssh` and `actions`.
- URL parsing in the link handler: only loopback hosts and a plain port should
  ever produce a forward. The URL is validated once when the click is turned
  into the `confirm` pane's payload, and again in `open_url_for` when the pane
  acts on it — under `herdr --remote` that payload made a round trip through an
  environment variable on the host whose terminal printed the URL, so the second
  pass is not redundant.
- `Agent::open_requested_url` in `local_agent`: the re-validation of a URL that
  came from the remote, the check that it names the port the agent bound, and
  the record that keeps it to one opening.
- Control socket paths: mode `0700`, user-owned, length-checked.
- State file writes: atomic replace, no predictable path in a world-writable
  directory.
