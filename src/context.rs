//! Invocation context, and resolving it to a host.
//!
//! Herdr passes `HERDR_PLUGIN_CONTEXT_JSON` with workspace, tab, focused pane,
//! agent, selected text, clicked URL, and link handler fields. Shell-friendly
//! duplicates exist as `HERDR_ACTIVE_*`, `HERDR_PLUGIN_CLICKED_URL`, and
//! `HERDR_PLUGIN_LINK_HANDLER_ID`; the JSON is authoritative and the env vars
//! are the fallback, so an action behaves the same however it was invoked.
//!
//! Host resolution order:
//! 1. an explicit `--host`,
//! 2. the mirror prefix on the invoking workspace label (`"mini: scholion"`
//!    resolves to the host whose `prefix` is `mini`),
//! 3. the configured default host,
//! 4. the only configured host, when there is exactly one.
//!
//! Reading the environment happens at the edge; the resolution itself takes
//! values and is pure.
//!
//! This module also answers a second question about the invocation: *how* was
//! this process started — see [`PluginInvocation`].

use serde::Deserialize;

use crate::herdr::BIN_PATH_ENV;
use crate::types::Host;

/// Authoritative context, injected into every plugin command.
pub const CONTEXT_JSON_ENV: &str = "HERDR_PLUGIN_CONTEXT_JSON";

/// Injected for a plugin *action* command, and only for one.
pub const ACTION_ID_ENV: &str = "HERDR_PLUGIN_ACTION_ID";

/// Injected for a plugin *pane* command, and only for one.
pub const ENTRYPOINT_ID_ENV: &str = "HERDR_PLUGIN_ENTRYPOINT_ID";

/// Injected for every plugin command: the id Herdr registered this plugin as.
pub const PLUGIN_ID_ENV: &str = "HERDR_PLUGIN_ID";

/// Separator herdr-mirror puts between a host prefix and the workspace name.
/// The space is part of it: without it, any workspace named `foo:bar` would
/// look like a mirrored one.
const PREFIX_SEPARATOR: &str = ": ";

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("{CONTEXT_JSON_ENV} is not valid context JSON: {0}")]
    Json(String),
    #[error("no hosts are configured; add [hosts.<name>] to hosts.toml")]
    NoHosts,
    #[error("unknown host \"{requested}\"; configured: {configured}")]
    UnknownHost {
        requested: String,
        configured: String,
    },
    #[error("default_host \"{requested}\" is not a configured host; configured: {configured}")]
    UnknownDefaultHost {
        requested: String,
        configured: String,
    },
    #[error("workspace prefix \"{prefix}\" matches more than one host: {matches}")]
    AmbiguousPrefix { prefix: String, matches: String },
    #[error("cannot tell which host to use; configured: {configured}. Pass --host, or set default_host in hosts.toml")]
    AmbiguousHost { configured: String },
}

pub type Result<T> = std::result::Result<T, ContextError>;

/// What this crate needs out of an invocation. Every field is optional because
/// every field is: a global action has no pane, a shell invocation has no
/// clicked URL, and Herdr omits what it does not have.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Invocation {
    pub workspace_id: Option<String>,
    /// `"mini: scholion"` for a mirrored workspace. The route back to a host.
    pub workspace_label: Option<String>,
    pub tab_id: Option<String>,
    #[serde(rename = "focused_pane_id")]
    pub pane_id: Option<String>,
    #[serde(rename = "focused_pane_cwd")]
    pub pane_cwd: Option<String>,
    #[serde(rename = "focused_pane_agent")]
    pub agent: Option<String>,
    /// Set for a link-handler invocation; `open-url` re-validates it anyway.
    pub clicked_url: Option<String>,
    pub link_handler_id: Option<String>,
    /// `"link_click"`, `"action"`, ... Informational.
    pub invocation_source: Option<String>,
}

impl Invocation {
    /// The mirror prefix on this invocation's workspace label, if it has one.
    pub fn workspace_prefix(&self) -> Option<&str> {
        self.workspace_label
            .as_deref()
            .and_then(workspace_label_prefix)
    }

