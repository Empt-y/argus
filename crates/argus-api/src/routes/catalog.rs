//! `/v1/sources` and `/v1/layers` — what Argus is watching and how well.
//!
//! These two are what make the honesty of the health model visible. Everything
//! upstream of here has been careful to distinguish "I asked and there was
//! nothing" from "I have never had an answer" and "you have not given me a
//! key"; this is where a client can finally see it.

use crate::error::ApiResult;
use crate::{ApiState, Caller};
use argus_core::layer::LayerStyle;
use axum::extract::State;
use axum::{Extension, Json};
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize)]
pub struct SourcesResponse {
    pub sources: Vec<argus_store::model::SourceRow>,
}

pub async fn sources(State(state): State<ApiState>) -> ApiResult<Json<SourcesResponse>> {
    Ok(Json(SourcesResponse {
        sources: state.store.list_sources().await?,
    }))
}

/// A layer as a client needs it: identity, honest health, and enough style to
/// draw it without a hard-coded table in the app.
#[derive(Serialize)]
pub struct LayerView {
    pub id: String,
    pub display_name: String,
    pub kind: String,
    pub state: String,
    pub sources: Vec<String>,
    pub last_success: Option<chrono::DateTime<chrono::Utc>>,
    pub observations: i64,
    pub live_entities: i64,
    pub attribution: Value,
    pub style: LayerStyle,
    /// Whether this layer has any geometry that can be tiled. A layer of pure
    /// measurements has rows and no map presence, and a client should not
    /// request tiles for it.
    pub tileable: bool,
}

#[derive(Serialize)]
pub struct LayersResponse {
    pub layers: Vec<LayerView>,
}

pub async fn layers(State(state): State<ApiState>) -> ApiResult<Json<LayersResponse>> {
    let rows = state.store.layers().await?;
    let layers = rows
        .into_iter()
        .map(|row| {
            let kind = argus_store::model::parse_entity_kind(&row.entity_kind);
            let style = kind
                .map(|k| LayerStyle::for_layer(&row.layer_id, k))
                .unwrap_or_else(|| LayerStyle::for_kind(argus_core::EntityKind::Feature));
            LayerView {
                tileable: row.live_entities > 0 || row.observations > 0,
                id: row.layer_id,
                display_name: row.display_name,
                kind: row.entity_kind,
                state: row.state,
                sources: row.source_ids,
                last_success: row.last_success,
                observations: row.observations,
                live_entities: row.live_entities,
                attribution: row.attribution,
                style,
            }
        })
        .collect();
    Ok(Json(LayersResponse { layers }))
}

/// The two credentials that legitimately reach a browser, handed only to a
/// paired device.
///
/// Everything else in the config stays server-side forever. These two cannot:
/// CesiumJS talks to Google and Cesium ion directly for the photorealistic
/// tileset, and brokering that server-side would mean proxying every 3D tile.
/// They are protected by provider-side restriction — HTTP referrer, token
/// scope — not by secrecy, which is why handing them to a paired client is
/// acceptable and handing them to an unauthenticated one is not.
#[derive(Serialize)]
pub struct ClientKeysResponse {
    pub google_maps_api_key: Option<String>,
    pub cesium_ion_token: Option<String>,
}

pub async fn client_keys(
    State(state): State<ApiState>,
    Extension(caller): Extension<Caller>,
) -> Json<ClientKeysResponse> {
    tracing::debug!(caller = caller.name(), "issuing client keys");
    Json(ClientKeysResponse {
        google_maps_api_key: state.config.client_keys.google_maps_api_key.clone(),
        cesium_ion_token: state.config.client_keys.cesium_ion_token.clone(),
    })
}
