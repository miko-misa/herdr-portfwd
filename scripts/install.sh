#!/usr/bin/env bash
# Fetch the prebuilt herdr-portfwd binary for this platform from GitHub
# Releases, verified against SHA256SUMS. Run by the herdr plugin [[build]] step
# with cwd = plugin root. No cargo fallback: dev installs (herdr plugin link)
# build with `cargo build --release` themselves.
set -euo pipefail

cd "$(dirname "$0")/.."
DEST="target/release/herdr-portfwd"
# Staged beside the destination rather than left in $TMPDIR, so the binary is
# proved on the filesystem it will actually be executed from, and so a bad
# download never displaces a working install. `$$` keeps two concurrent runs
# off each other's file.
STAGED="target/release/.herdr-portfwd.$$"

fail() {
  echo "herdr-portfwd fetch failed: $1" >&2
  echo "to build from source instead: cargo build --release" >&2
  exit 1
}

note() {
  echo "herdr-portfwd: $1" >&2
}

# Every capture below ends in `|| VAR=""`. Under `set -e -o pipefail` a bare
# `VAR="$(cmd | cmd)"` whose pipeline fails exits the script right there, so the
# guard on the next line — the one that names what was missing — never runs and
# the user gets a silent exit 1. Catching the failure keeps the guard reachable.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml 2>/dev/null | head -1)" ||
  VERSION=""
[ -n "$VERSION" ] || fail "cannot read version from Cargo.toml"

# owner/repo from the git remote: plugins are installed by git clone, so this
# keeps forks fetching their own assets instead of upstream's. The remote is
# read on its own, because outside a git repo `git config` fails and would
# otherwise take the whole script down with it.
REMOTE="$(git config --get remote.origin.url 2>/dev/null)" || REMOTE=""
[ -n "$REMOTE" ] ||
  fail "no git remote 'origin' to derive owner/repo from — install with \`herdr plugin install <owner>/<repo>\`"

# Assets are only ever fetched from github.com. A remote that is not a GitHub
# URL — a local path, a bare clone on a fileserver, another forge — still yields
# a syntactically fine `owner/repo` from the sed below, and the user's reward
# for a mirrored clone would be a 404 naming a repository nobody ever created.
# Name the real problem instead.
remote_host() {
  local url="$1"
  case "$url" in
    *://*) url="${url#*://}" ;; # scheme://[user@]host[:port]/path
    *:*) url="${url%%:*}/" ;;   # scp-like [user@]host:path
  esac
  url="${url#*@}" # drop any userinfo
  printf '%s' "${url%%[/:]*}"
}
[ "$(remote_host "$REMOTE")" = "github.com" ] ||
  fail "git remote 'origin' is not a github.com URL, so it names no release to fetch: ${REMOTE}"

SLUG="$(printf '%s\n' "$REMOTE" |
  sed -n 's#/*$##
    s#.*[:/]\([^/]*/[^/]*\)\.git$#\1#p
    s#.*[:/]\([^/]*/[^/]*\)$#\1#p' | head -1)" || SLUG=""
# Shape-check rather than merely non-empty: a degenerate remote can leave an
# empty owner or an embedded slash, which builds a URL that 404s for a reason
# the message would not explain.
[[ "$SLUG" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] ||
  fail "cannot derive owner/repo from the git remote: ${REMOTE}"

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

command -v curl >/dev/null 2>&1 ||
  fail "curl not found: install curl, or build from source"

# SHA-256 verifier: coreutils `sha256sum` on Linux and recent macOS, `shasum`
# elsewhere. Neither is universal, so probe for both.
if command -v sha256sum >/dev/null 2>&1; then
  sha256_check() { sha256sum -c -; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_check() { shasum -a 256 -c -; }
else
  fail "no SHA-256 tool found: install coreutils (sha256sum) or perl (shasum)"
fi

TMP="$(mktemp -d)" || fail "cannot create a temporary directory"
trap 'rm -rf "$TMP"; rm -f "$STAGED"' EXIT

echo "fetching ${BASE}/${ASSET}"
curl -fsSL --retry 2 -o "${TMP}/${ASSET}" "${BASE}/${ASSET}" ||
  fail "download failed: ${BASE}/${ASSET}"
curl -fsSL --retry 2 -o "${TMP}/SHA256SUMS" "${BASE}/SHA256SUMS" ||
  fail "download failed: ${BASE}/SHA256SUMS"

# Look the hash up before verifying, so a release missing this asset reports as
# incomplete rather than as a corrupt download: under `pipefail` a no-match grep
# would otherwise fail the pipeline and land on the mismatch message.
#
# grep's 1 (no match) and its 2 (could not read the file, bad pattern) mean
# opposite things: one says the release is incomplete, the other says the
# lookup itself never happened. Collapsing them would blame the release for a
# local failure, so branch on the status.
GREP_STATUS=0
EXPECTED="$(grep " ${ASSET}\$" "${TMP}/SHA256SUMS")" || GREP_STATUS=$?
case "$GREP_STATUS" in
  0) ;;
  1) fail "${ASSET} is not listed in SHA256SUMS — the v${VERSION} release looks incomplete" ;;
  *) fail "cannot search SHA256SUMS for ${ASSET} (grep exited ${GREP_STATUS})" ;;
