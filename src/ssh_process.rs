//! The real [`crate::ssh::SshRunner`], driving the `ssh` binary.
//!
//! Master options mirror what Herdr itself uses for remote attach:
//! `ControlMaster=auto`, `ControlPersist=yes`, plus `BatchMode=yes` so a host
//! that would prompt fails fast instead of hanging a plugin action.
//!
//! Two behaviors are load-bearing:
//! - `-O cancel` for a forward that does not exist must succeed. Reconciliation
//!   calls it speculatively (invariant 4).
//! - the `-L` string given to `cancel` must be byte-identical to the one given
//!   to `forward`, or OpenSSH matches nothing and reports success anyway. Both
//!   come from [`crate::types::ForwardSpec::forward_arg`].
//!
//! Every invocation is an argv array. Nothing user-controlled is ever assembled
//! into a shell command line.
