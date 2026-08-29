//! Query-parameter parsing shared by the read endpoints.
//!
//! Kept in one place because the same three parameters — a box, a layer/kind
//! filter, and the DVR instant — appear on `/v1/entities`, on the tile routes
//! and in the WebSocket subscribe frame, and they must mean exactly the same
//! thing in all three or the scrubber and the map will disagree.

use crate::error::ApiError;
use argus_core::geo::BoundingBox;
use argus_store::EntityFilter;
use chrono::{DateTime, Utc};
use serde::Deserialize;

/// The default cap on a single viewport response.
///
/// Generous enough for a continental view of aircraft, small enough that one
/// careless request cannot pull the whole table into memory.
pub const DEFAULT_LIMIT: i64 = 5_000;
pub const MAX_LIMIT: i64 = 50_000;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ViewportQuery {
    /// `west,south,east,north` in degrees. Absent means the whole planet.
    pub bbox: Option<String>,
    /// Comma-separated layer ids.
    pub layers: Option<String>,
    /// Comma-separated entity kinds.
    pub kinds: Option<String>,
    /// RFC 3339 instant. Absent is live; present is the DVR.
    pub at: Option<String>,
    pub limit: Option<i64>,
}

impl ViewportQuery {
    pub fn bbox(&self) -> Result<BoundingBox, ApiError> {
        match &self.bbox {
            None => Ok(BoundingBox::GLOBAL),
            Some(text) => parse_bbox(text),
        }
    }

    pub fn filter(&self) -> Result<EntityFilter, ApiError> {
        let layers = self
            .layers
            .as_deref()
            .map(split_csv)
            .unwrap_or_default();
        let mut kinds = Vec::new();
        for name in self.kinds.as_deref().map(split_csv).unwrap_or_default() {
            let kind = argus_store::model::parse_entity_kind(&name).ok_or_else(|| {
                ApiError::BadRequest(format!(
                    "'{name}' is not an entity kind; expected one of \
                     aircraft, vessel, satellite, event, station, feature, measure"
                ))
            })?;
            kinds.push(kind);
        }
        Ok(EntityFilter { layers, kinds })
    }

    pub fn at(&self) -> Result<Option<DateTime<Utc>>, ApiError> {
        self.at.as_deref().map(parse_instant).transpose()
    }

    /// Clamped, so a client asking for a million rows gets the cap rather than
    /// an error — refusing the request would just mean the client retries with
    /// a smaller number, having learnt nothing.
    pub fn limit(&self) -> i64 {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }
}

pub fn split_csv(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse `west,south,east,north`.
///
/// `west > east` is accepted rather than rejected: that is how a box crossing
/// the antimeridian is written, and the store knows how to split it. Rejecting
/// it would make the Pacific unwatchable.
pub fn parse_bbox(text: &str) -> Result<BoundingBox, ApiError> {
    let parts: Vec<f64> = text
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| {
            ApiError::BadRequest(format!("bbox '{text}' is not four numbers"))
        })?;
    let [west, south, east, north] = parts.as_slice() else {
        return Err(ApiError::BadRequest(format!(
            "bbox '{text}' needs exactly four values: west,south,east,north"
        )));
    };
    if !(-90.0..=90.0).contains(south) || !(-90.0..=90.0).contains(north) || south >= north {
        return Err(ApiError::BadRequest(format!(
            "bbox latitudes are invalid (south={south}, north={north})"
        )));
    }
    if !(-180.0..=180.0).contains(west) || !(-180.0..=180.0).contains(east) {
        return Err(ApiError::BadRequest(format!(
            "bbox longitudes are outside -180..180 (west={west}, east={east})"
        )));
    }
    Ok(BoundingBox::new(*west, *south, *east, *north))
}

pub fn parse_instant(text: &str) -> Result<DateTime<Utc>, ApiError> {
    // `+` is a space in a query string, so an RFC 3339 instant with a numeric
    // offset — `2026-08-29T12:00:00+00:00`, which is what `Date.toISOString`'s
    // equivalents produce in half the languages a client might be written in —
    // arrives here with the sign eaten. Restoring it costs nothing and turns a
    // baffling 400 into a working request. A real space cannot appear in a
    // well-formed instant, so there is nothing to lose by the substitution.
    let text = &text.replace(' ', "+");
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|err| {
            ApiError::BadRequest(format!("'{text}' is not an RFC 3339 instant: {err}"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wrapped_box_is_accepted_rather_than_rejected() {
        // 170E..-170E is Fiji, not a typo. Rejecting west > east would make a
        // whole meridian of ocean unwatchable.
        let bbox = parse_bbox("170,-20,-170,-10").expect("wrapped box");
        assert!(bbox.crosses_antimeridian());
    }

    #[test]
    fn a_box_with_south_above_north_is_rejected() {
        assert!(parse_bbox("-2,52,0.5,51").is_err());
        assert!(parse_bbox("-2,-91,0.5,51").is_err());
        assert!(parse_bbox("-200,50,0.5,51").is_err());
    }

    #[test]
    fn a_box_that_is_not_four_numbers_says_so() {
        assert!(parse_bbox("1,2,3").is_err());
        assert!(parse_bbox("1,2,3,4,5").is_err());
        assert!(parse_bbox("north,south,east,west").is_err());
    }

    #[test]
    fn an_absent_box_means_the_whole_planet() {
        let q = ViewportQuery::default();
        assert_eq!(q.bbox().unwrap(), BoundingBox::GLOBAL);
        assert_eq!(q.filter().unwrap(), argus_store::EntityFilter::default());
        assert!(q.at().unwrap().is_none());
        assert_eq!(q.limit(), DEFAULT_LIMIT);
    }

    #[test]
    fn an_unknown_kind_is_named_in_the_error_rather_than_ignored() {
        let q = ViewportQuery {
            kinds: Some("aircraft,submarine".into()),
            ..Default::default()
        };
        let err = q.filter().expect_err("submarine is not a kind");
        assert!(err.to_string().contains("submarine"), "{err}");
    }

    #[test]
    fn limits_are_clamped_rather_than_refused() {
        let huge = ViewportQuery {
            limit: Some(10_000_000),
            ..Default::default()
        };
        assert_eq!(huge.limit(), MAX_LIMIT);
        let zero = ViewportQuery {
            limit: Some(0),
            ..Default::default()
        };
        assert_eq!(zero.limit(), 1);
    }

    #[test]
    fn a_plus_offset_survives_query_string_decoding() {
        // The `+` in "+00:00" decodes to a space before it reaches us.
        let decoded = parse_instant("2026-08-29T12:00:00 00:00").expect("offset restored");
        assert_eq!(decoded.to_rfc3339(), "2026-08-29T12:00:00+00:00");
        assert_eq!(
            parse_instant("2026-08-29T12:00:00Z").unwrap(),
            decoded,
            "Z and a restored +00:00 are the same instant"
        );
    }

    #[test]
    fn csv_lists_tolerate_spaces_and_trailing_commas() {
        assert_eq!(split_csv("flights, earthquakes,"), ["flights", "earthquakes"]);
        assert!(split_csv("").is_empty());
    }
}
