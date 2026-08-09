//! Command-line surface.
//!
//! This is a contract in two directions: `herdr-plugin.toml` invokes these
//! subcommands, and people run them from a shell. Renaming one is a breaking
//! change to the manifest.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "herdr-portfwd",
    version,
    about = "Forward dev-server ports from remote Herdr hosts to this machine",
    long_about = None,
)]
pub struct Cli {
    /// Host to act on. Defaults to the host behind the invoking pane, then to
    /// the configured default host.
    #[arg(long, global = true, env = "HERDR_PORTFWD_HOST")]
    pub host: Option<String>,

    /// Print machine-readable JSON instead of human output.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the local agent, then hand off to `herdr --remote <host>`.
    ///
    /// Needed because under `--remote` the Herdr server, and therefore every
    /// plugin, runs on the remote host and cannot open a port here.
    Attach {
        /// Host to attach to.
        host: String,
        /// Extra arguments passed through to `herdr --remote`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        herdr_args: Vec<String>,
    },

    /// Run the local half: poll for requests and apply forwards. Normally
    /// started by `attach` rather than invoked directly.
    LocalAgent {
        /// Host to serve.
        host: String,
        /// Exit when the master goes away instead of retrying.
        #[arg(long)]
        oneshot: bool,
    },

    /// List remote listeners that belong to a Herdr pane, and forward the
    /// chosen ones.
    Scan,

    /// Forward a remote port to the same port locally.
    Add {
        /// Remote port. Prompted for when omitted.
        port: Option<u16>,
        /// Bind a different local port. Breaks URLs that embed the port, so it
        /// is never chosen automatically.
        #[arg(long)]
        local_port: Option<u16>,
    },

    /// Show live forwards.
    List,

    /// Stop one forward.
    Stop {
        /// Remote port. Prompted for when omitted.
        port: Option<u16>,
    },

    /// Stop every forward for the selected host.
    StopAll,

    /// Forward the port in a loopback URL, then open it locally. This is the
    /// link-handler target; the URL arrives in `HERDR_PLUGIN_CLICKED_URL`.
    OpenUrl {
        /// URL. Falls back to the clicked-URL environment variable.
        url: Option<String>,
    },

    /// Bring live forwards back in line with the recorded state: drop records
    /// whose pane or host is gone, and re-add ones that vanished.
    Reconcile {
        /// Invoked from the `[[startup]]` hook.
        #[arg(long)]
        startup: bool,
        /// Invoked from an `[[events]]` hook.
        #[arg(long)]
        event: bool,
    },

    /// Render a plugin pane. Entrypoints are declared in `herdr-plugin.toml`.
    PaneUi {
        #[command(subcommand)]
        entrypoint: PaneEntrypoint,
    },

    /// Print hosts, control masters, and live forwards.
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum PaneEntrypoint {
    /// Overlay listing live forwards, with keys to stop them.
    List,
    /// Popup for picking from attributed remote listeners.
    Picker,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn manifest_subcommands_all_parse() {
        // Every argv the plugin manifest can invoke. If one stops parsing, the
        // plugin breaks at runtime with no compile-time signal.
        for argv in [
            vec!["herdr-portfwd", "scan"],
            vec!["herdr-portfwd", "add"],
            vec!["herdr-portfwd", "stop"],
            vec!["herdr-portfwd", "stop-all"],
            vec!["herdr-portfwd", "open-url"],
            vec!["herdr-portfwd", "reconcile", "--startup"],
            vec!["herdr-portfwd", "reconcile", "--event"],
            vec!["herdr-portfwd", "pane-ui", "list"],
            vec!["herdr-portfwd", "pane-ui", "picker"],
        ] {
            Cli::try_parse_from(&argv)
                .unwrap_or_else(|err| panic!("manifest argv {argv:?} does not parse: {err}"));
        }
    }

    #[test]
    fn attach_passes_trailing_args_through_to_herdr() {
        let cli = Cli::try_parse_from(["herdr-portfwd", "attach", "buildbox", "--session", "work"])
            .expect("attach with pass-through args should parse");
        match cli.command {
            Command::Attach { host, herdr_args } => {
                assert_eq!(host, "buildbox");
                assert_eq!(herdr_args, ["--session", "work"]);
            }
            other => panic!("expected attach, got {other:?}"),
        }
    }

    #[test]
    fn local_port_remap_must_be_explicit() {
        let cli = Cli::try_parse_from(["herdr-portfwd", "add", "5173"])
            .expect("add with a port should parse");
        match cli.command {
            Command::Add { port, local_port } => {
                assert_eq!(port, Some(5173));
                assert_eq!(local_port, None, "a remap must never be implied");
            }
            other => panic!("expected add, got {other:?}"),
        }
    }
}
