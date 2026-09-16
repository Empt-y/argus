//! Server-side vector tiles.
//!
//! This crate is the reason the native Android client is affordable. MapLibre
//! Native consumes MVT directly, so every layer's rendering is a style rule
//! rather than Kotlin, and — because the tiler honours `at` — the phone gets the
//! whole DVR without knowing time travel exists. Nothing above a tile request
//! has to be time-aware.
//!
//! The encode happens here rather than in PostGIS (`ST_AsMVT`) on purpose. The
//! volumes involved are small — a dense tile is a few thousand points, tens of
//! kilobytes of WKB — and in exchange the feature cap, the zoom gating and the
//! tag vocabulary are ordinary Rust that can be tested without a database. The
//! part PostGIS *is* better at, throwing away vertices too close together to
//! see, stays in SQL where it saves the transfer as well as the encode.

pub mod dem;

use argus_core::entity::EntityKind;
use argus_core::geo::TileCoord;
use argus_core::layer::LayerStyle;
use argus_store::{EntityFilter, Store, StoreError, model::TileRow};
use chrono::{DateTime, Utc};
use geozero::mvt::{Message, TagsBuilder, TileValue, tile};
use std::collections::BTreeMap;

/// Tile-internal coordinate space. 4096 is the near-universal choice and what
/// MapLibre assumes when a style omits it.
pub const EXTENT: u32 = 4096;

/// Overdraw, in tile-coordinate units, so that a symbol or a line crossing a
/// tile edge is present in both tiles and does not flicker at the seam.
pub const BUFFER: u32 = 64;

/// Hard ceiling on features in one tile.
///
/// A z=3 tile over Europe covers every aircraft in the air there. Without a cap
/// a single pan at low zoom asks the database for a hundred thousand rows and
/// hands the phone a tile it cannot draw. The cap is a blunt instrument and is
/// meant to be: the layer's `min_zoom` is the real control, and hitting this is
/// a signal that a layer's zoom range is wrong.
pub const MAX_FEATURES: i64 = 20_000;

#[derive(Debug, thiserror::Error)]
pub enum TileError {
    #[error("tile {z}/{x}/{y} is outside the valid range for its zoom")]
    OutOfRange { z: u8, x: u32, y: u32 },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// One tile request: where, which layers, and when.
#[derive(Debug, Clone)]
pub struct TileRequest {
    pub coord: TileCoord,
    /// Empty means every layer. Clients normally name the ones their style
    /// actually draws, which keeps the tile small.
    pub filter: EntityFilter,
    /// `None` is live; `Some(t)` reads the DVR at that instant.
    pub at: Option<DateTime<Utc>>,
}

/// Builds vector tiles from the store.
#[derive(Debug, Clone)]
pub struct Tiler {
    store: Store,
    max_features: i64,
}

impl Tiler {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            max_features: MAX_FEATURES,
        }
    }

    #[must_use]
    pub fn with_max_features(mut self, max: i64) -> Self {
        self.max_features = max;
        self
    }

    /// Render one tile to encoded MVT bytes.
    ///
    /// An empty tile is a legitimate answer and encodes to an empty protobuf
    /// message, not an error. Callers should serve it with a 204 rather than a
    /// 404 — "nothing is here" and "this tile does not exist" are different
    /// things to a map client, and conflating them makes MapLibre retry
    /// forever.
    pub async fn vector_tile(&self, req: &TileRequest) -> Result<Vec<u8>, TileError> {
        if !req.coord.is_valid() {
            return Err(TileError::OutOfRange {
                z: req.coord.z,
                x: req.coord.x,
                y: req.coord.y,
            });
        }

        let bounds = req.coord.bounds();
        let width_deg = tile_width_deg(req.coord.z);
        // A vertex closer than one tile unit to its neighbour cannot be drawn
        // distinctly, so PostGIS drops it before the row is ever sent.
        let simplify_deg = width_deg / f64::from(EXTENT);
        let query_box = bounds.expanded(width_deg * f64::from(BUFFER) / f64::from(EXTENT));

        let rows = self
            .store
            .tile_rows(
                query_box,
                &req.filter,
                req.at,
                simplify_deg,
                self.max_features,
            )
            .await?;

        Ok(encode(&rows, req.coord, bounds))
    }
}

