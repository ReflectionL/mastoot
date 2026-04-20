//! Phase-1 acceptance example.
//!
//! Runs the full login flow if no token is cached, then fetches the first
//! 10 statuses on the user's home timeline and prints them as plain
//! text — no ratatui involved.
//!
//! Usage:
//! ```bash
//! cargo run --example fetch_home
//! cargo run --example fetch_home -- --instance mastodon.social
//! ```

use anyhow::{Context, Result};
use clap::Parser;
use mastoot::api::{MastodonClient, auth, endpoints::TimelineParams, html};
use mastoot::config::{self, AccountRef, Config};

#[derive(Parser)]
struct Args {
    /// Instance hostname. Overrides the config's default.
    #[arg(long)]
    instance: Option<String>,
    /// Force a fresh OAuth flow even if a token is already stored.
    #[arg(long)]
    force_login: bool,
    /// How many statuses to print.
    #[arg(long, default_value_t = 10)]
    limit: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,mastoot=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let mut cfg = Config::load(None).context("loading config")?;
    let instance = cfg.effective_instance(args.instance.as_deref());

    // 1. Obtain a token — from keyring or by running the OAuth flow.
    let token = if args.force_login {
        run_login(&instance, &mut cfg).await?
    } else if let Some(handle) = cfg
        .default_account
        .clone()
        .filter(|h| h.ends_with(&format!("@{instance}")))
    {
        println!("Using stored credentials for {handle}");
        config::load_token(&handle)?
    } else {
        run_login(&instance, &mut cfg).await?
    };

    // 2. Fetch home timeline.
    let client = MastodonClient::new(&instance, token)?;
    let me = client.verify_credentials().await?;
    println!("\n✓ signed in as @{} on {}", me.username, instance);

    let params = TimelineParams {
        limit: Some(args.limit),
        ..Default::default()
    };
    let page = client.home_timeline(&params).await?;

    println!("\n── home timeline (first {}): ──\n", page.items.len());
    for (i, status) in page.items.iter().enumerate() {
        // Boosts: show the inner reblog but prefix a line.
        let (account, content, url) = if let Some(reblog) = &status.reblog {
            (
                &reblog.account,
                reblog.content.as_str(),
                reblog.url.as_deref(),
            )
        } else {
            (
                &status.account,
                status.content.as_str(),
                status.url.as_deref(),
            )
        };

        println!(
            "[{:>2}] {} @{}  ·  {}",
            i + 1,
            account.display_name,
            account.acct,
            status
                .created_at
                .map(|t| t.format("%b %d %H:%M").to_string())
                .unwrap_or_default(),
        );
        if status.reblog.is_some() {
            println!("     (boosted by @{})", status.account.acct);
        }
        if !status.spoiler_text.is_empty() {
            println!("     CW: {}", status.spoiler_text);
        }
        let plain = html::to_plain_text(content);
        for line in plain.lines() {
            println!("     {line}");
        }
        if !status.media_attachments.is_empty() {
            for m in &status.media_attachments {
                let desc = m.description.as_deref().unwrap_or("[no alt text]");
                println!("     [{:?}] {}", m.media_type, desc);
            }
        }
        if let Some(u) = url {
            println!("     {u}");
        }
        println!();
    }

    Ok(())
}

async fn run_login(instance: &str, cfg: &mut Config) -> Result<secrecy::SecretString> {
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
    cfg.save(None)?;
    println!("✓ signed in as {handle}; token stored in keyring.");
    Ok(outcome.token)
}
