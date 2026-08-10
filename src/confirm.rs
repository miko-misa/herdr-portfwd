//! What the `confirm` pane is told about the click it is confirming.
//!
//! # Why the environment rather than a file
//!
//! A pane command gets no arguments beyond the argv the manifest declares, so
//! the state it renders — host, ports, whether the local port is free, the
//! owning workspace and agent — has to arrive some other way. Herdr offers two:
//! `herdr plugin pane open --env KEY=VALUE`, and anything the pane can read off
//! a disk both sides agree on.
//!
//! This uses the environment, for four reasons.
//!
//! 1. **It has the lifetime of the pane.** A file needs a name, an owner, and a
//!    deletion, and every one of those is a way for the next click to render
//!    the previous click's URL. The variable exists exactly as long as the
//!    process Herdr spawned for it.
//! 2. **There is no second writer.** Two Ctrl+clicks in quick succession are
//!    two `plugin pane open` calls with two environments, where they would be
//!    one path written twice. This crate already carries one file it has to
//!    lock (`requests.json`); a second one bought nothing.
//! 3. **It works unchanged under `herdr --remote`.** The plugin, the pane and
//!    the variable are all on the far side. A file would be too — but a file
//!    also has to live somewhere the pane can find without being told, and the
//!    state directory rules in `docs/ARCHITECTURE.md` exist precisely because
//!    that question has more than one wrong answer.
//! 4. **Nothing in it is a secret.** It is a loopback URL, a host name, two
//!    port numbers and a workspace label. The URL is already in Herdr's own
//!    injected context as `HERDR_PLUGIN_CLICKED_URL`, so the environment is not
//!    a new place for it to be.
//!
//! The cost is that the payload has to survive one round trip through a
//! `KEY=VALUE` string, so it is JSON on one line: [`ConfirmState::encode`] never
//! emits a newline or a NUL, which is all Herdr's `--env` parser rejects.
//!
//! # What the pane is allowed to believe
//!
//! Nothing, without checking. Under `herdr --remote` the pane runs on the host,
//! and the URL in the payload came off that host's terminal. So the pane
//! re-validates it with [`crate::actions::parse_loopback_url`] before acting,
//! exactly as `open-url` does — the payload is a rendering hint, and the forward
//! is decided from the parsed parts. Invariant 11.

use serde::{Deserialize, Serialize};

/// Variable [`ConfirmState`] travels in.
///
/// Namespaced like the crate's other variables (`HERDR_PORTFWD_HOST`,
/// `HERDR_PORTFWD_MODE`) rather than under `HERDR_`, which is Herdr's own and
/// is partly protected against a plugin setting it.
pub const CONFIRM_ENV: &str = "HERDR_PORTFWD_CONFIRM";

/// The click, as the `confirm` pane will draw it.
///
/// Every field is what the *asking* side knew. `host` is `None` and `problem`
/// is set when the click could not be attributed to a configured host, because
/// a screen that invented one would be answering a question it was never told
/// the answer to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfirmState {
    /// Re-rendered from the validated parts of the click, never the raw text.
    pub url: String,
    pub remote_port: u16,
    /// What would be bound here. Equal to `remote_port`: a click never remaps,
    /// because the port in the text is the whole point.
    pub local_port: u16,
    pub host: Option<String>,
    /// Whether the local port is free. `None` when the asking side was on the
    /// far end of the connection and could not know — the same reason
    /// [`crate::actions::Candidate::local_port_free`] is optional.
    pub local_free: Option<bool>,
    /// Whether a record already covers this host and remote port. `None` when
    /// the asking side could not know, for the same reason `local_free` is
    /// optional: under `herdr --remote` the plugin runs on the host and the
    /// records live on the machine that applies the forward, so the store this
    /// process can read is not the one that would answer the question.
    pub already_forwarded: Option<bool>,
    /// True when the forward will be applied on this machine, false when it
    /// will be recorded for the local agent instead.
    pub applies_here: bool,
    pub workspace: Option<String>,
    pub agent: Option<String>,
    /// The pane the URL was clicked in, carried explicitly: the pane process
    /// sees whatever pane is focused when the popup opens, which is not by
    /// construction the one that printed the URL.
    pub pane_id: Option<String>,
    /// Why this click cannot be acted on, when it cannot. Rendered verbatim.
    pub problem: Option<String>,
}

impl ConfirmState {
    /// Whether there is a forward to offer at all.
    pub fn actionable(&self) -> bool {
        self.problem.is_none() && self.host.is_some()
    }

    /// Whether the local port is held by something that is not this forward.
    /// `local_free == None` is not evidence of a conflict; see the field.
    pub fn port_taken(&self) -> bool {
        self.local_free == Some(false) && self.already_forwarded != Some(true)
    }

    /// One line of JSON, for `--env`.
    pub fn encode(&self) -> std::result::Result<String, String> {
        serde_json::to_string(self).map_err(|err| err.to_string())
    }

    /// The `KEY=VALUE` word `plugin pane open --env` takes. One argv word, so
    /// nothing in it is ever parsed by a shell.
    pub fn env_assignment(&self) -> std::result::Result<String, String> {
        Ok(format!("{CONFIRM_ENV}={}", self.encode()?))
    }

