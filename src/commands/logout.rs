//! `mastoot logout` — remove cached credentials.

use anyhow::Result;

use crate::cli::{Cli, LogoutArgs};
use crate::config::{self, Config};

pub async fn run(cli: &Cli, args: LogoutArgs) -> Result<()> {
    let mut cfg = Config::load(cli.config.as_deref())?;
    let instance = cfg.effective_instance(cli.instance.as_deref());

    let handles: Vec<String> = cfg
        .accounts
        .iter()
        .filter(|a| a.instance == instance)
        .map(|a| a.handle.clone())
        .collect();

    if handles.is_empty() {
        println!("No accounts stored for {instance}.");
        return Ok(());
    }

    for handle in &handles {
        config::delete_token(handle)?;
        println!("✓ forgot {handle}");
    }

    cfg.accounts.retain(|a| a.instance != instance);
    if cfg
        .default_account
        .as_ref()
        .is_some_and(|d| handles.iter().any(|h| h == d))
    {
        cfg.default_account = None;
    }
    if args.purge {
        cfg.apps.remove(&instance);
    }
    cfg.save(cli.config.as_deref())?;

    Ok(())
}
