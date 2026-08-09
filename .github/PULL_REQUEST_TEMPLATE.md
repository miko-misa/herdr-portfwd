## What and why

<!-- What changed, and what problem it solves. Link the issue if there is one. -->

## Invariants touched

<!--
Name any numbered invariant from docs/ARCHITECTURE.md this change affects, and
how the tests defend it. Write "none" if it touches none.
-->

## Checklist

- [ ] `just check` passes
- [ ] Tests added or updated; a bug fix has a test that fails before the fix
- [ ] No real `ssh`, real `herdr`, or network access in tests
- [ ] No `unwrap()`/`expect()` outside tests
- [ ] User-facing changes update `README.md`, `README.ja.md`, and `CHANGELOG.md`
