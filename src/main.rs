use anyhow::{bail, Result};
use clap::Parser;
use herdr_portfwd::cli::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Attach { .. }
        | Command::LocalAgent { .. }
        | Command::Scan
        | Command::Add { .. }
        | Command::List
        | Command::Stop { .. }
        | Command::StopAll
        | Command::OpenUrl { .. }
        | Command::Reconcile { .. }
        | Command::PaneUi { .. }
        | Command::Status => {
            bail!("not implemented yet — this is the repository scaffold")
        }
    }
}
