//! `mastoot accounts` — list every logged-in account.

use anyhow::Result;

use crate::cli::Cli;
use crate::config::Config;

pub async fn run(cli: &Cli) -> Result<()> {
    let cfg = Config::load(cli.config.as_deref())?;
    if cfg.accounts.is_empty() {
        println!("no accounts. run `mastoot login` to add one.");
        return Ok(());
    }
    let default = cfg.default_account.as_deref();
    for a in &cfg.accounts {
        let marker = if Some(a.handle.as_str()) == default {
            "*"
        } else {
            " "
        };
        let name = a.display_name.as_deref().unwrap_or(&a.handle);
        println!("{marker} {}   ({name})", a.handle);
    }
    Ok(())
}
