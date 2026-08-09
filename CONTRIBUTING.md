# Contributing

Thanks for taking an interest. This is a small, focused tool, and the bar for
changes is mostly about not breaking the invariants in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Before you start

Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). It records why the design is
split the way it is, which parts depend on verified Herdr behavior, and the
invariants tests are expected to defend. Most review comments on this repo trace
back to something in that file.

For anything larger than a bug fix, open an issue or a discussion first so we
can agree on the shape before you spend time on it.

## Setup

```bash
git clone https://github.com/hacnosuke/herdr-portfwd
cd herdr-portfwd
just check
```

To run your working tree as a real plugin:

```bash
cargo build --release
herdr plugin link .
herdr server reload-config
herdr plugin action list --plugin portfwd
```

`herdr plugin link` does not run build commands, so rebuild yourself after
changes. `herdr plugin unlink portfwd` when you are done.

## Rules the code follows

- **No `unwrap()` or `expect()` in non-test code.** Return an error and let the
  caller decide.
- **Everything that shells out goes behind a trait.** `SshRunner` and
  `HerdrClient` exist so tests never spawn a real process. A test that invokes
  real `ssh`, real `herdr`, or the network will be rejected.
- **Commands are argv arrays.** No string concatenation into a shell. No user
  input reaching `sh -c`.
- **Paths that become Unix socket addresses get length-checked** before use. The
  `sun_path` limit is 104 bytes and violating it fails at connect time with a
  confusing error.
- **Parsers take `&str` and return values.** `lsof`, `ss`, `ps`, and TOML
  parsing must be callable from a test with a fixture string and no I/O.
- **New dependencies need a reason.** Check whether an existing one covers it.

## Tests

```bash
just test          # unit + integration
just check         # fmt --check, clippy -D warnings, tests
```

A change is expected to come with tests. Specifically:

- New parsing gets fixture-based tests, including malformed input.
- New behavior that touches one of the numbered invariants in
  `docs/ARCHITECTURE.md` gets a test that fails if the invariant is violated.
- Bug fixes get a regression test that fails before the fix.

Adversarial cases are welcome and expected: cyclic PID graphs, a `lsof` line
with spaces in the command name, a state file truncated mid-write, a forward
that vanished behind the tool's back, a host whose control path is too long.

## Commits and pull requests

Lowercase [conventional commits](https://www.conventionalcommits.org/):

```
fix: cancel a forward that no longer exists without erroring

refs #12
```

No emojis. Keep the subject descriptive; it feeds the changelog.

Before opening a PR:

1. `just check` passes.
2. The PR description says what changed and why, and names any invariant it
   touches.
3. User-facing changes update `README.md`, `README.ja.md`, and `CHANGELOG.md`
   under `## Unreleased`.

## Reporting bugs

Use the issue template. The parts that actually matter for triage:

- `herdr --version` on **both** ends, and `herdr-portfwd --version`.
- Which mode: herdr-mirror, `herdr --remote`, or SSH-then-`herdr`.
- Host OS on both ends.
- The shortest reproduction you can manage.
- Relevant output from `herdr plugin log list --plugin portfwd`.

## Security

Do not open a public issue for a vulnerability. See [SECURITY.md](SECURITY.md).