    /// Fill empty fields from the shell-friendly duplicates. Only the fields
    /// the JSON did not carry, so the JSON stays authoritative.
    fn fill_from<F>(&mut self, lookup: &F)
    where
        F: Fn(&str) -> Option<String>,
    {
        // `HERDR_ACTIVE_*` come from keybinding-launched commands and
        // `HERDR_*` from plugin commands; a plugin command also gets the JSON,
        // so in practice the second group only matters when the JSON is
        // somehow absent. Reading both costs nothing and removes a mode where
        // the same action sees less than it could.
        let fill = |slot: &mut Option<String>, names: &[&str]| {
            if slot.is_some() {
                return;
            }
            *slot = names
                .iter()
                .find_map(|name| lookup(name).and_then(nonempty));
        };
        fill(
            &mut self.workspace_id,
            &["HERDR_ACTIVE_WORKSPACE_ID", "HERDR_WORKSPACE_ID"],
        );
        fill(&mut self.tab_id, &["HERDR_ACTIVE_TAB_ID", "HERDR_TAB_ID"]);
        fill(
            &mut self.pane_id,
            &["HERDR_ACTIVE_PANE_ID", "HERDR_PANE_ID"],
        );
        fill(&mut self.pane_cwd, &["HERDR_ACTIVE_PANE_CWD"]);
        fill(&mut self.clicked_url, &["HERDR_PLUGIN_CLICKED_URL"]);
        fill(&mut self.link_handler_id, &["HERDR_PLUGIN_LINK_HANDLER_ID"]);
    }

    fn normalize(&mut self) {
        for slot in [
            &mut self.workspace_id,
            &mut self.workspace_label,
            &mut self.tab_id,
            &mut self.pane_id,
            &mut self.pane_cwd,
            &mut self.agent,
            &mut self.clicked_url,
            &mut self.link_handler_id,
            &mut self.invocation_source,
        ] {
            *slot = slot.take().and_then(nonempty);
        }
    }
}

fn nonempty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

/// The prefix in a mirrored workspace label. `None` when the label carries no
/// `"<prefix>: "` at all, so a plain workspace name resolves nothing.
///
/// Only the first separator counts: a mirrored workspace whose *name* contains
/// `": "` is still one workspace of one host.
pub fn workspace_label_prefix(label: &str) -> Option<&str> {
    let (prefix, name) = label.split_once(PREFIX_SEPARATOR)?;
    if prefix.trim().is_empty() || name.trim().is_empty() {
        return None;
    }
    Some(prefix)
}

/// Parse `HERDR_PLUGIN_CONTEXT_JSON`. Unknown fields are ignored: Herdr adds
/// context fields between releases and a plugin must not break on one.
pub fn parse_context(json: &str) -> Result<Invocation> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|err| ContextError::Json(err.to_string()))?;
    // Serde would happily read a struct out of a JSON array, so anything but an
    // object is rejected here rather than becoming a silently empty context.
    if !value.is_object() {
        return Err(ContextError::Json("expected a JSON object".to_string()));
    }
    let mut context: Invocation =
        serde_json::from_value(value).map_err(|err| ContextError::Json(err.to_string()))?;
    context.normalize();
    Ok(context)
}

/// Build the context from a variable lookup. The whole environment-facing
/// surface, kept as one function so tests need no process-global state.
pub fn from_lookup<F>(lookup: F) -> Result<Invocation>
where
    F: Fn(&str) -> Option<String>,
{
    let mut context = match lookup(CONTEXT_JSON_ENV).and_then(nonempty) {
        // Malformed JSON is loud rather than silently downgraded to the env
        // vars: Herdr always writes valid JSON, so a broken value means
        // something upstream is wrong and quietly acting on a thinner context
        // would just move the failure somewhere less obvious.
        Some(json) => parse_context(&json)?,
        None => Invocation::default(),
    };
    context.fill_from(&lookup);
    Ok(context)
}