esac
(cd "$TMP" && printf '%s\n' "$EXPECTED" | sha256_check) ||
  fail "checksum mismatch for ${ASSET}"

mkdir -p "$(dirname "$DEST")" || fail "cannot create $(dirname "$DEST")"
# `mv` onto a directory moves *into* it, which would leave the manifest's
# ./target/release/herdr-portfwd pointing at a directory and every action
# failing to start. Refuse instead.
[ ! -d "$DEST" ] || fail "$DEST is a directory; remove it and retry"
mv -f "${TMP}/${ASSET}" "$STAGED" || fail "cannot install $DEST"
chmod 755 "$STAGED" || fail "cannot make ${ASSET} executable"

# The checksum proves the bytes are the ones that were published under this
# name. It says nothing about whether this machine can run them: an asset built
# for another libc, or a release where one platform's binary was staged under
# another platform's name, verifies perfectly and then fails at every single
# invocation with an exec error the user has to decode. Ask the binary its
# version — the cheapest question it answers, and one the loader still has to
# accept it for — before anything is called installed.
"$STAGED" --version >/dev/null 2>&1 ||
  fail "the downloaded ${ASSET} does not run on this machine ($(uname -s) $(uname -m)): it is built for a different architecture or C library, or the v${VERSION} release staged the wrong file under that name"

mv -f "$STAGED" "$DEST" || fail "cannot install $DEST"
echo "installed $DEST (v${VERSION})"

# Put the CLI on PATH so `herdr-portfwd <cmd>` works from a shell and
# keybindings get one stable path that survives plugin updates.
#
# It has to be a *copy*. `herdr plugin install` runs [[build]] with cwd inside a
# temporary checkout, and only afterwards renames that checkout into its managed
# location and deletes the temp root (herdr src/cli/plugin.rs:
# create_plugin_temp_dir("install") → run_plugin_build_commands(…, manifest_root)
# → fs::rename(checkout, final_checkout) → remove_dir_all(temp_root)). A symlink
# to $(pwd) recorded here dangles the moment the install finishes. The managed
# path cannot be computed either: build commands run with every HERDR_* variable
# scrubbed from the environment. A copy is the one thing that survives the move.
CLI_COPY_PROBLEM=""

install_cli_copy() {
  if [ -z "${HOME:-}" ]; then
    CLI_COPY_PROBLEM="HOME is not set"
    return 1
  fi

  local bin_dir="$HOME/.local/bin"
  local target="$bin_dir/herdr-portfwd"
  local staged="$bin_dir/.herdr-portfwd.$$"

  if [ ! -d "$bin_dir" ] && ! mkdir -p "$bin_dir" 2>/dev/null; then
    CLI_COPY_PROBLEM="cannot create $bin_dir"
    return 1
  fi
  if [ -d "$target" ]; then
    CLI_COPY_PROBLEM="$target is a directory"
    return 1
  fi
  if ! cp "$DEST" "$staged" 2>/dev/null || ! chmod 755 "$staged" 2>/dev/null; then
    CLI_COPY_PROBLEM="cannot write to $bin_dir"
    rm -f "$staged" 2>/dev/null || true
    return 1
  fi
  # Rename into place rather than copying over the destination: a herdr-portfwd
  # that is running right now keeps its own inode instead of the write failing
  # with ETXTBSY, and a reader never sees a half-written binary.
  if ! mv -f "$staged" "$target" 2>/dev/null; then
    CLI_COPY_PROBLEM="cannot replace $target"
    rm -f "$staged" 2>/dev/null || true
    return 1
  fi
  echo "installed $target"
}

# Wholly best-effort, and deliberately the last thing that runs. Herdr treats
# any non-zero [[build]] as a failed install and does not register the plugin at
# all, so this convenience must never decide the exit status: the binary above
# is already fetched, verified and in place.
if ! install_cli_copy; then
  note "could not install the CLI on PATH: ${CLI_COPY_PROBLEM}"
  note "the plugin is installed and works from inside Herdr; only the"
  note "\`herdr-portfwd\` shell command is missing. To get it, fix that path"
  note "and re-run: herdr plugin install ${SLUG}"
fi

exit 0
