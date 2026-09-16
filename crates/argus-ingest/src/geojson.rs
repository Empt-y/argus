//! GeoJSON geometry, converted into the `geo_types` shapes the store speaks.
//!
//! This lives here rather than in a driver because it is the second time it was
//! needed: the NWS alert feed carries inline alert polygons, and the Environment
//! Agency serves flood area outlines as standalone FeatureCollections. Both are
//! the same handful of shapes and the same conversion, and two copies of a
//! ring-winding helper is how they quietly stop agreeing.
//!
//! Only the area shapes are modelled. Every consumer so far wants a polygon —
//! an alert area, a flood extent — and a `Point` variant that nothing
//! constructs would be dead code that still has to be maintained.

use geo_types::{Coord, Geometry, LineString, MultiPolygon, Polygon};
use serde::Deserialize;

/// The area geometries the feeds actually emit.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum GeoJsonGeometry {
    Polygon {
        coordinates: Vec<Vec<[f64; 2]>>,
    },
    MultiPolygon {
        coordinates: Vec<Vec<Vec<[f64; 2]>>>,
    },
}

/// A `FeatureCollection` wrapping one or more geometries.
///
/// The Environment Agency serves each flood area this way — a collection of one
/// feature whose geometry is the outline — where NWS puts the geometry inline
/// on the alert.
#[derive(Debug, Deserialize)]
pub struct FeatureCollection {
    #[serde(default)]
    pub features: Vec<GeoJsonFeature>,
}

#[derive(Debug, Deserialize)]
pub struct GeoJsonFeature {
    pub geometry: Option<GeoJsonGeometry>,
}

impl FeatureCollection {
    /// Every geometry in the collection, merged into one shape.
    ///
    /// A flood area is one place even when it is drawn as several disjoint
    /// polygons, so the parts are combined rather than the first one being
    /// picked and the rest dropped — which would silently shrink the area a
    /// warning covers.
    pub fn merged(&self) -> Option<Geometry<f64>> {
        let mut polygons: Vec<Polygon<f64>> = Vec::new();
        for feature in &self.features {
            match feature.geometry.as_ref().and_then(convert) {
                Some(Geometry::Polygon(p)) => polygons.push(p),
                Some(Geometry::MultiPolygon(mp)) => polygons.extend(mp.0),
                _ => {}
            }
        }
        match polygons.len() {
            0 => None,
            1 => Some(Geometry::Polygon(
                polygons.into_iter().next().expect("checked length"),
            )),
            _ => Some(Geometry::MultiPolygon(MultiPolygon(polygons))),
        }
    }
}

/// Convert one GeoJSON geometry.
pub fn convert(g: &GeoJsonGeometry) -> Option<Geometry<f64>> {
    match g {
        GeoJsonGeometry::Polygon { coordinates } => {
            let (outer, holes) = coordinates.split_first()?;
            Some(Geometry::Polygon(Polygon::new(
                ring(outer),
                holes.iter().map(|h| ring(h)).collect(),
            )))
        }
        GeoJsonGeometry::MultiPolygon { coordinates } => {
            let polys: Vec<Polygon<f64>> = coordinates
                .iter()
                .filter_map(|rings| {
                    let (outer, holes) = rings.split_first()?;
                    Some(Polygon::new(
                        ring(outer),
                        holes.iter().map(|h| ring(h)).collect(),
                    ))
                })
                .collect();
            (!polys.is_empty()).then_some(Geometry::MultiPolygon(MultiPolygon(polys)))
        }
    }
}

pub fn ring(points: &[[f64; 2]]) -> LineString<f64> {
    LineString(points.iter().map(|p| Coord { x: p[0], y: p[1] }).collect())
}

