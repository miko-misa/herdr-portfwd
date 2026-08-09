//! The local half, for `herdr --remote`.
//!
//! `attach <host>` starts a control master, spawns this, and execs
//! `herdr --remote <host>` so the user's session looks normal. This polls the
//! remote for requests and for new listeners, applies forwards to *its own*
//! master, and offers detections through a Herdr notification.
//!
//! It must survive the remote being briefly unreachable, must not leak masters
//! or forwards when the attach ends, and must never apply a forward the user
//! did not ask for. Detection uses polling because Herdr's manifest event hooks
//! deliberately exclude terminal-output events.
