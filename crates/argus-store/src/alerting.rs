//! Geofences and the alerts they raise.
//!
//! Both tables were in the schema from 0001 and unused until now. Two decisions
//! in that DDL turn out to carry this whole phase:
//!
//! `geofences.rule` is `jsonb`, owned by `argus-alert`, so a new predicate is a
//! code change rather than a migration.
//!
//! `alerts.delivered_to` is a per-device array rather than a boolean, which is
//! what makes replay possible: an alert raised while a phone was off the
//! network is not lost and not re-sent to the tablet that already showed it.
//! Delivery is a fact about a device, not about the alert.

use crate::{Store, StoreError, model};
use argus_core::entity::EntityId;
use chrono::{DateTime, Utc};
use geozero::ToWkb;

impl Store {
    /// Every geofence, or only the ones currently armed.
    ///
    /// Returns both encodings of the shape. The engine wants geometry it can
    /// run point-in-polygon against without a round trip per entity; a client
    /// wants GeoJSON it can hand to a map. Fetching both in one query costs a
    /// little width on a table that holds dozens of rows, and saves either a
    /// second query or a conversion crate.
    pub async fn geofences(&self, enabled_only: bool) -> Result<Vec<model::GeofenceRow>, StoreError> {
        let rows = sqlx::query_as::<_, model::GeofenceRow>(
            r#"
            SELECT geofence_id, name, rule, enabled, created_at, updated_at,
                   geom, ST_AsGeoJSON(geom)::jsonb AS geom_json
            FROM geofences
            WHERE ($1::boolean IS NOT TRUE OR enabled)
            ORDER BY geofence_id
            "#,
        )
        .bind(enabled_only)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Check that a GeoJSON shape is an area that can actually contain
    /// something, returning the reason if not.
    ///
    /// Two things get through `ST_GeomFromGeoJSON` that must not become
    /// fences: a shape that is not an area at all (a LineString parses
    /// perfectly and can contain nothing), and a ring that is invalid — a
    /// bow-tie, a sliver of collinear points, a repeated vertex. `ST_Contains`
    /// against an invalid polygon is undefined rather than merely false, so
    /// such a fence would not simply never fire; it would be unpredictable.
    ///
    /// A fence that looks armed and is not is the one outcome this feature
    /// cannot have, so the checks happen before the row exists.
    ///
    /// Note there is deliberately no separate zero-area test. Every degenerate
    /// ring that could produce one — `[[0,0],[0,0],[0,0],[0,0]]`,
    /// `[[0,0],[1,1],[2,2],[0,0]]` — is already rejected by `ST_IsValid` as
    /// "Too few points" or "Self-intersection", so an area check would be dead
    /// code guarding nothing.
    pub async fn validate_geofence_shape(
        &self,
        geojson: &serde_json::Value,
    ) -> Result<Option<String>, StoreError> {
        let (kind, valid, reason): (Option<String>, Option<bool>, Option<String>) =
            sqlx::query_as(
                r#"
                SELECT GeometryType(g), ST_IsValid(g), ST_IsValidReason(g)
                FROM (SELECT ST_SetSRID(ST_GeomFromGeoJSON($1::text), 4326) AS g) t
                "#,
            )
            .bind(geojson)
            .fetch_one(&self.pool)
            .await?;

        let kind = kind.unwrap_or_default();
        if kind != "POLYGON" {
            return Ok(Some(format!(
                "a geofence must be a Polygon; this is a {}",
                if kind.is_empty() { "shape with no type" } else { &kind }
            )));
        }
        if valid != Some(true) {
            return Ok(Some(
                reason.unwrap_or_else(|| "the polygon is not valid".into()),
            ));
        }
        Ok(None)
    }

    /// Create a geofence from GeoJSON.
    ///
    /// The shape is converted by PostGIS rather than in Rust: `ST_GeomFromGeoJSON`
    /// already exists and reports a malformed document in words. What it does
    /// not do is refuse a well-formed but useless shape, which is what
    /// [`Self::validate_geofence_shape`] is for — call it first.
    pub async fn create_geofence(
        &self,
        name: &str,
        geojson: &serde_json::Value,
        rule: &serde_json::Value,
    ) -> Result<model::GeofenceRow, StoreError> {
        let row = sqlx::query_as::<_, model::GeofenceRow>(
            r#"
            INSERT INTO geofences (name, geom, rule)
            VALUES ($1, ST_SetSRID(ST_GeomFromGeoJSON($2::text), 4326), $3)
            RETURNING geofence_id, name, rule, enabled, created_at, updated_at,
                      geom, ST_AsGeoJSON(geom)::jsonb AS geom_json
            "#,
        )
        .bind(name)
        .bind(geojson)
        .bind(rule)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Arm or disarm a geofence without losing it.
    ///
    /// Disabling rather than deleting is the common case: a fence over an
    /// approach path is useful on some days and noise on others, and the alerts
    /// it already raised reference it.
    pub async fn set_geofence_enabled(
        &self,
        geofence_id: i64,
        enabled: bool,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE geofences SET enabled = $2, updated_at = now() WHERE geofence_id = $1",
        )
        .bind(geofence_id)
        .bind(enabled)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn delete_geofence(&self, geofence_id: i64) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM geofences WHERE geofence_id = $1")
            .bind(geofence_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Record a fired alert.
    pub async fn insert_alert(&self, alert: &NewAlert) -> Result<model::AlertRow, StoreError> {
        let position = alert
            .lon
            .zip(alert.lat)
            .and_then(|(lon, lat)| {
                geo_types::Geometry::Point(geo_types::Point::new(lon, lat))
                    .to_ewkb(geozero::CoordDimensions::xy(), Some(4326))
                    .ok()
            });

        let row = sqlx::query_as::<_, model::AlertRow>(
            r#"
            INSERT INTO alerts (geofence_id, entity_kind, entity_key, fired_at,
                                severity, message, position, attrs)
            VALUES ($1, $2::entity_kind, $3, $4, $5, $6,
                    CASE WHEN $7::bytea IS NULL THEN NULL ELSE ST_GeomFromEWKB($7) END,
                    $8)
            RETURNING alert_id, geofence_id, entity_kind, entity_key, fired_at,
                      severity, message, attrs, acknowledged_at, delivered_to,
                      ST_X(position) AS lon, ST_Y(position) AS lat
            "#,
        )
        .bind(alert.geofence_id)
        .bind(alert.entity.kind.as_str())
        .bind(&alert.entity.key)
        .bind(alert.fired_at)
        .bind(&alert.severity)
        .bind(&alert.message)
        .bind(position)
        .bind(&alert.attrs)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Alert history, newest first.
    pub async fn alerts(
        &self,
        since: Option<DateTime<Utc>>,
        unacknowledged_only: bool,
        limit: i64,
    ) -> Result<Vec<model::AlertRow>, StoreError> {
        let rows = sqlx::query_as::<_, model::AlertRow>(
            r#"
            SELECT alert_id, geofence_id, entity_kind, entity_key, fired_at,
                   severity, message, attrs, acknowledged_at, delivered_to,
                   ST_X(position) AS lon, ST_Y(position) AS lat
            FROM alerts
            WHERE ($1::timestamptz IS NULL OR fired_at > $1)
              AND ($2::boolean IS NOT TRUE OR acknowledged_at IS NULL)
            ORDER BY fired_at DESC
            LIMIT $3
            "#,
        )
        .bind(since)
        .bind(unacknowledged_only)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Acknowledge an alert. Idempotent: the first acknowledgement stands, so
    /// two clients racing do not rewrite when it was seen.
    pub async fn acknowledge_alert(&self, alert_id: i64) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE alerts SET acknowledged_at = now()
             WHERE alert_id = $1 AND acknowledged_at IS NULL",
        )
        .bind(alert_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// What a device missed while it was away.
    ///
    /// This is the whole point of tracking delivery per device. A phone that
    /// loses signal over a geofence and reconnects ten minutes later should be
    /// told what happened, and a phone that was never away should not be told
    /// twice.
    pub async fn alerts_undelivered_to(
        &self,
        device: &str,
        limit: i64,
    ) -> Result<Vec<model::AlertRow>, StoreError> {
        let rows = sqlx::query_as::<_, model::AlertRow>(
            r#"
            SELECT alert_id, geofence_id, entity_kind, entity_key, fired_at,
                   severity, message, attrs, acknowledged_at, delivered_to,
                   ST_X(position) AS lon, ST_Y(position) AS lat
            FROM alerts
            WHERE NOT (delivered_to @> to_jsonb($1::text))
              AND acknowledged_at IS NULL
            ORDER BY fired_at ASC
            LIMIT $2
            "#,
        )
        .bind(device)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Note that a device has been shown an alert.
    ///
    /// Appends only if absent, so a client that reconnects mid-replay and gets
    /// the same alert twice does not grow the array without bound.
    pub async fn mark_alert_delivered(
        &self,
        alert_id: i64,
        device: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            UPDATE alerts
            SET delivered_to = delivered_to || to_jsonb($2::text)
            WHERE alert_id = $1 AND NOT (delivered_to @> to_jsonb($2::text))
            "#,
        )
        .bind(alert_id)
        .bind(device)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// An alert about to be written.
#[derive(Debug, Clone)]
pub struct NewAlert {
    pub geofence_id: Option<i64>,
    pub entity: EntityId,
    pub fired_at: DateTime<Utc>,
    pub severity: String,
    pub message: String,
    pub lon: Option<f64>,
    pub lat: Option<f64>,
    pub attrs: serde_json::Value,
}