/// Web Mercator, EPSG:3857, from degrees. Latitude is clamped to the
/// projection's limit so a pole does not become infinity.
fn mercator(lon: f64, lat: f64) -> (f64, f64) {
    // atan(sinh(π)): the latitude of the world tile's edge, computed the
    // same way `TileCoord::bounds` computes it, so the two agree to the bit.
    let limit = std::f64::consts::PI.sinh().atan().to_degrees();
    let lat = lat.clamp(-limit, limit);
    let x = lon.to_radians() * 6_378_137.0;
    // asinh(tan φ) is ln(tan(π/4 + φ/2)) in a form that is odd in floating
    // point, so the two edges of the world tile are equal and opposite.
    let y = lat.to_radians().tan().asinh() * 6_378_137.0;
    (x, y)
}

/// A tile's bounds in Web Mercator metres: left, bottom, right, top.
fn mercator_bounds(bounds: argus_core::geo::BoundingBox) -> (f64, f64, f64, f64) {
    let (left, bottom) = mercator(bounds.west, bounds.south);
    let (right, top) = mercator(bounds.east, bounds.north);
    (left, bottom, right, top)
}

/// The geometry with every vertex projected.
fn to_mercator(g: &geo_types::Geometry<f64>) -> geo_types::Geometry<f64> {
    use geo::MapCoords;
    g.map_coords(|c| {
        let (x, y) = mercator(c.x, c.y);
        geo_types::Coord { x, y }
    })
}

/// Group rows into MVT layers and encode.
///
/// Split out from the query so the encoding is testable without a database —
/// which is most of why the encode lives in Rust at all.
pub fn encode(
    rows: &[TileRow],
    coord: TileCoord,
    bounds: argus_core::geo::BoundingBox,
) -> Vec<u8> {
    let mut by_layer: BTreeMap<&str, Vec<&TileRow>> = BTreeMap::new();
    for row in rows {
        let kind = match argus_store::model::parse_entity_kind(&row.entity_kind) {
            Some(k) => k,
            // A kind the database holds and this binary does not understand
            // means a rollback to an older build. Skipping the row keeps the
            // rest of the tile drawable rather than failing the whole request.
            None => continue,
        };
        // Zoom gating is applied here rather than in SQL because it depends on
        // the layer catalogue, which is Rust. A well-behaved client never asks
        // for a layer outside its range, so this normally filters nothing.
        let style = LayerStyle::for_layer(&row.layer_id, kind);
        if coord.z < style.min_zoom {
            continue;
        }
        by_layer.entry(&row.layer_id).or_default().push(row);
    }

    let mut tile = geozero::mvt::Tile::default();
    for (layer_id, layer_rows) in by_layer {
        if let Some(layer) = encode_layer(layer_id, &layer_rows, bounds) {
            tile.layers.push(layer);
        }
    }

    let mut buf = Vec::with_capacity(rows.len() * 32);
    // Encoding into a Vec cannot fail — the only error prost returns here is a
    // buffer-full one, and a Vec grows.
    tile.encode(&mut buf).expect("encoding MVT into a Vec cannot fail");
    buf
}

