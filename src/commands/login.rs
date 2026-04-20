//! `mastoot login` — interactive OAuth flow.

use anyhow::Result;

use crate::api::{MastodonClient, auth};
use crate::cli::{Cli, LoginArgs};
use crate::config::{self, AccountRef, Config};

pub async fn run(cli: &Cli, args: LoginArgs) -> Result<()> {
    let mut cfg = Config::load(cli.config.as_deref())?;
    let instance = cfg.effective_instance(cli.instance.as_deref());

    println!("Authorizing mastoot against https://{instance} …");
    let outcome = auth::login(&instance, &mut cfg, args.no_browser).await?;

    // Re-query the account with the fresh token so we cache a handle and
    // display name in config.
    let client = MastodonClient::new(&instance, outcome.token.clone())?;
    let me = client.verify_credentials().await?;

    let handle = format!("{}@{instance}", me.username);
    config::store_token(&handle, &outcome.token)?;

    // Replace any prior entry for this handle.
    cfg.accounts.retain(|a| a.handle != handle);
    cfg.accounts.push(AccountRef {
        handle: handle.clone(),
        instance: instance.clone(),
        account_id: me.id.to_string(),
        display_name: Some(me.display_name.clone()),
    });
    if cfg.default_account.is_none() {
        cfg.default_account = Some(handle.clone());
    }
    if cfg.default_instance.is_none() {
        cfg.default_instance = Some(instance.clone());
    }
    cfg.save(cli.config.as_deref())?;

    println!(
        "✓ signed in as @{} ({}). Token stored in keyring; config updated.",
        me.username, me.display_name
    );
    Ok(())
}
