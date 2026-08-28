//! The Argus daemon.
//!
//! Phase 0 scope: load and validate config, connect to the store, apply
//! migrations, and report what it found. Ingest, API and alerting are wired in
//! as their crates land.

use std::path::PathBuf;
use std::process::ExitCode;

mod config;

use config::Config;

const DEFAULT_CONFIG_PATH: &str = "/etc/argus/argus.toml";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ARGUS_LOG")
                .unwrap_or_else(|_| "argusd=info,argus_store=info,argus_ingest=info".into()),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Print the whole chain: the root cause of a startup failure is
            // usually two levels down (a connection refused inside a store
            // error inside a config error), and burying it wastes the operator's
            // time at exactly the wrong moment.
            tracing::error!("argusd failed to start: {err}");
            let mut source = std::error::Error::source(&*err);
            while let Some(cause) = source {
                tracing::error!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = std::env::var_os("ARGUS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));

    tracing::info!("loading config from {}", config_path.display());
    let config = Config::load(&config_path)?;

    if config.is_externally_bound() {
        // Worth saying out loud every start. The daemon holds every configured
        // provider credential and will broker them for anyone who can reach it.
        tracing::warn!(
            bind = %config.server.bind,
            "bound beyond loopback — this instance brokers your API keys to anyone \
             who can reach it. Ensure provider-side budget caps are set."
        );
    }

    tracing::info!("connecting to database");
    let store = argus_store::Store::connect(&config.database.url, config.database.max_connections)
        .await?;

    tracing::info!("applying migrations");
    store.migrate().await?;

    let bytes = store.total_bytes().await?;
    let budget = config.capture.disk_budget_gb * 1024 * 1024 * 1024;
    tracing::info!(
        used_mb = bytes / 1024 / 1024,
        budget_gb = config.capture.disk_budget_gb,
        "store ready"
    );
    if bytes as u64 > (budget as f64 * config.capture.disk_warn_fraction) as u64 {
        tracing::warn!("store is above the disk warning threshold; capture will degrade to AOI-only");
    }

    tracing::info!(
        aois = config.aois.len(),
        sources = config.sources.len(),
        "configuration loaded"
    );

    // Phase 1 onwards attaches the ingest scheduler, API server and alert
    // engine here.
    tracing::info!("argusd startup complete");
    Ok(())
}