/// The edge: the only function here that reads the process environment.
pub fn from_env() -> Result<Invocation> {
    from_lookup(|name| std::env::var(name).ok())
}

// ---------------------------------------------------------------------------
// how this process was started
// ---------------------------------------------------------------------------

/// What Herdr started, as far as the injected environment says.
///
/// Herdr injects `HERDR_SOCKET_PATH`, `HERDR_PLUGIN_ID` and `HERDR_BIN_PATH`
/// into every plugin command; those say *that* a plugin command is running,
/// not which kind. The kind comes from the two variables that are specific to
/// one: `HERDR_PLUGIN_ACTION_ID` for an action, `HERDR_PLUGIN_ENTRYPOINT_ID`
/// for a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invoked {
    /// A plugin action: headless, with nobody to prompt, and a Herdr on the
    /// other end that can open a pane on its behalf.
    Action,
    /// A plugin pane: Herdr already gave this process a pane to draw in.
    Pane,
    /// A startup or event hook, or a plain shell invocation. Neither has a
    /// person waiting on a pane.
    Other,
}

/// The injected variables that say how this process was started.
///
/// A value rather than a set of `std::env` calls so the decision below is a
/// pure function of it: the alternative is an `is_terminal()`-shaped branch
/// that no test can reach, which is exactly how the actions this answers for
/// came to be dead in the first place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginInvocation {
    pub action_id: Option<String>,
    pub entrypoint_id: Option<String>,
    pub plugin_id: Option<String>,
    /// `HERDR_BIN_PATH`: the `herdr` to talk back through, and the evidence
    /// that there is one at all.
    pub herdr_bin: Option<String>,
}

impl PluginInvocation {
    /// Build from a variable lookup, so tests need no process-global state.
    /// Blank values read as absent: an exported-but-empty variable is not a
    /// statement that this is an action.
    pub fn from_lookup<F>(lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let read = |name: &str| lookup(name).and_then(nonempty);
        Self {
            action_id: read(ACTION_ID_ENV),
            entrypoint_id: read(ENTRYPOINT_ID_ENV),
            plugin_id: read(PLUGIN_ID_ENV),
            herdr_bin: read(BIN_PATH_ENV),
        }
    }

    /// The edge: the only function here that reads the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Which kind of command this is.
    ///
    /// A pane wins over an action when both variables are somehow present.
    /// The consequence of getting that order wrong is a pane whose command
    /// asks for the same pane, whose command asks again.
    pub fn kind(&self) -> Invoked {
        if self.entrypoint_id.is_some() {
            return Invoked::Pane;
        }
        if self.action_id.is_some() {
            return Invoked::Action;
        }
        Invoked::Other
    }

    /// The plugin id to name in `plugin pane open --plugin <id>` when this
    /// invocation was not told enough and should ask Herdr for a pane, or
    /// `None` when it must not ask.
    ///
    /// Three conditions, in this order:
    ///
    /// 1. **Only an action asks.** A pane must not (see [`Self::kind`]); a
    ///    startup or event hook has no user waiting on it; and a shell
    ///    invocation gets the refusal, which is a sentence a person can act
    ///    on, where a pane opening in some other window would not be.
    /// 2. **There has to be a Herdr to ask.** `HERDR_BIN_PATH` names it.
    ///    Without the injected path there is nothing to talk back through —
    ///    [`crate::herdr::herdr_bin`]'s fall back to a bare `herdr` on `PATH`
    ///    is for a person's shell, not for guessing that a server exists.
    /// 3. **The plugin has to be named.** Herdr's own id wins, so a plugin
    ///    registered under some other id still opens its own pane; the
    ///    manifest id is the fallback for the case where Herdr injected an
    ///    action id without one.
    ///
    /// `HERDR_SOCKET_PATH` is deliberately not consulted: everything this
    /// crate says to Herdr goes through the CLI, which is what hides the
    /// platform difference behind that socket, so requiring it would be a
    /// second thing to keep true with nothing depending on it.
    pub fn pane_to_open(&self) -> Option<&str> {
        match (self.kind(), self.herdr_bin.is_some()) {
            (Invoked::Action, true) => Some(self.plugin_id.as_deref().unwrap_or(crate::PLUGIN_ID)),
            _ => None,
        }
    }
}

