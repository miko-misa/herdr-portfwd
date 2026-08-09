//! The real [`crate::herdr::HerdrClient`], driving the `herdr` binary.
//!
//! Two flavors of the same client:
//! - local, running `herdr` directly, used when the plugin and the server are
//!   on the same machine;
//! - remote, running `herdr` on the far side over an existing control master,
//!   used by the local agent to inspect the server it is not running on.
//!
//! Herdr's CLI answers with JSON on stdout and JSON errors on stderr with exit
//! status 1; syntax errors exit 2. Identifiers are read out of the responses,
//! never predicted.
