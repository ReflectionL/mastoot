//! mastoot — a Mastodon TUI client.
//!
//! Entry point. Parses CLI args, loads config, dispatches to subcommands.

use anyhow::Result;
use clap::Parser;

use mastoot::cli::{Cli, Command};
use mastoot::{commands, logging};

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();
    let _guard = logging::init(cli.verbose)?;

    let command = cli.command.take().unwrap_or(Command::Run);
    match command {
        Command::Run => commands::run::run(&cli).await,
        Command::Login(args) => commands::login::run(&cli, args).await,
        Command::Logout(args) => commands::logout::run(&cli, args).await,
        Command::Whoami => commands::whoami::run(&cli).await,
        Command::Accounts => commands::accounts::run(&cli).await,
        Command::Switch(args) => commands::switch::run(&cli, args).await,
    }
}