fn encode_layer(
    layer_id: &str,
    rows: &[&TileRow],
    bounds: argus_core::geo::BoundingBox,
) -> Option<tile::Layer> {
    use geozero::ToMvt;

    let mut tags = TagsBuilder::<String>::new();
    let mut features = Vec::with_capacity(rows.len());

    for row in rows {
        let Some(geometry) = row.geometry.geometry.as_ref() else {
            continue;
        };
        // Encoded in Web Mercator, which is what a tile is. Handing `to_mvt`
        // degrees with the tile's latitude edges maps latitude linearly
        // across the tile, and Mercator is not linear in latitude: at z2 a
        // point at 55°N landed nine degrees north of itself, and the DNO
        // regions drew a second Britain off Iceland. Negligible in a z12
        // tile a tenth of a degree tall, which is why it went unnoticed
        // until the first country-sized polygon.
        let projected = to_mercator(geometry);
        let (left, bottom, right, top) = mercator_bounds(bounds);
        let mut feature = match projected.to_mvt(EXTENT, left, bottom, right, top) {
            Ok(f) => f,
            Err(err) => {
                tracing::debug!(layer = layer_id, entity = row.entity_key, "unencodable geometry: {err}");
                continue;
            }
        };

        // A stable id lets MapLibre carry hover and selection state across the
        // tile reloads that a pan or a DVR scrub causes. Derived from the
        // natural key so it is the same in every tile and at every zoom.
        feature.id = Some(feature_id(&row.entity_kind, &row.entity_key));

        let mut put = |key: &str, value: TileValue| {
            let (k, v) = tags.insert(key.to_string(), value);
            feature.tags.push(k);
            feature.tags.push(v);
        };

        put("kind", TileValue::Str(row.entity_kind.clone()));
        put("key", TileValue::Str(row.entity_key.clone()));
        put("source", TileValue::Str(row.source_id.clone()));
        // Carried into every tile because the client must never present
        // modeled or stale data as live, and the tile is all a MapLibre style
        // has to colour by.
        put("quality", TileValue::Str(row.quality.clone()));
        put("t", TileValue::Int(row.observed_at.timestamp()));
        if let Some(label) = &row.label {
            put("label", TileValue::Str(label.clone()));
        }
        if let Some(alt) = row.alt_m {
            put("alt_m", TileValue::Double(alt));
        }
        // Course is the direction of travel and heading is where the nose
        // points; they differ in a crosswind and a style that rotates an icon
        // wants whichever the source actually measured. Both are carried rather
        // than collapsed.
        if let Some(course) = row.course_deg {
            put("course_deg", TileValue::Float(course));
        }
        if let Some(heading) = row.heading_deg {
            put("heading_deg", TileValue::Float(heading));
        }
        if let Some(speed) = row.speed_mps {
            put("speed_mps", TileValue::Float(speed));
        }
        if let Some(vrate) = row.vrate_mps {
            put("vrate_mps", TileValue::Float(vrate));
        }

        features.push(feature);
    }

    if features.is_empty() {
        return None;
    }

    let (keys, values) = tags.into_tags();
    Some(tile::Layer {
        version: 2,
        name: layer_id.to_string(),
        features,
        keys,
        values: values.into_iter().map(Into::into).collect(),
        extent: Some(EXTENT),
    })
}

