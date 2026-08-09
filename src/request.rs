//! The record that lets "decide to forward" and "actually forward" run on
//! different machines.
//!
//! Under herdr-mirror both happen locally and this is barely more than a
//! function call. Under `herdr --remote` the plugin runs on the remote host and
//! writes here; the local agent reads it over SSH and applies it. Same record,
//! same semantics, only the transport differs.
//!
//! Requests are therefore a stable serialized format, not an internal detail:
//! a newer plugin can be talking to an older local agent. Unknown fields are
//! ignored rather than fatal, and every request carries the plugin version that
//! wrote it.
//!
//! Requests are also idempotent and self-expiring: the agent may read the same
//! file twice, and a request nobody consumed must not be applied hours later.
