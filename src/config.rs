//! Config file parsing and account credential storage.
//!
//! `~/.config/mastoot/config.toml` holds non-secret preferences (theme,
//! default instance, OAuth app registrations). The OAuth access token is
//! stored in the OS keyring (macOS Keychain / Linux Secret Service), not
//! the config file, so the config file can be checked into a dotfiles repo
//! without leaking credentials.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::cli::DEFAULT_INSTANCE;

/// Keyring service name. Everything mastoot stores in the keyring uses this.
pub const KEYRING_SERVICE: &str = "mastoot";

/// Root config file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Instance used when `--instance` is not passed.
    pub default_instance: Option<String>,

    /// Which logged-in account to use by default, if any.
    pub default_account: Option<String>,

    #[serde(default)]
    pub theme: ThemeConfig,

    #[serde(default)]
    pub ui: UiConfig,

    /// Per-instance OAuth app registrations. Keyed by hostname.
    #[serde(default)]
    pub apps: std::collections::BTreeMap<String, AppRegistration>,

    /// Logged-in account identifiers (`@user@instance`) whose tokens live
    /// in the keyring.
    #[serde(default)]
    pub accounts: Vec<AccountRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub name: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "frost".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub show_relative_time: bool,
    pub media_render: MediaRender,
    pub nerd_font: bool,
    /// Default live-update mode at startup. Runtime `S` toggles
    /// without persisting — edit the config to change the default.
    pub stream_mode: crate::state::StreamMode,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            show_relative_time: true,
            media_render: MediaRender::Auto,
            nerd_font: true,
            stream_mode: crate::state::StreamMode::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaRender {
    Auto,
    Images,
    TextOnly,
}

/// An OAuth app registration. Mastodon scopes these per-instance: you
/// POST /api/v1/apps once per instance and re-use the returned client id
/// and secret for every OAuth flow afterwards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRegistration {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub scopes: String,
}

/// Reference to a logged-in account. The access token itself is stored in
/// the keyring under (service=`mastoot`, account=`handle`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRef {
    /// Fully qualified handle, e.g. `alice@mastodon.social`.
    pub handle: String,
    /// Instance hostname, e.g. `mastodon.social`.
    pub instance: String,
    /// Mastodon account id (string — never parse as integer).
    pub account_id: String,
    /// Display name cached at login time; refreshed on startup.
    pub display_name: Option<String>,
}

impl Config {
    /// Returns `(config_file_path, project_dirs)`.
    pub fn resolve_path(explicit: Option<&Path>) -> Result<(PathBuf, ProjectDirs)> {
        let dirs = ProjectDirs::from("io.github", "reflectionl", "mastoot")
            .context("could not resolve platform project directories")?;
        let path = if let Some(p) = explicit {
            p.to_path_buf()
        } else {
            dirs.config_dir().join("config.toml")
        };
        Ok((path, dirs))
    }

    /// Load the config file, or return `Config::default()` if it does not
    /// exist yet. Only hard I/O or parse errors surface.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let (path, _) = Self::resolve_path(explicit)?;
        if !path.exists() {
            tracing::debug!(path = %path.display(), "config file missing, using defaults");
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let cfg: Self = toml::from_str(&text)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        Ok(cfg)
    }

    /// Serialize back to disk, creating parent directories as needed.
    pub fn save(&self, explicit: Option<&Path>) -> Result<()> {
        let (path, _) = Self::resolve_path(explicit)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }
        let text = toml::to_string_pretty(self).context("failed to serialize config")?;
        fs::write(&path, text)
            .with_context(|| format!("failed to write config {}", path.display()))?;
        Ok(())
    }

    /// Returns the instance to use, given optional CLI override.
    pub fn effective_instance(&self, cli_override: Option<&str>) -> String {
        cli_override
            .map(str::to_string)
            .or_else(|| self.default_instance.clone())
            .unwrap_or_else(|| DEFAULT_INSTANCE.to_string())
    }
}

/// Save an access token to the OS keyring. The `handle` is the full
/// `user@instance` identifier; looking up by instance alone is not enough
/// because a user may be signed into multiple accounts on the same server
/// (future v2).
pub fn store_token(handle: &str, token: &SecretString) -> Result<()> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, handle).context("failed to open keyring entry")?;
    entry
        .set_password(token.expose_secret())
        .context("failed to save token to keyring")?;
    Ok(())
}

/// Load an access token from the OS keyring.
pub fn load_token(handle: &str) -> Result<SecretString> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, handle).context("failed to open keyring entry")?;
    let token = entry
        .get_password()
        .with_context(|| format!("no token stored for {handle}"))?;
    Ok(SecretString::from(token))
}

/// Forget an access token.
pub fn delete_token(handle: &str) -> Result<()> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, handle).context("failed to open keyring entry")?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::anyhow!(e).context("failed to delete keyring entry")),
    }
}
