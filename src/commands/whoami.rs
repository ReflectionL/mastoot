//! `mastoot whoami` — print the account that would be used by `run`.

use anyhow::Result;

use crate::cli::Cli;
use crate::config::Config;

pub async fn run(cli: &Cli) -> Result<()> {
    let cfg = Config::load(cli.config.as_deref())?;
    let instance = cfg.effective_instance(cli.instance.as_deref());

    if let Some(handle) = cfg
        .default_account
        .clone()
        .or_else(|| cfg.accounts.first().map(|a| a.handle.clone()))
    {
        println!("{handle} (instance {instance})");
    } else {
        println!("not signed in; default instance is {instance}");
    }
    Ok(())
}
