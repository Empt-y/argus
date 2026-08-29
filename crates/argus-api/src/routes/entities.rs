//! `/v1/entities` — the DVR, expressed as three endpoints.
//!
//! `?at=` is the whole point. A client that can build a viewport request can
//! time-travel by adding one parameter, and nothing else about it changes: same
//! shape in, same shape out. That is what makes the Android scrubber a slider
//! wired to a query string rather than a feature.

use crate::error::{ApiError, ApiResult};
use crate::params::{ViewportQuery, parse_instant};
use crate::ApiState;
use argus_core::entity::EntityId;
use axum::extract::{Path, Query, State};
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct EntitiesResponse {
    /// The instant this answer describes. Echoed back so a client can label
    /// what it is showing without re-deriving it, and so a live response is
    /// self-describing rather than implicitly "now".
    pub at: DateTime<Utc>,
    pub live: bool,
    pub count: usize,
    /// True when the result hit the limit, so a client knows it is seeing a
    /// truncated view rather than an empty sky. Silently truncating is how a
    /// map convinces someone a region is quiet.
    pub truncated: bool,
    pub entities: Vec<argus_store::EntityRow>,
}

pub async fn list(
    State(state): State<ApiState>,
    Query(query): Query<ViewportQuery>,
) -> ApiResult<Json<EntitiesResponse>> {
    let bbox = query.bbox()?;
    let filter = query.filter()?;
    let limit = query.limit();

    let (at, entities) = match query.at()? {
        Some(at) => (at, state.store.entities_at(bbox, at, &filter, limit).await?),
        None => (
            Utc::now(),
            state.store.entities_in_bbox(bbox, &filter, limit).await?,
        ),
    };

    Ok(Json(EntitiesResponse {
        at,
        live: query.at.is_none(),
        count: entities.len(),
        truncated: entities.len() as i64 >= limit,
        entities,
    }))
}

pub async fn detail(
    State(state): State<ApiState>,
    Path((kind, key)): Path<(String, String)>,
) -> ApiResult<Json<argus_store::EntityRow>> {
    let id = entity_id(&kind, &key)?;
    state
        .store
        .entity(&id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("no {kind} with key '{key}'")))
}

#[derive(Debug, Deserialize)]
pub struct TrackQuery {
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Serialize)]
pub struct TrackResponse {
    pub entity: String,
    pub kind: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub points: Vec<argus_store::model::TrackPoint>,
}

/// One entity's history.
///
/// Defaults to the last two hours rather than to everything: an unbounded track
/// on a satellite that has been propagated for weeks is a hundred thousand
/// points, and no client asked for that by omitting a parameter.
pub async fn track(
    State(state): State<ApiState>,
    Path((kind, key)): Path<(String, String)>,
    Query(query): Query<TrackQuery>,
) -> ApiResult<Json<TrackResponse>> {
    let id = entity_id(&kind, &key)?;
    let to = query
        .to
        .as_deref()
        .map(parse_instant)
        .transpose()?
        .unwrap_or_else(Utc::now);
    let from = query
        .from
        .as_deref()
        .map(parse_instant)
        .transpose()?
        .unwrap_or(to - Duration::hours(2));
    if from >= to {
        return Err(ApiError::BadRequest(
            "'from' must be before 'to'".into(),
        ));
    }

    let points = state.store.track(&id, from, to).await?;
    Ok(Json(TrackResponse {
        entity: id.key,
        kind: kind.clone(),
        from,
        to,
        points,
    }))
}

fn entity_id(kind: &str, key: &str) -> Result<EntityId, ApiError> {
    let kind = argus_store::model::parse_entity_kind(kind).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "'{kind}' is not an entity kind; expected one of \
             aircraft, vessel, satellite, event, station, feature, measure"
        ))
    })?;
    Ok(EntityId::new(kind, key))
}
