//! Host and behavior configuration.
//!
//! Hosts come from `~/.config/herdr-mirror/hosts.toml` when it exists, so
//! herdr-mirror users need no extra setup, and from
//! `~/.config/herdr-portfwd/hosts.toml` otherwise. Both use the same shape;
//! this crate reads only `target` and `prefix` and ignores mirror's own keys.
//!
//! Behavior settings live in `~/.config/herdr-portfwd/config.toml`
//! (`poll_seconds`, `offer_new_listeners`, `require_pane_attribution`, and the
//! opt-in `[[auto]]` allowlist).
//!
//! Parsing must be callable as `parse(&str)` with no I/O so it can be tested
//! against fixtures.
