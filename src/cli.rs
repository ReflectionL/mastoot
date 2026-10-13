//! Command-line argument parsing via `clap` derive.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Default instance used when no `--instance` flag is supplied and the
/// config file does not set `default_instance`.
pub const DEFAULT_INSTANCE: &str = "mastodon.social";

/// Top-level CLI. Running `mastoot` with no subcommand enters the TUI.
#[derive(Debug, Parser)]
#[command(
    name = "mastoot",
    version,
    about = "An aesthetically-driven Mastodon TUI client",
    long_about = None,
)]
pub struct Cli {
    /// Instance hostname (overrides config). Example: `mastodon.social`.
    #[arg(long, global = true)]
    pub instance: Option<String>,

    /// Path to config file. Defaults to `~/.config/mastoot/config.toml`.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Increase logging verbosity (-v = debug, -vv = trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// (default) Launch the TUI.
    Run,

    /// Interactively log in to a Mastodon instance via OAuth.
    Login(LoginArgs),

    /// Forget credentials for an instance.
    Logout(LogoutArgs),

    /// Print the currently logged-in account.
    Whoami,

    /// List all logged-in accounts.
    Accounts,

    /// Switch the default account for the TUI.
    Switch(SwitchArgs),

    /// Update mastoot to the latest GitHub release.
    ///
    /// Works for copies installed with the shell installer from the
    /// release page. Homebrew users run `brew upgrade mastoot` instead.
    Update(UpdateArgs),
}

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Skip opening the browser automatically; print the URL instead.
    #[arg(long)]
    pub no_browser: bool,
}

#[derive(Debug, Args)]
pub struct LogoutArgs {
    /// Also delete app registration from config.
    #[arg(long)]
    pub purge: bool,
}

#[derive(Debug, Args)]
pub struct SwitchArgs {
    /// Fully qualified handle, e.g. `alice@mastodon.social`. Must
    /// match one of the accounts listed by `mastoot accounts`.
    pub handle: String,
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Only report whether a newer release exists; install nothing.
    #[arg(long)]
    pub check: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_subcommand_parses() {
        let cli = Cli::try_parse_from(["mastoot", "update", "--check"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Update(UpdateArgs { check: true }))
        ));
        let cli = Cli::try_parse_from(["mastoot", "update"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Update(UpdateArgs { check: false }))
        ));
    }
}
