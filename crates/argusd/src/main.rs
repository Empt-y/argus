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
                // Every crate that runs a loop of its own belongs here. The
                // geofence engine was added and left out, which meant a
                // subsystem whose whole job is to say something said nothing —
                // indistinguishable from not running at all.
                .unwrap_or_else(|_| {
                    "argusd=info,argus_store=info,argus_ingest=info,\
                     argus_alert=info,argus_api=info"
                        .into()
                }),
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
    // A second client that keeps cookies, for the one provider that
    // authenticates with a session instead of a header. Separate on purpose:
    // a shared cookie jar lets one host's state follow requests to another.
    let session_http = argus_ingest::HttpClient::with_session(std::time::Duration::from_secs(30))?;
    let api_store = store.clone();
    let alert_store = store.clone();
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

    for source in build_sources(
        &config,
        &http,
        &session_http,
        std::sync::Arc::new(api_store.clone()),
        std::sync::Arc::new(api_store.clone()),
    ) {
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
            basemap: {
                // A named default wins; otherwise the single legacy URL. Both
                // spellings keep working, because a config that stops loading
                // after an upgrade is a worse outcome than two ways to say the
                // same thing.
                let named = config
                    .client
                    .basemap
                    .as_ref()
                    .and_then(|name| config.client.basemaps.get(name))
                    .map(|b| argus_api::Basemap {
                        tiles_url: b.tiles_url.clone(),
                        attribution: b.attribution.clone(),
                        paint: basemap_paint(b),
                    });
                named.or_else(|| {
                    config.client.basemap_tiles_url.clone().map(|tiles_url| {
                        argus_api::Basemap {
                            tiles_url,
                            attribution: config.client.basemap_attribution.clone(),
                            paint: argus_api::BasemapPaint::default(),
                        }
                    })
                })
            },
            basemaps: config
                .client
                .basemaps
                .iter()
                .map(|(name, b)| {
                    (
                        name.clone(),
                        argus_api::Basemap {
                            tiles_url: b.tiles_url.clone(),
                            attribution: b.attribution.clone(),
                            paint: basemap_paint(b),
                        },
                    )
                })
                .collect(),
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
    warn_about_unreachable_pairing(&config.server.bind, &config.server.public_url());
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

    // The geofence engine is a third peer, not a passenger on either of the
    // other two: it must keep watching whether or not a client is connected —
    // an alert nobody was there to receive is exactly the one the replay
    // machinery exists to deliver later.
    let alert_cancel = runtime.cancel_token();
    let alerts = tokio::spawn(
        argus_alert::Engine::new(alert_store).run(alert_cancel),
    );

    tracing::info!("argusd startup complete; ingest, alerts and api running");
    runtime
        .run(&credentials, budget, config.capture.disk_warn_fraction)
        .await?;
    tracing::info!("ingest stopped; draining api and alerts");
    if let Err(err) = alerts.await {
        tracing::warn!("geofence engine did not stop cleanly: {err}");
    }
    // The cancel token the API shut down on is the same one ingest stopped on,
    // so this join is already resolving by the time it is awaited.
    api.await??;
    Ok(())
}

/// Say so when the pairing QR will carry an address no other device can use.
///
/// The failure this catches is quiet and specific: bind the daemon to the LAN
/// or a tailnet address so a phone can reach it, forget to set
/// `server.public_url`, and it defaults to the bind — which is right — but bind
/// to `0.0.0.0` and it becomes a QR pointing at `0.0.0.0`, while leaving the
/// default loopback bind produces a QR pointing at the phone itself. In both
/// cases the daemon starts, the console prints a handsome QR code, and pairing
/// fails with a connection error that says nothing about why.
fn warn_about_unreachable_pairing(bind: &str, public_url: &str) {
    let host = public_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or_default()
        .rsplit_once(':')
        .map_or(public_url, |(host, _)| host);

    let unroutable = matches!(host, "0.0.0.0" | "[::]" | "::");
    let loopback = matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1");
    let bind_is_loopback = bind.starts_with("127.") || bind.starts_with("[::1]") || bind.starts_with("localhost");

    if unroutable {
        tracing::warn!(
            %public_url,
            "the pairing QR will carry an address no device can dial; set server.public_url              to this machine's LAN or tailnet address"
        );
    } else if loopback && !bind_is_loopback {
        tracing::warn!(
            %bind,
            %public_url,
            "bound beyond loopback but the pairing QR still says localhost, which on a phone              means the phone; set server.public_url"
        );
    }
}

fn basemap_paint(b: &config::BasemapConfig) -> argus_api::BasemapPaint {
    argus_api::BasemapPaint {
        brightness_max: b.brightness_max,
        brightness_min: b.brightness_min,
        saturation: b.saturation,
        contrast: b.contrast,
    }
}

