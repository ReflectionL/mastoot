//! `mastoot switch <handle>` — change the default account for the TUI.

use anyhow::{Context, Result, anyhow};

use crate::cli::{Cli, SwitchArgs};
use crate::config::{self, Config};

pub async fn run(cli: &Cli, args: SwitchArgs) -> Result<()> {
    let mut cfg = Config::load(cli.config.as_deref())?;
    let handle = &args.handle;

    let account = cfg
        .accounts
        .iter()
        .find(|a| a.handle == *handle)
        .cloned()
        .ok_or_else(|| {
            let known: Vec<&str> = cfg.accounts.iter().map(|a| a.handle.as_str()).collect();
            anyhow!(
                "unknown handle `{handle}`. known: {}",
                if known.is_empty() {
                    "(none — run `mastoot login`)".to_string()
                } else {
                    known.join(", ")
                }
            )
        })?;

    // Sanity check: the keyring entry must still exist. If the user
    // cleared Keychain manually we can't silently claim success.
    config::load_token(handle).with_context(|| {
        format!(
            "no keyring entry for {handle}; re-run `mastoot login --instance {}`",
            account.instance
        )
    })?;

    cfg.default_account = Some(handle.clone());
    cfg.default_instance = Some(account.instance.clone());
    cfg.save(cli.config.as_deref())?;

    let name = account.display_name.as_deref().unwrap_or(handle);
    println!("✓ default account → {handle} ({name})");
    Ok(())
}