    /// Parse a payload. A malformed one is an error rather than a default: an
    /// empty screen that says nothing is worse than a pane that refuses with a
    /// reason.
    pub fn decode(json: &str) -> std::result::Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(json).map_err(|err| err.to_string())?;
        // Serde reads a struct whose fields all default out of a JSON *array*
        // as happily as out of an object, and would turn `[]` into a click with
        // no URL and no host — a blank screen where the reason should be. The
        // same guard `crate::context::parse_context` needs, for the same
        // reason.
        if !value.is_object() {
            return Err("expected a JSON object".to_string());
        }
        serde_json::from_value(value).map_err(|err| err.to_string())
    }

    /// The edge: read the payload Herdr injected, if there is one. A blank
    /// value reads as absent, matching how every other injected variable in
    /// this crate is treated.
    pub fn from_env() -> std::result::Result<Option<Self>, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// [`Self::from_env`] over an injectable lookup, so tests need no
    /// process-global state.
    pub fn from_lookup<F>(lookup: F) -> std::result::Result<Option<Self>, String>
    where
        F: Fn(&str) -> Option<String>,
    {
        match lookup(CONFIRM_ENV) {
            Some(json) if !json.trim().is_empty() => Self::decode(&json).map(Some),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ConfirmState {
        ConfirmState {
            url: "http://localhost:5173/app?x=1".to_string(),
            remote_port: 5173,
            local_port: 5173,
            host: Some("buildbox".to_string()),
            local_free: Some(true),
            already_forwarded: Some(false),
            applies_here: true,
            workspace: Some("mini: scholion".to_string()),
            agent: Some("claude".to_string()),
            pane_id: Some("w1:p1".to_string()),
            problem: None,
        }
    }

    #[test]
    fn a_payload_survives_the_trip_through_one_env_variable() {
        let encoded = state().encode().expect("encodes");
        assert_eq!(ConfirmState::decode(&encoded).expect("decodes"), state());
    }

    #[test]
    fn an_assignment_is_one_line_with_no_nul() {
        // Herdr's `--env` parser splits on the first `=` and rejects NUL; a
        // newline would not be rejected but would make the argv unreadable in
        // every log that prints it.
        let assignment = state().env_assignment().expect("assignment");
        assert!(assignment.starts_with("HERDR_PORTFWD_CONFIRM="));
        assert!(!assignment.contains('\n'), "{assignment}");
        assert!(!assignment.contains('\0'), "{assignment}");
    }

    #[test]
    fn a_japanese_workspace_label_survives_encoding() {
        let mut wide = state();
        wide.workspace = Some("実験用ワークスペース".to_string());
        let encoded = wide.encode().expect("encodes");
        assert_eq!(ConfirmState::decode(&encoded).expect("decodes"), wide);
    }

    #[test]
    fn a_payload_from_an_older_or_newer_writer_still_loads() {
        // Fields are `#[serde(default)]`: a writer that predates one must not
        // make the pane refuse to draw, and one that adds a field must not
        // either.
        let state = ConfirmState::decode(r#"{"url":"http://localhost:9/","future":true}"#)
            .expect("unknown keys are ignored and missing ones default");
        assert_eq!(state.url, "http://localhost:9/");
        assert_eq!(state.host, None);
    }

    #[test]
    fn a_malformed_payload_is_a_reason_not_an_empty_screen() {
        assert!(ConfirmState::decode("{").is_err());
        assert!(ConfirmState::decode("[]").is_err());
    }

    #[test]
    fn an_absent_or_blank_variable_reads_as_nothing_to_confirm() {
        assert_eq!(ConfirmState::from_lookup(|_| None).expect("absent"), None);
        assert_eq!(
            ConfirmState::from_lookup(|_| Some("   ".to_string())).expect("blank"),
            None
        );
    }

    #[test]
    fn a_click_with_no_host_is_not_actionable() {
        let mut orphan = state();
        orphan.host = None;
        orphan.problem = Some("cannot tell which host to use".to_string());
        assert!(!orphan.actionable());
        assert!(state().actionable());
    }

    #[test]
    fn a_port_held_by_our_own_forward_is_not_a_conflict() {
        // The port reads as taken precisely *because* the forward is already
        // up. Calling that a collision would refuse to open a page that is
        // already reachable.
        let mut live = state();
        live.local_free = Some(false);
        live.already_forwarded = Some(true);
        assert!(!live.port_taken());

        let mut taken = state();
        taken.local_free = Some(false);
        assert!(taken.port_taken());

        // And an unknown answer is not a conflict either.
        let mut unknown = state();
        unknown.local_free = None;
        assert!(!unknown.port_taken());

        // An unknown *forward* is not an excuse to ignore a port this side can
        // see is taken: only our own forward holding it makes it not a
        // collision.
        let mut unknown_forward = state();
        unknown_forward.local_free = Some(false);
        unknown_forward.already_forwarded = None;
        assert!(unknown_forward.port_taken());
    }
}
