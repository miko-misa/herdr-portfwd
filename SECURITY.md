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

- **Loopback only.** Local forwards bind `127.0.0.1`/`::1`, never a routable
  address. A forwarded dev server is not exposed to your network.
- **No shell.** Every external command is an argv array. No user-controlled
  string is concatenated into a shell command line.
- **No new authentication path.** It reuses OpenSSH and your existing
  configuration. It never prompts for, stores, or forwards credentials.
- **No forwarding without intent.** A forward requires an explicit action, an
  accepted offer, or a match against an allowlist you wrote.

What is explicitly out of scope:

- A remote host you have already granted SSH access to is trusted. A malicious
  remote can print any URL it likes into a pane; Ctrl+clicking it is an
  affirmative action by you, and the resulting forward reaches only that host.
- Herdr plugins are unsandboxed by design. Installing this plugin means running
  its code, the same as any editor or shell extension.

## Notes for reviewers

Areas worth extra scrutiny in a security review:

- Local port selection and bind address, in `ssh` and `actions`.
- URL parsing in the link handler: only loopback hosts and a plain port should
  ever produce a forward.
- Control socket paths: mode `0700`, user-owned, length-checked.
- State file writes: atomic replace, no predictable path in a world-writable
  directory.