/// FNV-1a over `kind:key`.
///
/// Any stable 64-bit function would do; FNV is chosen because it is four lines
/// and has no dependency, and a collision here costs a shared hover highlight
/// between two unrelated contacts, not a wrong position.
fn feature_id(kind: &str, key: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in kind.bytes().chain(b":".iter().copied()).chain(key.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Width of one tile at this zoom, in degrees of longitude.
fn tile_width_deg(z: u8) -> f64 {
    360.0 / f64::from(1u32 << z)
}

/// The kinds a layer may hold, for callers building a style document.
pub fn layer_kinds() -> &'static [EntityKind] {
    &[
        EntityKind::Aircraft,
        EntityKind::Vessel,
        EntityKind::Vehicle,
        EntityKind::Satellite,
        EntityKind::Event,
        EntityKind::Station,
        EntityKind::Feature,
        EntityKind::Measure,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use geozero::wkb;

    fn point_row(layer: &str, key: &str, lon: f64, lat: f64) -> TileRow {
        TileRow {
            entity_kind: "aircraft".into(),
            entity_key: key.into(),
            source_id: "adsb-lol".into(),
            layer_id: layer.into(),
            observed_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            geometry: wkb::Decode {
                geometry: Some(geo_types::Geometry::Point(geo_types::Point::new(lon, lat))),
            },
            alt_m: Some(10_000.0),
            course_deg: Some(271.5),
            heading_deg: None,
            speed_mps: Some(230.0),
            vrate_mps: None,
            quality: "live".into(),
            label: Some("BAW117".into()),
        }
    }

    fn decode(bytes: &[u8]) -> geozero::mvt::Tile {
        geozero::mvt::Tile::decode(bytes).expect("output should be a valid MVT tile")
    }

    #[test]
    fn a_point_lands_where_it_should_in_tile_space() {
        // Tile 0/0/0 spans the world; a point at (0, 0) is the exact centre, so
        // it must encode to the middle of the 4096-unit grid.
        let coord = TileCoord::new(0, 0, 0);
        let bounds = coord.bounds();
        // `earthquakes` rather than `flights` because a world tile of every
        // aircraft alive is exactly what the zoom gate refuses; quakes are
        // visible at z=0 by design.
        let bytes = encode(&[point_row("earthquakes", "abc123", 0.0, 0.0)], coord, bounds);
        let tile = decode(&bytes);

        assert_eq!(tile.layers.len(), 1);
        let layer = &tile.layers[0];
        assert_eq!(layer.name, "earthquakes");
        assert_eq!(layer.version, 2);
        assert_eq!(layer.extent, Some(EXTENT));
        assert_eq!(layer.features.len(), 1);

        // MoveTo command, then one zigzag-encoded pair.
        let geometry = &layer.features[0].geometry;
        assert_eq!(geometry.len(), 3);
        assert_eq!(geometry[0], 9, "one MoveTo");
        let x = (geometry[1] >> 1) as i32 ^ -((geometry[1] & 1) as i32);
        let y = (geometry[2] >> 1) as i32 ^ -((geometry[2] & 1) as i32);
        // geozero floors the scaled coordinate, and the Mercator scaling of
        // the world tile's edges lands a last bit either side of the centre.
        // One unit in 4096 is nothing on screen; exact equality was luck with
        // the old degree units, and the point of this test is the centre.
        assert!((2047..=2048).contains(&x), "x {x}");
        assert!((2047..=2048).contains(&y), "y {y}");
    }

    #[test]
    fn quality_and_key_survive_into_the_tags() {
        let coord = TileCoord::new(0, 0, 0);
        let bytes = encode(
            &[point_row("earthquakes", "abc123", 0.0, 0.0)],
            coord,
            coord.bounds(),
        );
        let tile = decode(&bytes);
        let layer = &tile.layers[0];

        let tags: Vec<(&str, &tile::Value)> = layer.features[0]
            .tags
            .chunks(2)
            .map(|pair| {
                (
                    layer.keys[pair[0] as usize].as_str(),
                    &layer.values[pair[1] as usize],
                )
            })
            .collect();

        let quality = tags.iter().find(|(k, _)| *k == "quality").expect("quality tag");
        assert_eq!(quality.1.string_value.as_deref(), Some("live"));
        let key = tags.iter().find(|(k, _)| *k == "key").expect("key tag");
        assert_eq!(key.1.string_value.as_deref(), Some("abc123"));
        // Absent optional fields are absent, not zero: a vertical rate of 0 and
        // an unreported vertical rate are different claims.
        assert!(tags.iter().all(|(k, _)| *k != "vrate_mps"));
    }

    #[test]
    fn rows_are_grouped_into_one_mvt_layer_per_argus_layer() {
        let coord = TileCoord::new(0, 0, 0);
        let rows = vec![
            point_row("earthquakes", "a", 0.0, 0.0),
            point_row("earthquakes", "b", 1.0, 1.0),
            point_row("weather-alerts", "c", 2.0, 2.0),
        ];
        let tile = decode(&encode(&rows, coord, coord.bounds()));
        assert_eq!(tile.layers.len(), 2);
        let names: Vec<&str> = tile.layers.iter().map(|l| l.name.as_str()).collect();
        assert!(names.contains(&"earthquakes") && names.contains(&"weather-alerts"));
        let quakes = tile.layers.iter().find(|l| l.name == "earthquakes").unwrap();
        assert_eq!(quakes.features.len(), 2);
    }

    #[test]
    fn a_layer_below_its_minimum_zoom_is_omitted_entirely() {
        // `flights` defaults to min_zoom 4. Asking for a world tile of every
        // aircraft alive is the request the cap exists to refuse.
        let coord = TileCoord::new(0, 0, 0);
        let tile = decode(&encode(
            &[point_row("flights", "a", 0.0, 0.0)],
            coord,
            coord.bounds(),
        ));
        assert!(tile.layers.is_empty());

        let coord = TileCoord::new(6, 32, 21);
        let bounds = coord.bounds();
        let inside = point_row(
            "flights",
            "a",
            (bounds.west + bounds.east) / 2.0,
            (bounds.south + bounds.north) / 2.0,
        );
        let tile = decode(&encode(&[inside], coord, bounds));
        assert_eq!(tile.layers.len(), 1);
    }

    #[test]
    fn feature_ids_are_stable_and_kind_scoped() {
        // The same key under two kinds is two different things, and must not
        // share a hover highlight.
        assert_eq!(feature_id("aircraft", "abc"), feature_id("aircraft", "abc"));
        assert_ne!(feature_id("aircraft", "abc"), feature_id("vessel", "abc"));
        assert_ne!(feature_id("aircraft", "abc"), feature_id("aircraft", "abd"));
    }

    #[test]
    fn an_empty_result_encodes_to_an_empty_tile_rather_than_failing() {
        let coord = TileCoord::new(6, 32, 21);
        let bytes = encode(&[], coord, coord.bounds());
        assert!(decode(&bytes).layers.is_empty());
    }

    #[test]
    fn a_row_with_no_geometry_is_skipped_without_losing_the_rest() {
        let coord = TileCoord::new(6, 32, 21);
        let bounds = coord.bounds();
        let (lon, lat) = (
            (bounds.west + bounds.east) / 2.0,
            (bounds.south + bounds.north) / 2.0,
        );
        let mut naked = point_row("flights", "no-geom", lon, lat);
        naked.geometry = wkb::Decode { geometry: None };
        let rows = vec![naked, point_row("flights", "fine", lon, lat)];
        let tile = decode(&encode(&rows, coord, bounds));
        assert_eq!(tile.layers[0].features.len(), 1);
    }

    #[test]
    fn buffered_features_encode_outside_the_extent_rather_than_being_clipped() {
        // A contact just west of the tile belongs in this tile too, at a
        // negative x, so that its icon is not cut in half at the seam.
        let coord = TileCoord::new(6, 32, 21);
        let bounds = coord.bounds();
        let just_west = bounds.west - tile_width_deg(6) * 0.005;
        let lat = (bounds.south + bounds.north) / 2.0;
        let tile = decode(&encode(
            &[point_row("flights", "edge", just_west, lat)],
            coord,
            bounds,
        ));
        let geometry = &tile.layers[0].features[0].geometry;
        let x = (geometry[1] >> 1) as i32 ^ -((geometry[1] & 1) as i32);
        assert!(x < 0, "expected a negative tile x for a buffered feature, got {x}");
    }

    #[test]
    fn latitude_is_encoded_in_mercator_not_linearly_across_the_tile() {
        // 55°N in the z2 tile spanning 0° to 66.5°: linearly that is 83%
        // of the way up, in Mercator it is 73%, and the difference is nine
        // degrees on the ground. The screenshot that found it showed the
        // DNO regions drawing a second Britain off Iceland.
        let coord = TileCoord::new(2, 1, 1);
        let bounds = coord.bounds();
        let tile = decode(&encode(&[point_row("earthquakes", "uk", -3.0, 55.0)], coord, bounds));
        let g = &tile.layers[0].features[0].geometry;
        let y = (g[2] >> 1) as i32 ^ -((g[2] & 1) as i32);
        let merc = |lat: f64| ((lat.to_radians() / 2.0 + std::f64::consts::FRAC_PI_4).tan()).ln();
        let expected = (merc(bounds.north) - merc(55.0)) / (merc(bounds.north) - merc(bounds.south))
            * f64::from(EXTENT);
        assert!((f64::from(y) - expected).abs() < 2.0, "encoded y {y}, Mercator says {expected:.0}");
        // And the top edge really is the top of the tile.
        assert_eq!(mercator_bounds(bounds).3, mercator(0.0, bounds.north).1);
    }

    #[test]
    fn tile_widths_halve_with_each_zoom_level() {
        assert_eq!(tile_width_deg(0), 360.0);
        assert_eq!(tile_width_deg(1), 180.0);
        assert!((tile_width_deg(10) - 0.3515625).abs() < 1e-12);
    }
}
