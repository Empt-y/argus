//! The Argus daemon.
//!
//! Loads and validates config, connects to the store, applies migrations, then
//! runs two things concurrently for the rest of its life: the ingest scheduler
//! filling the DVR, and the API serving it. Neither can outlive the other — a
//! server with no ingest is a museum, and ingest with no server is a database
//! nobody can see — so a failure in either brings the process down and lets
//! systemd restart it.

use std::path::PathBuf;
use std::process::ExitCode;

mod config;
mod pairing;

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
    let budget: u64 = config.capture.disk_budget_gb * 1024 * 1024 * 1024;
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

    // --- ingest ---------------------------------------------------------
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30))?;
    let api_store = store.clone();
    let runtime = argus_ingest::Runtime::new(
        store,
        argus_ingest::SchedulerConfig {
            global_cadence_scale: config.capture.global_cadence_scale,
            aoi_only: false,
        },
    );

    let aois: Vec<argus_core::BoundingBox> =
        config.aois.iter().map(|a| a.to_bbox()).collect();
    if aois.is_empty() {
        tracing::warn!(
            "no areas of interest configured; bounded sources will fall back to a \
             single clamped global query. Declare [[aoi]] blocks for the regions \
             you actually watch."
        );
    }
    let mut runtime = runtime.with_aois(aois);

    for source in build_sources(&config, &http, std::sync::Arc::new(api_store.clone())) {
        runtime.register(source);
    }

    let credentials = argus_ingest::CredentialResolver::new(
        config
            .sources
            .iter()
            .map(|(id, sc)| (id.clone(), sc.credentials.clone()))
            .collect(),
    );

    let cancel = runtime.cancel_token();
    tokio::spawn(async move {
        // SIGTERM is what systemd sends on stop; without handling it the
        // daemon is killed mid-batch and the shutdown looks like a crash in
        // the journal every single time.
        let mut term = match tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        ) {
            Ok(s) => s,
            Err(err) => {
                tracing::error!("could not install SIGTERM handler: {err}");
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("interrupt received, shutting down"),
            _ = term.recv() => tracing::info!("SIGTERM received, shutting down"),
        }
        cancel.cancel();
    });

    // --- api -------------------------------------------------------------
    let api_state = argus_api::ApiState::new(
        api_store.clone(),
        argus_api::ApiConfig {
            auth: match config.server.auth {
                config::AuthPolicy::LoopbackExempt => argus_api::AuthMode::LoopbackExempt,
                config::AuthPolicy::Required => argus_api::AuthMode::Required,
            },
            allowed_origins: config.server.allowed_origins.clone(),
            client_keys: argus_api::ClientKeys {
                google_maps_api_key: config.client_keys.google_maps_api_key.clone(),
                cesium_ion_token: config.client_keys.cesium_ion_token.clone(),
                buildings_tileset_url: config.client.buildings_tileset_url.clone(),
            },
            public_url: config.server.public_url(),
        },
    );

    // Terrain is optional and must stay optional: a grid that fails to load is
    // a client falling back to global terrain, never a daemon that will not
    // start. Order is preserved from the config — finest first — because the
    // tile route answers from the first grid that covers a tile whole.
    let mut dems = Vec::new();
    for path in &config.client.terrain_grids {
        match argus_tiles::dem::Dem::load(path) {
            Ok(dem) => {
                let m = dem.meta();
                tracing::info!(
                    grid = %path.display(),
                    width = m.width,
                    height = m.height,
                    ground_m = m.ground_metres,
                    datum = %m.datum,
                    bounds = format!("{},{},{},{}", m.west, m.south, m.east, m.north),
                    "terrain grid loaded"
                );
                dems.push(dem);
            }
            Err(err) => tracing::warn!("terrain grid {} not loaded: {err}", path.display()),
        }
    }
    let api_state = argus_api::ApiState {
        dems: std::sync::Arc::new(dems),
        ..api_state
    };

    // A daemon nobody has paired with is a daemon nobody can use, and the one
    // moment an operator is definitely looking at the console is the moment
    // they started it. Offering the code here rather than making them find a
    // command for it is the difference between pairing taking ten seconds and
    // taking a documentation search.
    if !api_store.has_devices().await? {
        let code = api_state.pairing.issue();
        pairing::print_invitation(&api_state.pairing_url(&code), &code);
    }

    let listener = tokio::net::TcpListener::bind(&config.server.bind).await?;
    tracing::info!(
        bind = %config.server.bind,
        public_url = %config.server.public_url(),
        "api listening"
    );
    let api_cancel = runtime.cancel_token();
    let api = tokio::spawn(async move {
        axum::serve(
            listener,
            argus_api::router(api_state)
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move { api_cancel.cancelled().await })
        .await
    });

    tracing::info!("argusd startup complete; ingest and api running");
    runtime
        .run(&credentials, budget, config.capture.disk_warn_fraction)
        .await?;
    tracing::info!("ingest stopped; draining api");
    // The cancel token the API shut down on is the same one ingest stopped on,
    // so this join is already resolving by the time it is awaited.
    api.await??;
    Ok(())
}

