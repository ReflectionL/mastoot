//! Mastodon OAuth 2.0 "authorization code" flow, with PKCE.
//!
//! The flow:
//!
//! 1. Register an app via `POST /api/v1/apps` if we don't have credentials
//!    for this instance yet (cached in config under `config.apps`).
//! 2. Spawn a tiny loopback HTTP server on a random free port of 127.0.0.1
//!    and remember the full redirect URL.
//! 3. Build the authorize URL with PKCE `code_challenge` and open it in the
//!    user's browser.
//! 4. Wait for the browser to redirect back to the loopback with `?code=…`.
//! 5. Exchange the code for a token via `POST /oauth/token`.
//! 6. Return the token as a `SecretString`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use rand::Rng;
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use url::Url;

use crate::api::client::MastodonClient;
use crate::api::error::{ApiError, ApiResult};
use crate::api::models::{Application, TokenResponse};
use crate::config::{AppRegistration, Config};

/// OAuth scopes we request. `follow` is a Mastodon-legacy umbrella that
/// covers follow/unfollow/block/mute. We do not request `push`.
pub const SCOPES: &str = "read write follow";

const CLIENT_NAME: &str = "mastoot";
const CLIENT_WEBSITE: &str = "https://github.com/ReflectionL/mastoot";

/// Result of a successful login flow.
#[derive(Debug)]
pub struct LoginOutcome {
    pub token: SecretString,
    pub scope: String,
}

/// Perform an interactive OAuth login against `instance`. `cfg` is
/// updated in-place with a cached app registration if the instance was
/// unseen; the caller is responsible for `cfg.save()` afterwards.
///
/// Set `no_browser = true` for headless / SSH environments — the URL
/// will just be printed instead of opened.
pub async fn login(instance: &str, cfg: &mut Config, no_browser: bool) -> Result<LoginOutcome> {
    let anon = MastodonClient::anonymous(instance)?;

    // --- 1. Bind a local redirect listener before anything else. ---
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("failed to bind a local redirect port on 127.0.0.1")?;
    listener
        .set_nonblocking(false)
        .context("set_nonblocking failed")?;
    let local_addr = listener
        .local_addr()
        .context("failed to read local redirect address")?;
    let redirect_uri = format!("http://{local_addr}/callback");
    tracing::debug!(%redirect_uri, "waiting for OAuth redirect");

    // --- 2. Ensure we have an app registration for this instance. ---
    let app = ensure_app_registration(&anon, cfg, instance, &redirect_uri).await?;

    // --- 3. Build PKCE + state. ---
    let verifier = generate_code_verifier();
    let challenge = code_challenge_s256(&verifier);
    let state: String = random_token(32);

    // --- 4. Build authorize URL. ---
    let mut authorize = anon.base_url().join("/oauth/authorize")?;
    authorize
        .query_pairs_mut()
        .append_pair("client_id", &app.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &app.scopes)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);

    let authorize_url = authorize.to_string();
    println!("\nOpen the following URL in your browser to authorize mastoot:");
    println!("\n    {authorize_url}\n");
    if !no_browser {
        if let Err(e) = open::that(&authorize_url) {
            tracing::warn!(?e, "failed to open browser automatically");
        }
    }

    // --- 5. Wait for the redirect (blocking read on a oneshot socket). ---
    let (code, got_state) =
        tokio::task::spawn_blocking(move || wait_for_redirect(&listener, Duration::from_secs(300)))
            .await
            .context("redirect listener task failed")??;

    if got_state != state {
        return Err(anyhow!(
            "OAuth state mismatch; got {got_state:?} expected {state:?}"
        ));
    }

    // --- 6. Exchange the authorization code for an access token. ---
    let form = [
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("client_id", &app.client_id),
        ("client_secret", &app.client_secret),
        ("redirect_uri", &redirect_uri),
        ("scope", &app.scopes),
        ("code_verifier", &verifier),
    ];
    let token: TokenResponse = anon.post_form("/oauth/token", &form).await?;

    Ok(LoginOutcome {
        token: SecretString::from(token.access_token),
        scope: token.scope,
    })
}

/// Revoke a token at the server. Silently succeeds if the server refuses.
pub async fn revoke(instance: &str, app: &AppRegistration, token: &SecretString) -> ApiResult<()> {
    use secrecy::ExposeSecret as _;
    let client = MastodonClient::anonymous(instance)?;
    let form = [
        ("client_id", app.client_id.as_str()),
        ("client_secret", app.client_secret.as_str()),
        ("token", token.expose_secret()),
    ];
    match client
        .post_form::<serde_json::Value, _>("/oauth/revoke", &form)
        .await
    {
        Ok(_) | Err(ApiError::Server { .. }) => Ok(()),
        Err(e) => Err(e),
    }
}