/// A label anchor for an area: the area-weighted centroid of the largest
/// polygon's outer ring.
///
/// Deliberately not the mean of the vertices, which was what this did
/// first. A vertex mean is pulled toward whatever has the most vertices,
/// and coastlines and islands have the most: it put the label for the South
/// West England licence area on the Isles of Scilly and North Scotland's on
/// Arran, because those rings are drawn in far more detail than the
/// mainland. The largest polygon of a multipolygon rather than the first,
/// for the same reason — the first is whatever the publisher listed first.
/// It is still a label anchor, not the feature's location: the polygon is
/// the thing.
pub fn centroid(g: &Geometry<f64>) -> Option<(f64, f64)> {
    let polygons: Vec<&geo_types::Polygon<f64>> = match g {
        Geometry::Polygon(p) => vec![p],
        Geometry::MultiPolygon(mp) => mp.0.iter().collect(),
        _ => return None,
    };
    // Shoelace area and centroid of each exterior ring; the ring with the
    // largest area wins.
    let mut best: Option<(f64, f64, f64)> = None;
    for polygon in polygons {
        let ring = &polygon.exterior().0;
        if ring.len() < 3 {
            continue;
        }
        let (mut area2, mut cx, mut cy) = (0.0, 0.0, 0.0);
        for pair in ring.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let cross = a.x * b.y - b.x * a.y;
            area2 += cross;
            cx += (a.x + b.x) * cross;
            cy += (a.y + b.y) * cross;
        }
        if area2.abs() < f64::EPSILON {
            // Degenerate: fall back to the vertex mean for this ring.
            let n = ring.len() as f64;
            let mean = (
                ring.iter().map(|c| c.x).sum::<f64>() / n,
                ring.iter().map(|c| c.y).sum::<f64>() / n,
            );
            if best.is_none() {
                best = Some((0.0, mean.0, mean.1));
            }
            continue;
        }
        let centroid = (cx / (3.0 * area2), cy / (3.0 * area2));
        let area = area2.abs();
        if best.is_none_or(|(a, _, _)| area > a) {
            best = Some((area, centroid.0, centroid.1));
        }
    }
    best.map(|(_, x, y)| (x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_anchor_is_on_the_mainland_not_the_islands() {
        // A big square drawn with four vertices and a tiny island drawn with
        // a hundred, off to the west. A vertex mean lands on the island.
        let mut island: Vec<[f64; 2]> = (0..100)
            .map(|i| {
                let t = i as f64 / 100.0 * std::f64::consts::TAU;
                [-6.0 + 0.01 * t.cos(), 50.0 + 0.01 * t.sin()]
            })
            .collect();
        island.push(island[0]);
        let g = convert(&GeoJsonGeometry::MultiPolygon {
            coordinates: vec![
                vec![island],
                vec![vec![
                    [-3.0, 50.0],
                    [-1.0, 50.0],
                    [-1.0, 52.0],
                    [-3.0, 52.0],
                    [-3.0, 50.0],
                ]],
            ],
        })
        .unwrap();
        let (lon, lat) = centroid(&g).unwrap();
        assert!(
            (lon - -2.0).abs() < 1e-9 && (lat - 51.0).abs() < 1e-9,
            "anchor at {lon},{lat}"
        );
    }

    #[test]
    fn a_collection_of_several_polygons_becomes_one_multipolygon() {
        // A flood area drawn as three disjoint pieces is still one place, and
        // keeping only the first would shrink the warned area silently.
        let fc: FeatureCollection = serde_json::from_str(
            r#"{"type":"FeatureCollection","features":[
                 {"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}},
                 {"geometry":{"type":"Polygon","coordinates":[[[5,5],[6,5],[6,6],[5,5]]]}}
               ]}"#,
        )
        .expect("a feature collection");
        let Some(Geometry::MultiPolygon(mp)) = fc.merged() else {
            panic!("expected a multipolygon");
        };
        assert_eq!(mp.0.len(), 2);
    }

    #[test]
    fn a_multipolygon_feature_is_flattened_rather_than_nested() {
        let fc: FeatureCollection = serde_json::from_str(
            r#"{"type":"FeatureCollection","features":[
                 {"geometry":{"type":"MultiPolygon","coordinates":[
                    [[[0,0],[1,0],[1,1],[0,0]]],
                    [[[5,5],[6,5],[6,6],[5,5]]]]}}
               ]}"#,
        )
        .expect("a feature collection");
        let Some(Geometry::MultiPolygon(mp)) = fc.merged() else {
            panic!("expected a multipolygon");
        };
        assert_eq!(mp.0.len(), 2, "the parts are lifted, not wrapped again");
    }

    #[test]
    fn a_feature_with_no_geometry_is_skipped_not_fatal() {
        let fc: FeatureCollection = serde_json::from_str(
            r#"{"type":"FeatureCollection","features":[
                 {"geometry":null},
                 {"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}}
               ]}"#,
        )
        .expect("a feature collection");
        assert!(matches!(fc.merged(), Some(Geometry::Polygon(_))));
    }

    #[test]
    fn an_empty_collection_has_no_geometry() {
        let fc: FeatureCollection =
            serde_json::from_str(r#"{"type":"FeatureCollection","features":[]}"#)
                .expect("a feature collection");
        assert!(fc.merged().is_none());
    }

    #[test]
    fn holes_survive_the_conversion() {
        let g: GeoJsonGeometry = serde_json::from_str(
            r#"{"type":"Polygon","coordinates":[
                 [[0,0],[10,0],[10,10],[0,10],[0,0]],
                 [[2,2],[3,2],[3,3],[2,2]]]}"#,
        )
        .expect("a polygon");
        let Some(Geometry::Polygon(p)) = convert(&g) else {
            panic!("expected a polygon");
        };
        assert_eq!(
            p.interiors().len(),
            1,
            "an island inside a flood area is not flooded"
        );
    }

    #[test]
    fn the_centroid_anchors_inside_the_shape() {
        let g: GeoJsonGeometry = serde_json::from_str(
            r#"{"type":"Polygon","coordinates":[[[0,0],[10,0],[10,10],[0,10],[0,0]]]}"#,
        )
        .expect("a polygon");
        let (lon, lat) = centroid(&convert(&g).expect("converted")).expect("a centroid");
        assert!((0.0..=10.0).contains(&lon) && (0.0..=10.0).contains(&lat));
    }
}
