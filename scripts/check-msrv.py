#!/usr/bin/env python3
"""Fail when the declared MSRV is lower than a locked dependency requires.

`rust-version` in Cargo.toml is a promise to anyone on an older toolchain, and
nothing enforces it: `cargo build` on current stable succeeds no matter what the
field says. A dependency bump — including one Dependabot opens — can quietly
raise the real floor above the declared one, and the first person to find out is
someone whose build fails on the version the manifest told them was enough.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def version_key(value: str) -> tuple[int, ...]:
    parts = [int(part) for part in value.split(".")[:3]]
    return tuple(parts + [0] * (3 - len(parts)))


def declared_msrv() -> str:
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
    version = manifest.get("package", {}).get("rust-version")
    if not version:
        sys.exit("Cargo.toml has no package.rust-version")
    return version


def highest_dependency_requirement() -> tuple[str, str]:
    """The steepest `rust-version` among locked packages, and who asks for it."""
    metadata = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    required = [
        (package["rust_version"], f"{package['name']} {package['version']}")
        for package in json.loads(metadata.stdout)["packages"]
        if package.get("rust_version")
    ]
    if not required:
        return "0.0.0", "nothing"
    return max(required, key=lambda item: version_key(item[0]))


def main() -> int:
    declared = declared_msrv()
    highest, who = highest_dependency_requirement()
    if version_key(declared) < version_key(highest):
        print(
            f"rust-version = {declared}, but {who} needs {highest}.\n"
            f"Raise rust-version to {highest}, or pin that dependency back.",
            file=sys.stderr,
        )
        return 1
    print(f"ok: declared {declared}, highest dependency requirement {highest} ({who})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
