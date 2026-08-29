//! `/v1/pair` and `/v1/devices` — bringing a phone onto the server.
//!
//! The flow: `argusd` prints a QR code on its console containing the server's
//! URL and a one-time code. The phone scans it and POSTs here, and gets back the
//! only copy of its bearer token that will ever exist.
//!
//! This endpoint is the one unauthenticated write in the whole API, which is
//! why the code is single-use, short-lived, and never persisted.

use crate::error::{ApiError, ApiResult};
use crate::{ApiState, Caller};
use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct PairRequest {
    /// The code from the QR.
    pub code: String,
    /// What to call this device in the device list. A phone that cannot be
    /// told apart from a wall display is a phone that cannot be revoked with
    /// confidence.
    pub name: String,
}

#[derive(Serialize)]
pub struct PairResponse {
    pub device_id: uuid::Uuid,
    pub name: String,
    /// The bearer token. Shown exactly once — only its hash is stored, so a
    /// client that loses this must pair again.
    pub token: String,
    pub scopes: Vec<String>,
}

pub async fn pair(
    State(state): State<ApiState>,
    Json(request): Json<PairRequest>,
) -> ApiResult<Json<PairResponse>> {
    if !state.pairing.redeem(request.code.trim()) {
        // One message for every failure mode — wrong, expired, already used.
        // Distinguishing them would only help someone guessing.
        return Err(ApiError::Forbidden(
            "pairing code is unknown, expired or already used".into(),
        ));
    }
    let name = request.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("device name cannot be empty".into()));
    }

    let scopes = vec!["read".to_string(), "write".to_string()];
    let issued = state.store.create_device(name, &scopes).await?;
    tracing::info!(device = %issued.device.device_id, name, "paired a new device");

    Ok(Json(PairResponse {
        device_id: issued.device.device_id,
        name: issued.device.name,
        token: issued.token,
        scopes: issued.device.scopes,
    }))
}

#[derive(Serialize)]
pub struct DevicesResponse {
    pub devices: Vec<argus_store::model::DeviceRow>,
}

pub async fn list(State(state): State<ApiState>) -> ApiResult<Json<DevicesResponse>> {
    Ok(Json(DevicesResponse {
        devices: state.store.list_devices().await?,
    }))
}

/// Revoke a device. Idempotent — revoking an already-revoked device is a
/// success, because the caller's intent ("this phone must not work") is
/// satisfied either way.
pub async fn revoke(
    State(state): State<ApiState>,
    Extension(caller): Extension<Caller>,
    Path(device_id): Path<uuid::Uuid>,
) -> ApiResult<StatusOnly> {
    if !caller.can_write() {
        return Err(ApiError::Forbidden(
            "this device has read-only scope".into(),
        ));
    }
    let changed = state.store.revoke_device(device_id).await?;
    if changed {
        tracing::info!(%device_id, by = caller.name(), "device revoked");
    }
    Ok(StatusOnly)
}

/// A 204 with no body.
pub struct StatusOnly;

impl axum::response::IntoResponse for StatusOnly {
    fn into_response(self) -> axum::response::Response {
        axum::http::StatusCode::NO_CONTENT.into_response()
    }
}

/// Mint a pairing code. Only an already-trusted caller may do this — otherwise
/// anyone who can reach the port could mint themselves a way in.
pub async fn new_code(
    State(state): State<ApiState>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    if !caller.can_write() {
        return Err(ApiError::Forbidden(
            "this device has read-only scope".into(),
        ));
    }
    let code = state.pairing.issue();
    Ok(Json(serde_json::json!({
        "code": code,
        "pairing_url": state.pairing_url(&code),
        "expires_in_seconds": crate::auth::PAIRING_TTL.num_seconds(),
    })))
}
