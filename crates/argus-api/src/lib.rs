//! The HTTP and WebSocket surface.
//!
//! This is the point at which the DVR stops being something only `psql` can
//! reach. Everything both clients do — the map, the scrubber, the entity cards,
//! the alert stream — is one of the routes assembled here, and the shape of
//! this API is what decides whether the Android client is a thin renderer or a
//! second implementation of every layer. It is deliberately the former.

pub mod auth;
pub mod error;
pub mod params;
pub mod routes;

pub use auth::{AuthMode, Caller, PairingCodes};
pub use error::{ApiError, ApiResult};

use argus_store::Store;
use argus_tiles::Tiler;
use axum::routing::{delete, get, post};
use axum::Router;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

/// Credentials that legitimately reach a browser. See
/// [`routes::catalog::client_keys`] for why these two are different from every
/// other secret in the config.
#[derive(Debug, Clone, Default)]
pub struct ClientKeys {
    pub google_maps_api_key: Option<String>,
    pub cesium_ion_token: Option<String>,
    /// Not a credential — a plain URL, carried here because it reaches the
    /// client through the same request and a second round trip for one string
    /// would be silly.
    pub buildings_tileset_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub auth: AuthMode,
    /// Extra origins allowed to call the API — the web client in development.
    pub allowed_origins: Vec<String>,
    pub client_keys: ClientKeys,
    /// How this server is reachable, used to build tile URLs in the style
    /// document and the pairing URL in the QR. A phone cannot use
    /// `127.0.0.1`, so this must be the LAN or Tailscale address rather than
    /// the bind address.
    pub public_url: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            auth: AuthMode::LoopbackExempt,
            allowed_origins: Vec::new(),
            client_keys: ClientKeys::default(),
            public_url: "http://127.0.0.1:8787".into(),
        }
    }
}

#[derive(Clone)]
pub struct ApiState {
    pub store: Store,
    pub tiler: Tiler,
    /// The self-hosted elevation grid, when one is configured. `None` leaves
    /// every terrain route answering "not here", which the client reads as
    /// "use your global fallback".
    pub dem: Option<Arc<argus_tiles::dem::Dem>>,
    pub config: Arc<ApiConfig>,
    pub pairing: Arc<PairingCodes>,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl ApiState {
    pub fn new(store: Store, config: ApiConfig) -> Self {
        Self {
            tiler: Tiler::new(store.clone()),
            store,
            dem: None,
            config: Arc::new(config),
            pairing: Arc::new(PairingCodes::new()),
            started_at: chrono::Utc::now(),
        }
    }

    /// The URL a phone scans. A custom scheme rather than an `https://` link so
    /// the Android client can claim it with an intent filter and the code never
    /// travels to a web server.
    pub fn pairing_url(&self, code: &str) -> String {
        format!(
            "argus://pair?server={}&code={code}",
            self.config.public_url.trim_end_matches('/')
        )
    }
}

/// Assemble the whole API.
///
/// Note the split: `/v1/health` and `/v1/pair` are outside the auth layer and
/// everything else is inside it. Health has to answer before a device is paired
/// or there is no way to tell a wrong address from a down server, and pairing
/// cannot require a token because acquiring one is the entire point.
pub fn router(state: ApiState) -> Router {
    let protected = Router::new()
        .route("/v1/sources", get(routes::catalog::sources))
        .route("/v1/layers", get(routes::catalog::layers))
        .route("/v1/client-keys", get(routes::catalog::client_keys))
        .route("/v1/entities", get(routes::entities::list))
        .route("/v1/entities/{kind}/{key}", get(routes::entities::detail))
        .route(
            "/v1/entities/{kind}/{key}/track",
            get(routes::entities::track),
        )
        .route("/v1/style.json", get(routes::tiles::style))
        .route("/v1/tiles/{z}/{x}/{y}", get(routes::tiles::tile))
        .route(
            "/v1/tiles/{layer}/{z}/{x}/{y}",
            get(routes::tiles::layer_tile),
        )
        .route("/v1/terrain/meta", get(routes::terrain::meta))
        .route("/v1/terrain/{z}/{x}/{y}", get(routes::terrain::tile))
        .route("/v1/stream", get(routes::stream::stream))
        .route("/v1/devices", get(routes::pairing::list))
        .route("/v1/devices/{device_id}", delete(routes::pairing::revoke))
        .route("/v1/pair/code", post(routes::pairing::new_code))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_device,
        ));

    let public = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/pair", post(routes::pairing::pair));

    public
        .merge(protected)
        .layer(cors(&state.config.allowed_origins))
        .layer(tower_http::compression::CompressionLayer::new())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Origins are opt-in.
///
/// A permissive CORS policy on a daemon holding the operator's API keys would
/// mean any page they happen to visit could read their entire feed history from
/// the browser they are already authenticated in.
fn cors(allowed: &[String]) -> CorsLayer {
    let mut layer = CorsLayer::new()
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::DELETE,
        ])
        .allow_headers([axum::http::header::AUTHORIZATION, axum::http::header::CONTENT_TYPE]);
    for origin in allowed {
        if let Ok(value) = origin.parse::<axum::http::HeaderValue>() {
            layer = layer.allow_origin(value);
        } else {
            tracing::warn!(origin, "ignoring an unparseable allowed origin");
        }
    }
    layer
}

#[derive(serde::Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
    started_at: chrono::DateTime<chrono::Utc>,
    uptime_seconds: i64,
    /// Whether this instance will accept an unauthenticated request from
    /// loopback. Stated plainly so an operator checking their exposure does not
    /// have to infer it.
    loopback_exempt: bool,
}

/// Liveness, and only liveness.
///
/// Deliberately says nothing about the database or the feeds: those are what
/// `/v1/sources` is for, and a health check that fails when one feed is down is
/// a health check that gets ignored.
async fn health(axum::extract::State(state): axum::extract::State<ApiState>) -> axum::Json<Health> {
    axum::Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        started_at: state.started_at,
        uptime_seconds: (chrono::Utc::now() - state.started_at).num_seconds(),
        loopback_exempt: state.config.auth == AuthMode::LoopbackExempt,
    })
}
