//! `mastoot update` — replace this binary with the latest GitHub release.
//!
//! Built on `axoupdater`, which reads the install receipt that the
//! cargo-dist shell installer leaves behind and re-runs that installer.
//! Copies that did not come from the installer (Homebrew, `cargo build`)
//! have no receipt, so we say how to update them instead of guessing.

use anyhow::{Context, Result};
use axoupdater::{AxoUpdater, AxoupdateError};

use crate::cli::UpdateArgs;

const APP_NAME: &str = "mastoot";

/// What the user should do when this copy cannot update itself.
const NOT_SELF_UPDATABLE: &str = "\
this copy of mastoot was not installed with the release installer, so it \
cannot update itself.
  · Homebrew:    brew upgrade mastoot
  · From source: git pull && cargo build --release
  · Otherwise:   re-run the install command from the README";

pub async fn run(args: UpdateArgs) -> Result<()> {
    let mut updater = AxoUpdater::new_for(APP_NAME);
    if let Err(err) = updater.load_receipt() {
        return match err {
            AxoupdateError::NoReceipt { .. } | AxoupdateError::ReceiptLoadFailed { .. } => {
                println!("{NOT_SELF_UPDATABLE}");
                Ok(())
            }
            other => Err(other).context("could not read the install receipt"),
        };
    }

    // The receipt can outlive the binary it was written for (e.g. a
    // `cargo install` on top of an installer copy); only the exact
    // executable the installer placed is ours to replace.
    if !updater.check_receipt_is_for_this_executable()? {
        println!("{NOT_SELF_UPDATABLE}");
        return Ok(());
    }

    let current = env!("CARGO_PKG_VERSION");
    if args.check {
        return if updater
            .is_update_needed()
            .await
            .context("could not reach GitHub to look for a new release")?
        {
            let latest = updater
                .query_new_version()
                .await?
                .map_or_else(|| "a newer version".to_string(), ToString::to_string);
            println!("mastoot {current} → {latest} is available; run `mastoot update`.");
            Ok(())
        } else {
            println!("mastoot {current} is up to date.");
            Ok(())
        };
    }

    println!("mastoot {current}: checking for updates…");
    match updater
        .run()
        .await
        .context("update failed (nothing was changed; try again or re-run the installer)")?
    {
        Some(done) => println!(
            "updated to mastoot {} — restart mastoot to use it.",
            done.new_version
        ),
        None => println!("mastoot {current} is up to date."),
    }
    Ok(())
}
