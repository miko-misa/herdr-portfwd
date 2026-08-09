default: check

# Format, lint, and test — run this before committing.
check: fmt-check clippy test

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test --all-features

build:
    cargo build --release

# Link this working tree into the running Herdr as a plugin.
link: build
    herdr plugin link .
    herdr server reload-config
    herdr plugin action list --plugin portfwd

unlink:
    herdr plugin unlink portfwd
    herdr server reload-config

# Tail this plugin's command log.
logs:
    herdr plugin log list --plugin portfwd