/// Which rule picked the host. Reported so `status` and log lines can explain
/// a choice the user did not make explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSource {
    /// `--host`, or `HERDR_PORTFWD_HOST`.
    Explicit,
    /// The mirror prefix on the invoking workspace label.
    WorkspacePrefix,
    /// `default_host` in `hosts.toml`.
    Default,
    /// Only one host is configured, so there is nothing to disambiguate.
    OnlyHost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHost {
    pub host: Host,
    pub source: HostSource,
}

/// Resolve an invocation to a host. Pure: the caller supplies the context and
/// the configured hosts, and gets a decision or an error that names the
/// alternatives.
pub fn resolve_host(
    explicit: Option<&str>,
    context: &Invocation,
    hosts: &[Host],
    default_host: Option<&str>,
) -> Result<ResolvedHost> {
    if hosts.is_empty() {
        return Err(ContextError::NoHosts);
    }
    let configured = || {
        hosts
            .iter()
            .map(|host| host.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };

    if let Some(requested) = explicit.and_then(|name| nonempty(name.to_string())) {
        return match hosts.iter().find(|host| host.name == requested) {
            Some(host) => Ok(ResolvedHost {
                host: host.clone(),
                source: HostSource::Explicit,
            }),
            None => Err(ContextError::UnknownHost {
                requested,
                configured: configured(),
            }),
        };
    }

    if let Some(prefix) = context.workspace_prefix() {
        let mut matches = hosts
            .iter()
            .filter(|host| host.prefix.as_deref() == Some(prefix));
        if let Some(host) = matches.next() {
            let rest: Vec<&str> = matches.map(|host| host.name.as_str()).collect();
            if !rest.is_empty() {
                // Two hosts sharing a prefix make the label ambiguous by
                // construction; guessing would forward to the wrong machine.
                let mut names = vec![host.name.as_str()];
                names.extend(rest);
                return Err(ContextError::AmbiguousPrefix {
                    prefix: prefix.to_string(),
                    matches: names.join(", "),
                });
            }
            return Ok(ResolvedHost {
                host: host.clone(),
                source: HostSource::WorkspacePrefix,
            });
        }
        // A label can contain ": " without being a mirror label, so an
        // unmatched prefix falls through instead of failing.
    }

    if let Some(requested) = default_host.and_then(|name| nonempty(name.to_string())) {
        return match hosts.iter().find(|host| host.name == requested) {
            Some(host) => Ok(ResolvedHost {
                host: host.clone(),
                source: HostSource::Default,
            }),
            None => Err(ContextError::UnknownDefaultHost {
                requested,
                configured: configured(),
            }),
        };
    }

    match hosts {
        [only] => Ok(ResolvedHost {
            host: only.clone(),
            source: HostSource::OnlyHost,
        }),
        _ => Err(ContextError::AmbiguousHost {
            configured: configured(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Verbatim shape of the injected context for a link-handler click.
    const CONTEXT_JSON: &str = r#"{
        "workspace_id": "w1",
        "workspace_label": "mini: scholion",
        "workspace_cwd": "/Users/hiroyuki/projects/scholion",
        "worktree": {"branch": "main"},
        "tab_id": "w1:t1",
        "tab_label": "shell",
        "focused_pane_id": "w1:p1",
        "focused_pane_cwd": "/Users/hiroyuki/projects/scholion",
        "focused_pane_agent": "claude",
        "focused_pane_status": "idle",
        "selected_text": "http://localhost:5173",
        "invocation_source": "link_click",
        "correlation_id": "c1",
        "clicked_url": "http://localhost:5173",
        "link_handler_id": "loopback-url"
    }"#;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn host(name: &str, prefix: Option<&str>) -> Host {
        Host {
            name: name.to_string(),
            target: format!("{name}.example"),
            prefix: prefix.map(str::to_string),
        }
    }

    #[test]
    fn a_real_injected_context_parses_every_field_we_use() {
        let context = parse_context(CONTEXT_JSON).expect("herdr's own JSON must parse");
        assert_eq!(context.workspace_id.as_deref(), Some("w1"));
        assert_eq!(context.workspace_label.as_deref(), Some("mini: scholion"));
        assert_eq!(context.tab_id.as_deref(), Some("w1:t1"));
        assert_eq!(context.pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(
            context.pane_cwd.as_deref(),
            Some("/Users/hiroyuki/projects/scholion")
        );
        assert_eq!(context.agent.as_deref(), Some("claude"));
        assert_eq!(
            context.clicked_url.as_deref(),
            Some("http://localhost:5173")
        );
        assert_eq!(context.link_handler_id.as_deref(), Some("loopback-url"));
        assert_eq!(context.invocation_source.as_deref(), Some("link_click"));
    }

    #[test]
    fn unknown_json_fields_are_ignored_not_fatal() {
        let context = parse_context(r#"{"workspace_id":"w1","invented_in_0_9":{"a":[1]}}"#)
            .expect("a newer Herdr must not break this plugin");
        assert_eq!(context.workspace_id.as_deref(), Some("w1"));
        assert_eq!(context.pane_id, None);
    }

    #[test]
    fn an_empty_json_object_is_a_valid_empty_context() {
        assert_eq!(parse_context("{}").expect("{}"), Invocation::default());
    }

    #[test]
    fn malformed_context_json_is_reported_rather_than_downgraded() {
        for json in ["{", "", "null", "[]", r#"{"workspace_id":3}"#] {
            let err = from_lookup(env(&[
                (CONTEXT_JSON_ENV, json),
                ("HERDR_ACTIVE_WORKSPACE_ID", "w1"),
            ]));
            match json {
                // An unset-looking value is not malformed, it is unset: the
                // env fallback is exactly the case that covers.
                "" => assert_eq!(
                    err.expect("empty means absent").workspace_id.as_deref(),
                    Some("w1")
                ),
                _ => {
                    assert!(
                        matches!(err, Err(ContextError::Json(_))),
                        "should reject: {json:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn env_vars_stand_in_when_no_context_json_was_injected() {
        let context = from_lookup(env(&[
            ("HERDR_ACTIVE_WORKSPACE_ID", "w1"),
            ("HERDR_ACTIVE_TAB_ID", "w1:t1"),
            ("HERDR_ACTIVE_PANE_ID", "w1:p1"),
            ("HERDR_ACTIVE_PANE_CWD", "/Users/h/p"),
            ("HERDR_PLUGIN_CLICKED_URL", "http://127.0.0.1:5173/"),
            ("HERDR_PLUGIN_LINK_HANDLER_ID", "loopback-url"),
        ]))
        .expect("env-only invocation");
        assert_eq!(context.workspace_id.as_deref(), Some("w1"));
        assert_eq!(context.tab_id.as_deref(), Some("w1:t1"));
        assert_eq!(context.pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(context.pane_cwd.as_deref(), Some("/Users/h/p"));
        assert_eq!(
            context.clicked_url.as_deref(),
            Some("http://127.0.0.1:5173/")
        );
        assert_eq!(context.link_handler_id.as_deref(), Some("loopback-url"));
    }

    #[test]
    fn the_context_json_wins_over_the_env_duplicates() {
        let context = from_lookup(env(&[
            (CONTEXT_JSON_ENV, CONTEXT_JSON),
            ("HERDR_ACTIVE_WORKSPACE_ID", "stale"),
            ("HERDR_ACTIVE_PANE_ID", "stale:p9"),
            ("HERDR_PLUGIN_CLICKED_URL", "http://localhost:1/"),
        ]))
        .expect("both sources present");
        assert_eq!(context.workspace_id.as_deref(), Some("w1"));
        assert_eq!(context.pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(
            context.clicked_url.as_deref(),
            Some("http://localhost:5173")
        );
    }

    #[test]
    fn fields_missing_from_the_json_still_come_from_the_environment() {
        let context = from_lookup(env(&[
            (CONTEXT_JSON_ENV, r#"{"workspace_id":"w1"}"#),
            ("HERDR_ACTIVE_PANE_ID", "w1:p7"),
            ("HERDR_PLUGIN_CLICKED_URL", "http://localhost:5173/"),
        ]))
        .expect("partial json");
        assert_eq!(context.workspace_id.as_deref(), Some("w1"));
        assert_eq!(context.pane_id.as_deref(), Some("w1:p7"));
        assert_eq!(
            context.clicked_url.as_deref(),
            Some("http://localhost:5173/")
        );
    }

    #[test]
    fn blank_values_from_either_source_read_as_absent() {
        let context = from_lookup(env(&[
            (
                CONTEXT_JSON_ENV,
                r#"{"workspace_id":"","focused_pane_id":"  "}"#,
            ),
            ("HERDR_ACTIVE_PANE_ID", ""),
        ]))
        .expect("blank values");
        assert_eq!(context, Invocation::default());
    }

    #[test]
    fn a_bare_shell_invocation_yields_an_empty_context() {
        assert_eq!(
            from_lookup(env(&[])).expect("no herdr env at all"),
            Invocation::default()
        );
    }

    // -- how this process was started -----------------------------------------

    /// One row of the invocation table: what it is, the variables Herdr
    /// injected, the kind it must read as, and the plugin whose pane it may
    /// ask for.
    type InvocationCase<'a> = (&'a str, &'a [(&'a str, &'a str)], Invoked, Option<&'a str>);

    #[test]
    fn how_herdr_started_this_process_is_read_off_the_injected_variables() {
        // The manifest declares `add` and `stop` with no port, so what a
        // headless one of them does about that is decided entirely here. Every
        // row is an environment Herdr can actually produce.
        const BIN: &str = "/opt/herdr/bin/herdr";
        let cases: &[InvocationCase<'_>] = &[
            (
                "a plugin action, which has nobody to prompt and a Herdr to ask",
                &[
                    (ACTION_ID_ENV, "portfwd.add"),
                    (PLUGIN_ID_ENV, "portfwd"),
                    (BIN_PATH_ENV, BIN),
                ],
                Invoked::Action,
                Some("portfwd"),
            ),
            (
                "a pane command, which is already the pane",
                &[
                    (ENTRYPOINT_ID_ENV, "picker"),
                    (PLUGIN_ID_ENV, "portfwd"),
                    (BIN_PATH_ENV, BIN),
                ],
                Invoked::Pane,
                None,
            ),
            (
                "a pane command that also carries an action id: still a pane, \
                 or it would ask for the pane it already is",
                &[
                    (ACTION_ID_ENV, "portfwd.add"),
                    (ENTRYPOINT_ID_ENV, "picker"),
                    (PLUGIN_ID_ENV, "portfwd"),
                    (BIN_PATH_ENV, BIN),
                ],
                Invoked::Pane,
                None,
            ),
            (
                "a startup or event hook: a plugin command, but not an action",
                &[
                    ("HERDR_PLUGIN_EVENT", "startup"),
                    (PLUGIN_ID_ENV, "portfwd"),
                    (BIN_PATH_ENV, BIN),
                ],
                Invoked::Other,
                None,
            ),
            (
                "a person in a shell, with no Herdr environment at all",
                &[],
                Invoked::Other,
                None,
            ),
            (
                "an action with no HERDR_BIN_PATH: nothing to talk back through",
                &[(ACTION_ID_ENV, "portfwd.add"), (PLUGIN_ID_ENV, "portfwd")],
                Invoked::Action,
                None,
            ),
            (
                "an action whose HERDR_BIN_PATH was exported empty",
                &[
                    (ACTION_ID_ENV, "portfwd.add"),
                    (PLUGIN_ID_ENV, "portfwd"),
                    (BIN_PATH_ENV, ""),
                ],
                Invoked::Action,
                None,
            ),
            (
                "an action with no plugin id: the manifest id is the fallback",
                &[(ACTION_ID_ENV, "portfwd.add"), (BIN_PATH_ENV, BIN)],
                Invoked::Action,
                Some(crate::PLUGIN_ID),
            ),
            (
                "an action under a plugin id that is not the manifest's",
                &[
                    (ACTION_ID_ENV, "vendor.portfwd.add"),
                    (PLUGIN_ID_ENV, "vendor.portfwd"),
                    (BIN_PATH_ENV, BIN),
                ],
                Invoked::Action,
                Some("vendor.portfwd"),
            ),
            (
                "a blank action id is not a statement that this is an action",
                &[
                    (ACTION_ID_ENV, "   "),
                    (PLUGIN_ID_ENV, "portfwd"),
                    (BIN_PATH_ENV, BIN),
                ],
                Invoked::Other,
                None,
            ),
            (
                "a blank entrypoint id is not a pane either",
                &[
                    (ACTION_ID_ENV, "portfwd.stop"),
                    (ENTRYPOINT_ID_ENV, ""),
                    (PLUGIN_ID_ENV, "portfwd"),
                    (BIN_PATH_ENV, BIN),
                ],
                Invoked::Action,
                Some("portfwd"),
            ),
        ];
        for (what, pairs, kind, pane) in cases {
            let invocation = PluginInvocation::from_lookup(env(pairs));
            assert_eq!(invocation.kind(), *kind, "{what}");
            assert_eq!(invocation.pane_to_open(), *pane, "{what}");
        }
    }

    #[test]
    fn only_a_mirror_style_label_yields_a_prefix() {
        assert_eq!(workspace_label_prefix("mini: scholion"), Some("mini"));
        // ": " inside the workspace name belongs to the name, not the prefix.
        assert_eq!(workspace_label_prefix("mini: notes: 2026"), Some("mini"));
        assert_eq!(workspace_label_prefix("scholion"), None);
        assert_eq!(workspace_label_prefix(""), None);
        assert_eq!(workspace_label_prefix(": scholion"), None);
        assert_eq!(workspace_label_prefix("mini: "), None);
        // No space: mirror always writes one, and a colon alone appears in
        // ordinary names.
        assert_eq!(workspace_label_prefix("mini:scholion"), None);
    }

    fn context_with_label(label: &str) -> Invocation {
        Invocation {
            workspace_label: Some(label.to_string()),
            ..Invocation::default()
        }
    }

    #[test]
    fn an_explicit_host_beats_every_other_signal() {
        let hosts = [host("mini", Some("mini")), host("vps", Some("v"))];
        let resolved = resolve_host(
            Some("vps"),
            &context_with_label("mini: scholion"),
            &hosts,
            Some("mini"),
        )
        .expect("explicit wins");
        assert_eq!(resolved.host.name, "vps");
        assert_eq!(resolved.source, HostSource::Explicit);
    }

    #[test]
    fn an_unknown_explicit_host_names_the_configured_ones() {
        let hosts = [host("mini", Some("mini")), host("vps", Some("v"))];
        let err = resolve_host(Some("typo"), &Invocation::default(), &hosts, None)
            .expect_err("a typo must not silently fall back to a default")
            .to_string();
        assert!(err.contains("typo"), "{err}");
        assert!(err.contains("mini") && err.contains("vps"), "{err}");
    }

    #[test]
    fn a_mirror_label_resolves_to_the_host_behind_the_prefix() {
        let hosts = [host("build-mini", Some("mini")), host("vps", Some("v"))];
        let resolved = resolve_host(None, &context_with_label("mini: scholion"), &hosts, None)
            .expect("the prefix identifies the host");
        assert_eq!(resolved.host.name, "build-mini");
        assert_eq!(resolved.source, HostSource::WorkspacePrefix);
    }

    #[test]
    fn a_prefix_matching_two_hosts_is_an_error_that_names_both() {
        let hosts = [host("a", Some("mini")), host("b", Some("mini"))];
        let err = resolve_host(
            None,
            &context_with_label("mini: scholion"),
            &hosts,
            Some("a"),
        )
        .expect_err("guessing would forward to the wrong machine");
        assert!(
            matches!(&err, ContextError::AmbiguousPrefix { prefix, .. } if prefix == "mini"),
            "{err}"
        );
        let text = err.to_string();
        assert!(text.contains('a') && text.contains('b'), "{text}");
    }

    #[test]
    fn a_label_that_matches_no_prefix_falls_through_to_the_default() {
        let hosts = [host("mini", Some("mini")), host("vps", Some("v"))];
        // "docs: readme" looks like a mirror label but names no host.
        let resolved = resolve_host(
            None,
            &context_with_label("docs: readme"),
            &hosts,
            Some("vps"),
        )
        .expect("fall through, not fail");
        assert_eq!(resolved.host.name, "vps");
        assert_eq!(resolved.source, HostSource::Default);
    }

    #[test]
    fn an_unprefixed_or_empty_label_falls_through() {
        let hosts = [host("mini", Some("mini")), host("vps", Some("v"))];
        for label in ["scholion", "", ": scholion"] {
            let resolved = resolve_host(None, &context_with_label(label), &hosts, Some("mini"))
                .unwrap_or_else(|err| panic!("label {label:?}: {err}"));
            assert_eq!(resolved.source, HostSource::Default);
        }
    }

    #[test]
    fn one_configured_host_needs_no_default() {
        let hosts = [host("only", None)];
        let resolved = resolve_host(None, &Invocation::default(), &hosts, None)
            .expect("nothing to disambiguate");
        assert_eq!(resolved.host.name, "only");
        assert_eq!(resolved.source, HostSource::OnlyHost);
    }

    #[test]
    fn several_hosts_and_no_signal_is_an_actionable_error() {
        let hosts = [host("mini", Some("mini")), host("vps", Some("v"))];
        let err = resolve_host(None, &Invocation::default(), &hosts, None)
            .expect_err("picking one for the user is not allowed");
        assert!(matches!(err, ContextError::AmbiguousHost { .. }));
        let text = err.to_string();
        assert!(text.contains("mini") && text.contains("vps"), "{text}");
        assert!(text.contains("--host"), "must say how to fix it: {text}");
    }

    #[test]
    fn a_default_host_naming_nothing_configured_is_reported() {
        let hosts = [host("mini", Some("mini")), host("vps", Some("v"))];
        let err = resolve_host(None, &Invocation::default(), &hosts, Some("gone"))
            .expect_err("a stale default must be visible");
        assert!(matches!(err, ContextError::UnknownDefaultHost { .. }));
        assert!(err.to_string().contains("gone"));
    }

    #[test]
    fn no_hosts_at_all_says_so_rather_than_naming_an_empty_list() {
        let err = resolve_host(Some("mini"), &Invocation::default(), &[], Some("mini"))
            .expect_err("nothing can be resolved without hosts");
        assert!(matches!(err, ContextError::NoHosts));
        assert!(err.to_string().contains("hosts.toml"));
    }

    #[test]
    fn a_blank_explicit_host_is_treated_as_unset() {
        // `HERDR_PORTFWD_HOST=""` in an inherited environment must not be a
        // request to use a host named "".
        let hosts = [host("only", None)];
        let resolved =
            resolve_host(Some("  "), &Invocation::default(), &hosts, None).expect("blank is unset");
        assert_eq!(resolved.source, HostSource::OnlyHost);
    }

    #[test]
    fn a_host_without_a_prefix_never_matches_a_label() {
        let hosts = [host("a", None), host("b", Some("mini"))];
        let resolved = resolve_host(None, &context_with_label("mini: scholion"), &hosts, None)
            .expect("only the prefixed host can match");
        assert_eq!(resolved.host.name, "b");
    }
}