/// Build the enabled driver set.
///
/// A source absent from config is enabled by default — the config file lists
/// exceptions and credentials, not an allowlist, so adding a driver does not
/// require every existing deployment to opt in.
fn build_sources(
    config: &Config,
    http: &argus_ingest::HttpClient,
    zone_cache: std::sync::Arc<dyn argus_core::GeometryCache>,
) -> Vec<std::sync::Arc<dyn argus_core::Source>> {
    let enabled = |id: &str| {
        config
            .sources
            .get(id)
            .is_none_or(|s| s.enabled)
    };

    let mut sources: Vec<std::sync::Arc<dyn argus_core::Source>> = Vec::new();
    // Earthquakes run as a chain too. Both catalogues are global and keyless;
    // EMSC covers the case where USGS is unreachable, which has happened during
    // US government shutdowns.
    if enabled("earthquakes") {
        let providers: Vec<std::sync::Arc<dyn argus_core::Source>> = vec![
            std::sync::Arc::new(argus_ingest::sources::UsgsEarthquakes::new(http.clone())),
            std::sync::Arc::new(argus_ingest::sources::EmscEarthquakes::new(http.clone())),
        ];
        sources.push(std::sync::Arc::new(argus_ingest::ProviderChain::new(
            "earthquakes", providers,
        )));
    }
    if enabled("celestrak") {
        sources.push(std::sync::Arc::new(
            argus_ingest::sources::CelestrakSatellites::new(http.clone()),
        ));
    }

    if enabled("nws-alerts") {
        // The store backs the zone cache, so the few hundred county and marine
        // outlines this driver needs are fetched once in the life of the
        // deployment rather than once per restart.
        sources.push(std::sync::Arc::new(
            argus_ingest::sources::NwsAlerts::new(http.clone())
                .with_zone_cache(zone_cache.clone()),
        ));
    }

    // Flights are served by a chain rather than one provider. Both members are
    // keyless, unmetered and backed by independent receiver networks, so an
    // outage or a policy change at one costs nothing — which is not
    // hypothetical: a third candidate, airplanes.live, started requiring a key
    // during development and would simply have been skipped.
    //
    // Order is deliberate: these community aggregators are unmetered and cover
    // an area of interest well, so they go ahead of any allowance-limited
    // provider whose credits are better spent elsewhere.
    if enabled("flights") {
        let providers: Vec<std::sync::Arc<dyn argus_core::Source>> = vec![
            std::sync::Arc::new(argus_ingest::sources::ReadsbProvider::adsb_lol(http.clone())),
            std::sync::Arc::new(argus_ingest::sources::ReadsbProvider::adsb_fi(http.clone())),
            // Last resort. Metered at 400 credits a day anonymously, so its
            // allowance is only spent when both unmetered networks are down —
            // which is precisely when it is worth having.
            std::sync::Arc::new(argus_ingest::sources::OpenSky::new(http.clone())),
        ];
        sources.push(std::sync::Arc::new(argus_ingest::ProviderChain::new(
            "flights", providers,
        )));
    }

    sources
}
