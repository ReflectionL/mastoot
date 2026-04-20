//! `tracing` setup. Logs to `~/.cache/mastoot/log.txt` — never stdout,
//! because that would corrupt the TUI.

use std::fs;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Must be held alive for the duration of the program; dropping it flushes
/// any buffered log writes.
pub struct LogGuard {
    _worker: WorkerGuard,
}

/// Initialize tracing. `verbose` (0..=2) selects the default level if
/// `RUST_LOG` is unset.
pub fn init(verbose: u8) -> Result<LogGuard> {
    let dirs = ProjectDirs::from("io.github", "reflectionl", "mastoot")
        .context("could not resolve platform-specific project directories")?;
    let cache_dir = dirs.cache_dir();
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("failed to create cache directory {}", cache_dir.display()))?;

    let file_appender = tracing_appender::rolling::never(cache_dir, "log.txt");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let default_level = match verbose {
        0 => "mastoot=info,warn",
        1 => "mastoot=debug,info",
        _ => "mastoot=trace,debug",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let subscriber = tracing_subscriber::registry().with(filter).with(
        fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_target(true)
            .with_thread_ids(false),
    );

    subscriber
        .try_init()
        .context("tracing subscriber already installed")?;

    Ok(LogGuard { _worker: guard })
}
