//! `mastoot run` — launch the TUI.
//!
//! Zero-config flow: if no token is stored yet, we transparently kick
//! off the OAuth login wizard before entering raw-mode. After the user
//! authorizes, credentials are persisted in keyring + config, the TUI
//! starts.

use anyhow::{Context, Result};

use crate::api::{MastodonClient, auth};
use crate::cli::Cli;
use crate::config::{self, AccountRef, Config};
use crate::ui;

pub async fn run(cli: &Cli) -> Result<()> {
    let mut cfg = Config::load(cli.config.as_deref())?;
    let instance = cfg.effective_instance(cli.instance.as_deref());

    let handle = resolve_or_login(cli, &mut cfg, &instance).await?;
    let token = config::load_token(&handle)
        .with_context(|| format!("credentials for {handle} not found in keyring"))?;

    let client = MastodonClient::new(&instance, token)?;
    // Boxed because the TUI future is large (carries App + screen state +
    // tokio internals); without this, clippy::large_futures fires.
    Box::pin(ui::run(client, cfg)).await
}

/// Returns the fully-qualified `user@instance` handle to use. Runs the
/// OAuth flow if no credential is cached for this instance.
async fn resolve_or_login(cli: &Cli, cfg: &mut Config, instance: &str) -> Result<String> {
    // If the user already has a default_account matching the current
    // instance, use it. Otherwise pick any account on this instance.
    if let Some(h) = cfg
        .default_account
        .clone()
        .filter(|h| h.ends_with(&format!("@{instance}")))
    {
        return Ok(h);
    }
    if let Some(a) = cfg.accounts.iter().find(|a| a.instance == instance) {
        return Ok(a.handle.clone());
    }

    // Nothing cached — run the interactive login flow.
    eprintln!("No credentials for {instance}. Launching OAuth login…");
    let outcome = auth::login(instance, cfg, false).await?;
    let client = MastodonClient::new(instance, outcome.token.clone())?;
    let me = client.verify_credentials().await?;
    let handle = format!("{}@{instance}", me.username);
    config::store_token(&handle, &outcome.token)?;

    cfg.accounts.retain(|a| a.handle != handle);
    cfg.accounts.push(AccountRef {
        handle: handle.clone(),
        instance: instance.to_string(),
        account_id: me.id.to_string(),
        display_name: Some(me.display_name.clone()),
    });
    if cfg.default_account.is_none() {
        cfg.default_account = Some(handle.clone());
    }
    if cfg.default_instance.is_none() {
        cfg.default_instance = Some(instance.to_string());
    }
    cfg.save(cli.config.as_deref())?;
    eprintln!("✓ signed in as {handle}. Entering TUI…");

    Ok(handle)
}
