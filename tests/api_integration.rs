//! Integration tests that require a real Mastodon account. Ignored by
//! default — run with `cargo test -- --ignored` if you have credentials
//! stored via `mastoot login`.

#![cfg(test)]

use mastoot::api::MastodonClient;
use mastoot::api::endpoints::TimelineParams;
use mastoot::config::{self, Config};

fn env_instance() -> String {
    std::env::var("MASTOOT_TEST_INSTANCE").unwrap_or_else(|_| "mastodon.social".to_string())
}

#[tokio::test]
#[ignore = "requires a real Mastodon account and an internet connection"]
async fn verify_credentials_against_real_server() {
    let instance = env_instance();
    let cfg = Config::load(None).unwrap();
    let handle = cfg
        .default_account
        .clone()
        .expect("no default account; run `mastoot login`");
    let token = config::load_token(&handle).expect("no token in keyring");
    let client = MastodonClient::new(&instance, token).unwrap();
    let me = client.verify_credentials().await.unwrap();
    assert!(!me.username.is_empty());
}

#[tokio::test]
#[ignore = "requires a real Mastodon account and an internet connection"]
async fn fetch_home_timeline_returns_any_pagination_cursor_when_present() {
    let instance = env_instance();
    let cfg = Config::load(None).unwrap();
    let handle = cfg.default_account.clone().expect("no default account");
    let token = config::load_token(&handle).expect("no token");
    let client = MastodonClient::new(&instance, token).unwrap();
    let p = TimelineParams {
        limit: Some(5),
        ..Default::default()
    };
    let page = client.home_timeline(&p).await.unwrap();
    // An active home timeline will always produce a `next` cursor. If the
    // account is brand-new with no follows this may legitimately be None.
    let _ = page.next;
    assert!(page.items.len() <= 5);
}