/// Build the enabled driver set.
///
/// A source absent from config is enabled by default — the config file lists
/// exceptions and credentials, not an allowlist, so adding a driver does not
/// require every existing deployment to opt in.
fn build_sources(
    config: &Config,
    http: &argus_ingest::HttpClient,
    session_http: &argus_ingest::HttpClient,
    zone_cache: std::sync::Arc<dyn argus_core::GeometryCache>,
    catalogue: std::sync::Arc<dyn argus_core::TrackedCatalogue>,
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
        // Satellites are a chain too, and the shape is unusual enough to state:
        // the fallback follows the primary's curation rather than choosing its
        // own. CelesTrak decides *which* objects Argus tracks, by group;
        // Space-Track has no equivalent grouping and would otherwise hand back
        // the entire on-orbit catalogue. So both share one element store — the
        // primary writes the catalogue, the fallback reads which numbers to ask
        // for. Which also means the fallback cannot bootstrap: until CelesTrak
        // has succeeded once there is nothing to ask for, and that is the
        // honest shape of a failover rather than a limitation to paper over.
        let store = argus_ingest::sources::elements::ElementStore::in_dir(&config.state_dir);
        let credentials = config
            .sources
            .get("spacetrack")
            .map(|s| s.credentials.clone())
            .unwrap_or_default();

        let mut providers: Vec<std::sync::Arc<dyn argus_core::Source>> =
            vec![std::sync::Arc::new(
                argus_ingest::sources::CelestrakSatellites::new(http.clone())
                    .persisting_in(&config.state_dir),
            )];
        if enabled("spacetrack") {
            providers.push(std::sync::Arc::new(
                // Its own client: Space-Track authenticates with a session
                // cookie, and a cookie jar shared with thirty other providers
                // is a way for one host's state to follow requests to another.
                argus_ingest::sources::spacetrack::SpaceTrackSatellites::new(
                    session_http.clone(),
                    store,
                )
                .with_login(
                    credentials
                        .get(argus_ingest::sources::spacetrack::IDENTITY_KEY)
                        .cloned(),
                    credentials
                        .get(argus_ingest::sources::spacetrack::PASSWORD_KEY)
                        .cloned(),
                )
                // So a cold start during an outage can still name the objects
                // to ask for, from what this deployment has already recorded.
                .with_catalogue(catalogue.clone()),
            ));
        }
        sources.push(std::sync::Arc::new(argus_ingest::ProviderChain::new(
            "satellites",
            providers,
        )));
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
        ];
        sources.push(std::sync::Arc::new(argus_ingest::ProviderChain::new(
            "flights", providers,
        )));

        // OpenSky is not in that chain, and moving it out is the change that
        // makes global coverage possible at all.
        //
        // As a chain member it was a last-resort *substitute* for the
        // aggregators — polled only when they failed, and only over the same
        // AOI. But its `states/all` endpoint is the one genuinely global feed
        // available: adsb.lol's `/v2/all` answers 503, and covering the planet
        // through a 250 nm radius API would take ~750 requests per cycle.
        //
        // So it runs alongside instead: the aggregators sweep the declared AOIs
        // at full cadence, and this sweeps everywhere else at whatever its
        // allowance permits. Both write into the `flights` layer under the same
        // natural key, so an aircraft seen by both merges rather than doubling —
        // which is exactly what `EntityId` was specified to do.
        //
        // It also remains the fallback it used to be, and a better one: if both
        // aggregators die, what is left is a slower picture of the whole world
        // rather than a slower picture of one circle.
        // The wide tier: Europe, swept slowly.
        //
        // Every AOI shares one source cadence, so putting a continent in the
        // AOI list would drag the home area from a 20-second refresh to a
        // minute-plus — paying for breadth everywhere with fidelity where it
        // matters most. This is a separate source with its own region and its
        // own cadence, so the two tiers do not compete.
        //
        // Europe tiles to 44 circles at 250 nm. `HttpClient` paces every host to
        // one request a second, so that is a ~44-second poll however often it
        // is scheduled — the cadence decides the *duty cycle*, not the burst.
        //
        // Ten minutes, arrived at by being told off twice. Three minutes meant
        // 44 seconds of continuous requests out of every 180, which on top of
        // the flights chain's own traffic was enough for adsb.fi to start
        // refusing — and the chain answers a refusal by sidelining the provider
        // for fifteen minutes, so pushing costs far more coverage than it buys.
        // At ten minutes this is ~0.07 requests a second averaged, which is a
        // fair thing to ask of a network that gives its data away.
        //
        // It runs against adsb.fi rather than adsb.lol so the two tiers lean on
        // different receiver networks in normal operation.
        sources.push(std::sync::Arc::new(
            argus_ingest::sources::ReadsbProvider::wide(
                http.clone(),
                "adsb-fi-europe",
                "Aircraft (adsb.fi, Europe sweep)",
                "https://opendata.adsb.fi/api/v2",
                argus_core::source::Attribution {
                    provider: "adsb.fi".into(),
                    url: "https://adsb.fi/".into(),
                    license: "Open data — community-contributed receiver data".into(),
                    notice: Some("Aircraft data from the adsb.fi community network".into()),
                },
                argus_core::BoundingBox::new(-11.0, 35.0, 32.0, 71.0),
                600,
            ),
        ));

        sources.push(std::sync::Arc::new(argus_ingest::sources::OpenSky::new(
            http.clone(),
        )));
    }

    // Aviation hazards. Global despite the US operator: the Aviation Weather
    // Center aggregates SIGMETs from watch offices worldwide.
    if enabled("sigmets") {
        sources.push(std::sync::Arc::new(argus_ingest::sources::Sigmets::new(
            http.clone(),
        )));
    }

    // Storm overflows. Nine water companies, nine separate sources into one
    // layer: they are disjoint regions rather than alternative providers of the
    // same data, so one company failing must not stop the other eight being
    // polled, and the health panel should name whichever is down.
    if enabled("storm-overflows") {
        for company in argus_ingest::sources::StormOverflows::all(http.clone()) {
            sources.push(std::sync::Arc::new(company));
        }
    }

    // Radiosondes. Their own layer rather than joining `flights`: both are
    // EntityKind::Aircraft and reuse the same track machinery, but a weather
    // balloon and an airliner are different things to switch on and off, and
    // the layer is the unit a client toggles.
    if enabled("radiosondes") {
        sources.push(std::sync::Arc::new(argus_ingest::sources::SondeHub::new(
            http.clone(),
        )));
    }

    sources
}
