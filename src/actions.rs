//! The plugin actions, composed over the traits.
//!
//! `scan`, `add`, `list`, `stop`, `stop-all`, `open-url`, `reconcile`,
//! `status`. Each resolves a host from the invocation context, then either
//! applies a forward directly (server is local) or writes a request for the
//! local agent (server is remote).
//!
//! Rules these share, from `ARCHITECTURE.md`:
//! - the local port equals the remote port unless the user explicitly asked
//!   otherwise; a conflict is reported, never silently remapped;
//! - `open-url` accepts loopback hosts only, and re-validates the URL rather
//!   than trusting that the manifest pattern already did;
//! - nothing is forwarded without an explicit action, an accepted offer, or an
//!   allowlist match.
