//! `/v1/geofences` and `/v1/alerts`.
//!
//! A geofence is a stored query over live state, so creating one is a write
//! that outlives the client that made it — which is why these are the endpoints
//! that check `can_write()` rather than trusting the reader's scope.

use crate::error::{ApiError, ApiResult};
use crate::params::parse_instant;
use crate::{ApiState, Caller};
use argus_alert::Rule;
use axum::extract::{Path, Query, State};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A geofence as a client sees it: its shape as GeoJSON, its rule as written.
#[derive(Serialize)]
pub struct GeofenceView {
    pub geofence_id: i64,
    pub name: String,
    pub geometry: Value,
    pub rule: Value,
    pub enabled: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize)]
pub struct GeofencesResponse {
    pub geofences: Vec<GeofenceView>,
}

pub async fn list(State(state): State<ApiState>) -> ApiResult<Json<GeofencesResponse>> {
    let rows = state.store.geofences(false).await?;
    Ok(Json(GeofencesResponse {
        geofences: rows
            .into_iter()
            .map(|row| GeofenceView {
                geofence_id: row.geofence_id,
                name: row.name,
                geometry: row.geom_json,
                rule: row.rule,
                enabled: row.enabled,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
            .collect(),
    }))
}

#[derive(Deserialize)]
pub struct CreateGeofence {
    pub name: String,
    /// GeoJSON geometry — a `Polygon`, not a Feature. PostGIS validates it.
    pub geometry: Value,
    #[serde(default)]
    pub rule: Value,
}

pub async fn create(
    State(state): State<ApiState>,
    Extension(caller): Extension<Caller>,
    Json(request): Json<CreateGeofence>,
) -> ApiResult<Json<GeofenceView>> {
    if !caller.can_write() {
        return Err(ApiError::Forbidden("this device has read-only scope".into()));
    }
    let name = request.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("geofence name cannot be empty".into()));
    }

    // Parsed here, before it is stored, so a typo is a 400 with the field named
    // rather than a fence that saves, looks armed, and is skipped by the engine
    // every thirty seconds for the rest of its life.
    let rule = if request.rule.is_null() {
        serde_json::json!({})
    } else {
        request.rule
    };
    serde_json::from_value::<Rule>(rule.clone())
        .map_err(|err| ApiError::BadRequest(format!("rule is not valid: {err}")))?;

    // Before the row exists, because a fence that lists as enabled and can
    // never contain anything is the failure this whole feature cannot afford.
    if let Some(reason) = state
        .store
        .validate_geofence_shape(&request.geometry)
        .await
        .map_err(map_geometry_error)?
    {
        return Err(ApiError::BadRequest(format!("geometry is not usable: {reason}")));
    }

    let row = state
        .store
        .create_geofence(name, &request.geometry, &rule)
        .await
        .map_err(map_geometry_error)?;

    tracing::info!(geofence = row.geofence_id, name, by = caller.name(), "geofence created");
    Ok(Json(GeofenceView {
        geofence_id: row.geofence_id,
        name: row.name,
        geometry: row.geom_json,
        rule: row.rule,
        enabled: row.enabled,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

#[derive(Deserialize)]
pub struct SetEnabled {
    pub enabled: bool,
}

/// Arm or disarm without deleting: a fence over an approach path is useful on
/// some days and noise on others, and the alerts it raised still reference it.
pub async fn set_enabled(
    State(state): State<ApiState>,
    Extension(caller): Extension<Caller>,
    Path(geofence_id): Path<i64>,
    Json(request): Json<SetEnabled>,
) -> ApiResult<StatusOnly> {
    if !caller.can_write() {
        return Err(ApiError::Forbidden("this device has read-only scope".into()));
    }
    if !state.store.set_geofence_enabled(geofence_id, request.enabled).await? {
        return Err(ApiError::NotFound(format!("no geofence {geofence_id}")));
    }
    Ok(StatusOnly)
}

pub async fn delete(
    State(state): State<ApiState>,
    Extension(caller): Extension<Caller>,
    Path(geofence_id): Path<i64>,
) -> ApiResult<StatusOnly> {
    if !caller.can_write() {
        return Err(ApiError::Forbidden("this device has read-only scope".into()));
    }
    // Idempotent, like device revocation: the caller's intent is "this fence
    // must not fire", and that is satisfied either way.
    state.store.delete_geofence(geofence_id).await?;
    Ok(StatusOnly)
}

#[derive(Debug, Deserialize)]
pub struct AlertQuery {
    pub since: Option<String>,
    #[serde(default)]
    pub unacknowledged: bool,
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct AlertsResponse {
    pub alerts: Vec<argus_store::model::AlertRow>,
}

pub async fn alerts(
    State(state): State<ApiState>,
    Query(query): Query<AlertQuery>,
) -> ApiResult<Json<AlertsResponse>> {
    let since = query.since.as_deref().map(parse_instant).transpose()?;
    let limit = query.limit.unwrap_or(200).clamp(1, 2_000);
    Ok(Json(AlertsResponse {
        alerts: state.store.alerts(since, query.unacknowledged, limit).await?,
    }))
}

pub async fn acknowledge(
    State(state): State<ApiState>,
    Extension(caller): Extension<Caller>,
    Path(alert_id): Path<i64>,
) -> ApiResult<StatusOnly> {
    if !caller.can_write() {
        return Err(ApiError::Forbidden("this device has read-only scope".into()));
    }
    // Idempotent: acknowledging twice is a success, and the first
    // acknowledgement's timestamp stands so two clients racing do not rewrite
    // when it was actually seen.
    state.store.acknowledge_alert(alert_id).await?;
    Ok(StatusOnly)
}

/// PostGIS rejects a malformed GeoJSON document in words worth passing on: the
/// caller drew the shape and is the only one who can fix it.
fn map_geometry_error(err: argus_store::StoreError) -> ApiError {
    if let argus_store::StoreError::Db(db) = &err
        && let Some(message) = db.as_database_error().map(|d| d.message().to_string())
    {
        return ApiError::BadRequest(format!("geometry is not usable: {message}"));
    }
    ApiError::from(err)
}

/// A 204 with no body.
pub struct StatusOnly;

impl axum::response::IntoResponse for StatusOnly {
    fn into_response(self) -> axum::response::Response {
        axum::http::StatusCode::NO_CONTENT.into_response()
    }
}