async fn ensure_app_registration(
    anon: &MastodonClient,
    cfg: &mut Config,
    instance: &str,
    redirect_uri: &str,
) -> Result<AppRegistration> {
    // Every local-loopback redirect has a different port, so we can't
    // reuse a cached registration whose registered redirect URI pinned a
    // specific port. Re-register if the URI differs; drop the stale
    // entry so the config file doesn't grow unboundedly.
    if let Some(existing) = cfg.apps.get(instance) {
        if existing.redirect_uri == redirect_uri {
            return Ok(existing.clone());
        }
        cfg.apps.remove(instance);
    }

    // Register a new app. For loopback clients we register with a
    // wildcard-ish pattern by sending the exact URI; servers accept
    // arbitrary 127.0.0.1:PORT URIs.
    let form = [
        ("client_name", CLIENT_NAME),
        ("redirect_uris", redirect_uri),
        ("scopes", SCOPES),
        ("website", CLIENT_WEBSITE),
    ];
    let app: Application = anon.post_form("/api/v1/apps", &form).await?;
    let client_id = app
        .client_id
        .clone()
        .ok_or_else(|| anyhow!("server response missing client_id"))?;
    let client_secret = app
        .client_secret
        .clone()
        .ok_or_else(|| anyhow!("server response missing client_secret"))?;

    let registration = AppRegistration {
        client_id,
        client_secret,
        redirect_uri: redirect_uri.to_string(),
        scopes: SCOPES.to_string(),
    };
    cfg.apps.insert(instance.to_string(), registration.clone());
    Ok(registration)
}

/// Blocking — wait for a single HTTP GET to `/callback?code=…&state=…`,
/// write a friendly HTML response to the browser, and return the parsed
/// (code, state) pair.
fn wait_for_redirect(listener: &TcpListener, timeout: Duration) -> Result<(String, String)> {
    listener
        .set_nonblocking(false)
        .context("set_nonblocking failed")?;
    let _ = timeout; // reserved: could be hooked up via a background thread
    let (mut stream, peer) = listener
        .accept()
        .context("accept on redirect socket failed")?;
    tracing::debug!(%peer, "redirect received");
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).context("reading redirect request")?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let (code, state, error) = parse_redirect_request(&req);

    let body = if let Some(err) = &error {
        format!(
            "<html><body style='font-family:-apple-system,sans-serif'>\
             <h2>mastoot · authorization failed</h2>\
             <p>{err}</p>\
             <p>You may close this tab.</p></body></html>"
        )
    } else {
        "<html><body style='font-family:-apple-system,sans-serif'>\
         <h2>mastoot · authorized ✓</h2>\
         <p>You may close this tab and return to the terminal.</p>\
         </body></html>"
            .to_string()
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();

    if let Some(err) = error {
        return Err(anyhow!("authorization error: {err}"));
    }
    let code = code.ok_or_else(|| anyhow!("callback missing ?code"))?;
    let state = state.ok_or_else(|| anyhow!("callback missing ?state"))?;
    Ok((code, state))
}

fn parse_redirect_request(raw: &str) -> (Option<String>, Option<String>, Option<String>) {
    // Take the first request line: "GET /callback?code=…&state=… HTTP/1.1"
    let first_line = raw.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("");
    let dummy_base = "http://127.0.0.1";
    let full = format!("{dummy_base}{path}");
    let Ok(parsed) = Url::parse(&full) else {
        return (None, None, Some("malformed redirect URL".into()));
    };
    let params: HashMap<String, String> = parsed
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let code = params.get("code").cloned();
    let state = params.get("state").cloned();
    let error = params.get("error").map(|e| {
        params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| e.clone())
    });
    (code, state, error)
}

fn generate_code_verifier() -> String {
    // RFC 7636: 43..=128 chars from the unreserved set.
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::thread_rng();
    (0..96)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

fn code_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn random_token(n: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..n)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_deterministic_for_a_given_verifier() {
        // Test vector from RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(code_challenge_s256(verifier), expected);
    }

    #[test]
    fn redirect_parse_extracts_code_and_state() {
        let raw = "GET /callback?code=abc&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        let (code, state, err) = parse_redirect_request(raw);
        assert_eq!(code.as_deref(), Some("abc"));
        assert_eq!(state.as_deref(), Some("xyz"));
        assert!(err.is_none());
    }

    #[test]
    fn redirect_parse_surfaces_error() {
        let raw = "GET /callback?error=access_denied&error_description=user+denied HTTP/1.1\r\n";
        let (_, _, err) = parse_redirect_request(raw);
        assert_eq!(err.as_deref(), Some("user denied"));
    }
}
