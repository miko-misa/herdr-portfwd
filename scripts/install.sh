#!/usr/bin/env bash
# Fetch the prebuilt herdr-portfwd binary for this platform from GitHub
# Releases, verified against SHA256SUMS. Run by the herdr plugin [[build]] step
# with cwd = plugin root. No cargo fallback: dev installs (herdr plugin link)
# build with `cargo build --release` themselves.
set -euo pipefail

cd "$(dirname "$0")/.."
DEST="target/release/herdr-portfwd"

fail() {
  echo "herdr-portfwd fetch failed: $1" >&2
  echo "to build from source instead: cargo build --release" >&2
  exit 1
}

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || fail "cannot read version from Cargo.toml"

# owner/repo from the git remote: plugins are installed by git clone, so this
# keeps forks fetching their own assets instead of upstream's.
SLUG="$(git config --get remote.origin.url 2>/dev/null |
  sed -n 's#.*[:/]\([^/]*/[^/]*\)\.git$#\1#p; s#.*[:/]\([^/]*/[^/]*\)$#\1#p' | head -1)"
[ -n "$SLUG" ] || fail "cannot derive owner/repo from the git remote"

case "$(uname -s)" in
  Darwin) OS="darwin" ;;
  Linux) OS="linux" ;;
  *) fail "unsupported OS: $(uname -s)" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) ARCH="aarch64" ;;
  x86_64 | amd64) ARCH="x86_64" ;;
  *) fail "unsupported architecture: $(uname -m)" ;;
esac

ASSET="herdr-portfwd-${OS}-${ARCH}"
BASE="https://github.com/${SLUG}/releases/download/v${VERSION}"

# SHA-256 verifier: coreutils `sha256sum` on Linux and recent macOS, `shasum`
# elsewhere. Neither is universal, so probe for both.
if command -v sha256sum >/dev/null 2>&1; then
  sha256_check() { sha256sum -c -; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_check() { shasum -a 256 -c -; }
else
  fail "no SHA-256 tool found: install coreutils (sha256sum) or perl (shasum)"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "fetching ${BASE}/${ASSET}"
curl -fsSL --retry 2 -o "${TMP}/${ASSET}" "${BASE}/${ASSET}" ||
  fail "download failed: ${BASE}/${ASSET}"
curl -fsSL --retry 2 -o "${TMP}/SHA256SUMS" "${BASE}/SHA256SUMS" ||
  fail "download failed: ${BASE}/SHA256SUMS"

# Look the hash up before verifying, so a release missing this asset reports as
# incomplete rather than as a corrupt download: under `pipefail` a no-match grep
# would otherwise fail the pipeline and land on the mismatch message.
EXPECTED="$(grep " ${ASSET}\$" "${TMP}/SHA256SUMS")" ||
  fail "${ASSET} is not listed in SHA256SUMS — the v${VERSION} release looks incomplete"
(cd "$TMP" && printf '%s\n' "$EXPECTED" | sha256_check) ||
  fail "checksum mismatch for ${ASSET}"

mkdir -p "$(dirname "$DEST")"
chmod 755 "${TMP}/${ASSET}"
mv "${TMP}/${ASSET}" "$DEST"
echo "installed $DEST (v${VERSION})"

# Link the CLI so `herdr-portfwd <cmd>` works from a shell and keybindings get
# one stable path that survives plugin updates.
if [ -d "$HOME/.local/bin" ] || mkdir -p "$HOME/.local/bin" 2>/dev/null; then
  ln -sf "$(pwd)/$DEST" "$HOME/.local/bin/herdr-portfwd"
  echo "linked $HOME/.local/bin/herdr-portfwd"
fi
