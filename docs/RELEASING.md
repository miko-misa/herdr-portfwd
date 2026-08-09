# Releasing

## Cutting a release

1. Bump the version in **both** `Cargo.toml` and `herdr-plugin.toml`. They must
   match: `scripts/install.sh` reads the crate version and downloads
   `v<version>` assets, so a mismatch means `herdr plugin install` fetches a
   release that does not exist. CI fails the `manifest` job when they drift.
2. Move the `## Unreleased` entries in `CHANGELOG.md` under a new
   `## [x.y.z] - YYYY-MM-DD` heading.
3. `just check`.
4. Commit, then tag and push:

   ```bash
   git tag vx.y.z
   git push origin main --tags
   ```

The `Release` workflow builds four binaries on native runners — no
cross-compilation — checks that the tag matches the crate version, refuses to
publish unless all four assets arrived, writes `SHA256SUMS`, and creates the
GitHub release with `gh`.

Assets, and the names `scripts/install.sh` expects:

```
herdr-portfwd-linux-x86_64     herdr-portfwd-darwin-x86_64
herdr-portfwd-linux-aarch64    herdr-portfwd-darwin-aarch64
SHA256SUMS
```

Renaming one is a breaking change for every existing install: `install.sh`
looks the asset up in `SHA256SUMS` by exact name.

`workflow_dispatch` reruns the workflow against an existing tag, which is the
way to recover a release whose upload failed. It replaces assets rather than
failing on a release that already exists.

## Runner notes

The workflow uses `ubuntu-24.04-arm` and `macos-15-intel`. Both are free only
for **public** repositories; in a private fork the release job needs different
runners or a paid plan.

## Marketplace listing

Herdr's [plugin marketplace](https://herdr.dev/plugins/) is an automatic index
of public GitHub repositories carrying the topic **`herdr-plugin`**. Add that
topic to the repository (Settings, or the About panel's gear icon) and the index
picks it up within about 30 minutes. Nothing needs to be submitted anywhere.

## Version policy

`min_herdr_version` in `herdr-plugin.toml` is the oldest Herdr that has every
API this plugin calls. Herdr refuses to install a plugin whose minimum is newer
than itself, so raising it drops users — only raise it when the plugin actually
starts depending on something newer, and say so in the changelog.
