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
