//! Self-hosted terrain heights.
//!
//! Cesium's terrain is normally a hosted service. This serves the same job from
//! a grid on this machine, prepared from open LiDAR — see [`argus_tiles::dem`]
//! for why that is an upgrade rather than a compromise.
//!
//! Two endpoints. `meta` tells a client what is covered and, crucially, which
//! vertical datum the numbers are in; the tile route hands back a square grid
//! of heights as raw little-endian `f32`. Raw, not JSON: a 65×65 tile is 16 KB
//! of float and 40 KB of decimal text, and the client feeds the buffer straight
//! into a typed array with no parse at all.
//!
//! A tile the grid does not wholly cover is a `204`, not a `404`. The
//! distinction matters to the caller: 204 means "not here, ask your fallback",
//! which is a normal answer at the edge of the survey, while 404 would suggest
//! the client had asked for something that does not exist.

use crate::ApiState;
use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// Heights per tile edge. 65 is the classic Cesium heightmap size: a power of
/// two plus one, so the edge vertices are shared with the neighbouring tile
/// rather than interpolated to something slightly different on each side.
pub const TILE_SIZE: usize = 65;

/// Levels below this are coarser than the grid can usefully answer, and levels
/// above it ask for more detail than the LiDAR holds. Outside the range the
/// client keeps using its global fallback.
pub const MIN_LEVEL: u32 = 8;
pub const MAX_LEVEL: u32 = 16;

#[derive(Serialize)]
pub struct TerrainMeta {
    pub available: bool,
    /// `[west, south, east, north]`, degrees.
    pub bounds: Option<[f64; 4]>,
    pub tile_size: usize,
    pub min_level: u32,
    pub max_level: u32,
    /// `"orthometric"` or `"ellipsoidal"`. A client that ignores this will draw
    /// southern England about 46 m underground.
    pub datum: Option<String>,
    pub attribution: Option<String>,
    pub ground_metres: Option<f64>,
}

pub async fn meta(State(state): State<ApiState>) -> Json<TerrainMeta> {
    let Some(dem) = state.dem.as_ref() else {
        return Json(TerrainMeta {
            available: false,
            bounds: None,
            tile_size: TILE_SIZE,
            min_level: MIN_LEVEL,
            max_level: MAX_LEVEL,
            datum: None,
            attribution: None,
            ground_metres: None,
        });
    };
    let m = dem.meta();
    Json(TerrainMeta {
        available: true,
        bounds: Some([m.west, m.south, m.east, m.north]),
        tile_size: TILE_SIZE,
        min_level: MIN_LEVEL,
        max_level: MAX_LEVEL,
        datum: Some(m.datum.clone()),
        attribution: Some(m.attribution.clone()),
        ground_metres: Some(m.ground_metres),
    })
}

/// The rectangle of a tile in Cesium's geographic tiling scheme.
///
/// Two root tiles side by side rather than one, because the scheme covers
/// -180..180 by -90..90 and keeps tiles roughly square in degrees.
fn tile_rect(z: u32, x: u32, y: u32) -> Option<(f64, f64, f64, f64)> {
    let tiles_x = 2u32.checked_pow(z)?.checked_mul(2)?;
    let tiles_y = 2u32.checked_pow(z)?;
    if x >= tiles_x || y >= tiles_y {
        return None;
    }
    let lon_span = 360.0 / f64::from(tiles_x);
    let lat_span = 180.0 / f64::from(tiles_y);
    let west = -180.0 + lon_span * f64::from(x);
    let north = 90.0 - lat_span * f64::from(y);
    Some((west, north - lat_span, west + lon_span, north))
}

pub async fn tile(
    State(state): State<ApiState>,
    Path((z, x, y)): Path<(u32, u32, u32)>,
) -> Response {
    let Some(dem) = state.dem.as_ref() else {
        return StatusCode::NO_CONTENT.into_response();
    };
    if !(MIN_LEVEL..=MAX_LEVEL).contains(&z) {
        return StatusCode::NO_CONTENT.into_response();
    }
    let Some((west, south, east, north)) = tile_rect(z, x, y) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // Whole coverage only. Half a tile of real heights and half invented would
    // be worse than deferring to the fallback for all of it.
    if !dem.covers(west, south, east, north) {
        return StatusCode::NO_CONTENT.into_response();
    }
    let Some(heights) = dem.heightmap(west, south, east, north, TILE_SIZE) else {
        return StatusCode::NO_CONTENT.into_response();
    };

    let mut body = Vec::with_capacity(heights.len() * 4);
    for h in heights {
        body.extend_from_slice(&h.to_le_bytes());
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            // The ground does not move. Anything that re-prepares the grid
            // should change the URL rather than wait this out.
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_zero_is_two_tiles_wide() {
        assert_eq!(tile_rect(0, 0, 0), Some((-180.0, -90.0, 0.0, 90.0)));
        assert_eq!(tile_rect(0, 1, 0), Some((0.0, -90.0, 180.0, 90.0)));
        assert_eq!(tile_rect(0, 2, 0), None);
        assert_eq!(tile_rect(0, 0, 1), None);
    }

    #[test]
    fn tiles_tile_without_gaps_or_overlap() {
        // Neighbours must share an edge exactly, or terrain cracks at the seam.
        let (_, _, east_of_first, _) = tile_rect(3, 5, 2).unwrap();
        let (west_of_next, _, _, _) = tile_rect(3, 6, 2).unwrap();
        assert!((east_of_first - west_of_next).abs() < 1e-12);
        let (_, south_of_first, _, _) = tile_rect(3, 5, 2).unwrap();
        let (_, _, _, north_of_below) = tile_rect(3, 5, 3).unwrap();
        assert!((south_of_first - north_of_below).abs() < 1e-12);
    }

    #[test]
    fn a_london_tile_lands_over_london() {
        // z=12 over Heathrow: the rectangle must actually contain it.
        let tiles_x = f64::from(2u32.pow(12) * 2);
        let x = (((-0.45 + 180.0) / 360.0) * tiles_x).floor() as u32;
        let y = (((90.0 - 51.47) / 180.0) * f64::from(2u32.pow(12))).floor() as u32;
        let (w, s, e, n) = tile_rect(12, x, y).unwrap();
        assert!(w <= -0.45 && -0.45 <= e, "lon {w}..{e}");
        assert!(s <= 51.47 && 51.47 <= n, "lat {s}..{n}");
    }
}
