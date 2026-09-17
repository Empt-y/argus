//! TopoJSON, decoded to shapes.
//!
//! TopoJSON is GeoJSON with the shared edges factored out: a topology holds
//! one list of arcs (polylines), and each polygon ring is a list of arc
//! indices, a negative index meaning the arc reversed (`~i`, so `-1` is arc
//! 0 backwards). Optionally the coordinates are quantised — integers,
//! delta-encoded along each arc, mapped back through a `transform`. IODA
//! publishes Natural Earth's admin-1 regions and countries this way, 37 MB
//! for 4,581 regions, and that is the only reason this exists; it decodes
//! what those files use — Polygon and MultiPolygon in a GeometryCollection,
//! with or without a transform — and refuses the rest by name.

use geo_types::{Coord, Geometry, LineString, MultiPolygon, Polygon};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
pub struct Topology {
    pub objects: HashMap<String, TopoObject>,
    pub arcs: Vec<Vec<[f64; 2]>>,
    #[serde(default)]
    pub transform: Option<Transform>,
}

#[derive(Debug, Deserialize)]
pub struct Transform {
    pub scale: [f64; 2],
    pub translate: [f64; 2],
}

#[derive(Debug, Deserialize)]
pub struct TopoObject {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub geometries: Vec<TopoGeometry>,
}

#[derive(Debug, Deserialize)]
pub struct TopoGeometry {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub arcs: Option<serde_json::Value>,
    #[serde(default)]
    pub properties: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    pub id: Option<serde_json::Value>,
}

/// One decoded geometry with the properties that came with it.
#[derive(Debug)]
pub struct TopoFeature {
    pub properties: serde_json::Map<String, serde_json::Value>,
    pub geometry: Geometry<f64>,
}

/// Every polygonal geometry in one object of the topology. `object` picks
/// the object by name; `None` takes the first (IODA's files hold one).
/// Geometries with no shape — a country with a null geometry — are skipped;
/// a geometry of a kind this does not decode is an error, so a file that
/// changes shape is noticed rather than quietly emptied.
pub fn features(topology: &Topology, object: Option<&str>) -> Result<Vec<TopoFeature>, String> {
    let (name, object) = match object {
        Some(name) => (name.to_string(), topology.objects.get(name).ok_or_else(|| format!("no object {name} in the topology"))?),
        None => {
            let mut names: Vec<&String> = topology.objects.keys().collect();
            names.sort();
            let name = names.first().ok_or("the topology has no objects")?;
            ((*name).clone(), &topology.objects[*name])
        }
    };
    if object.kind.as_deref() != Some("GeometryCollection") {
        return Err(format!("object {name} is a {}, not a GeometryCollection", object.kind.as_deref().unwrap_or("null")));
    }
    let arcs = decode_arcs(topology);
    let mut out = Vec::with_capacity(object.geometries.len());
    for g in &object.geometries {
        let Some(kind) = g.kind.as_deref() else { continue };
        let Some(spec) = g.arcs.as_ref() else { continue };
        let geometry = match kind {
            "Polygon" => {
                let rings: Vec<Vec<i64>> = serde_json::from_value(spec.clone()).map_err(|e| format!("Polygon arcs: {e}"))?;
                polygon(&arcs, &rings)?.map(Geometry::Polygon)
            }
            "MultiPolygon" => {
                let polys: Vec<Vec<Vec<i64>>> = serde_json::from_value(spec.clone()).map_err(|e| format!("MultiPolygon arcs: {e}"))?;
                let mut parts = Vec::with_capacity(polys.len());
                for rings in &polys {
                    if let Some(p) = polygon(&arcs, rings)? {
                        parts.push(p);
                    }
                }
                (!parts.is_empty()).then_some(Geometry::MultiPolygon(MultiPolygon(parts)))
            }
            other => return Err(format!("geometry type {other} is not decoded")),
        };
        if let Some(geometry) = geometry {
            let mut properties = g.properties.clone().unwrap_or_default();
            if let Some(id) = &g.id
                && !properties.contains_key("id")
            {
                properties.insert("id".into(), id.clone());
            }
            out.push(TopoFeature { properties, geometry });
        }
    }
    Ok(out)
}

/// Arcs as absolute coordinates: delta-decoded and de-quantised when the
/// topology has a transform, taken as they are when it does not.
fn decode_arcs(topology: &Topology) -> Vec<Vec<Coord<f64>>> {
    topology
        .arcs
        .iter()
        .map(|arc| match &topology.transform {
            Some(t) => {
                let (mut x, mut y) = (0.0, 0.0);
                arc.iter()
                    .map(|[dx, dy]| {
                        x += dx;
                        y += dy;
                        Coord { x: x * t.scale[0] + t.translate[0], y: y * t.scale[1] + t.translate[1] }
                    })
                    .collect()
            }
            None => arc.iter().map(|[x, y]| Coord { x: *x, y: *y }).collect(),
        })
        .collect()
}

/// A ring from its arc indices; consecutive arcs share their joining
/// point, which is written once.
fn ring(arcs: &[Vec<Coord<f64>>], indices: &[i64]) -> Result<LineString<f64>, String> {
    let mut coords: Vec<Coord<f64>> = Vec::new();
    for &i in indices {
        let (index, reversed) = if i < 0 { ((!i) as usize, true) } else { (i as usize, false) };
        let arc = arcs.get(index).ok_or_else(|| format!("arc {index} is out of range ({} arcs)", arcs.len()))?;
        let mut points: Vec<Coord<f64>> = if reversed { arc.iter().rev().copied().collect() } else { arc.clone() };
        if !coords.is_empty() && !points.is_empty() {
            points.remove(0);
        }
        coords.extend(points);
    }
    Ok(LineString(coords))
}

fn polygon(arcs: &[Vec<Coord<f64>>], rings: &[Vec<i64>]) -> Result<Option<Polygon<f64>>, String> {
    let mut decoded = rings.iter().map(|r| ring(arcs, r)).collect::<Result<Vec<_>, _>>()?;
    // A ring needs three distinct points to enclose anything.
    decoded.retain(|r| r.0.len() >= 4);
    if decoded.is_empty() {
        return Ok(None);
    }
    let outer = decoded.remove(0);
    Ok(Some(Polygon::new(outer, decoded)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shoelace area of a ring; the geo crate is not a dependency here.
    fn area(p: &Polygon<f64>) -> f64 {
        let c = &p.exterior().0;
        (c.windows(2).map(|w| w[0].x * w[1].y - w[1].x * w[0].y).sum::<f64>() / 2.0).abs()
    }

    // Two squares sharing an edge: arc 0 is the shared edge, so the second
    // square walks it backwards.
    const RAW: &str = r#"{
      "type": "Topology",
      "objects": {"regions": {"type": "GeometryCollection", "geometries": [
        {"type": "Polygon", "arcs": [[0, 1]], "properties": {"id": "1", "name": "West"}},
        {"type": "Polygon", "arcs": [[-1, 2]], "properties": {"id": "2", "name": "East"}},
        {"type": "MultiPolygon", "arcs": [[[3]], [[4]]], "properties": {"id": "3", "name": "Islands"}},
        {"type": null, "arcs": null, "properties": {"id": "4", "name": "Nowhere"}}
      ]}},
      "arcs": [
        [[1, 0], [1, 1]],
        [[1, 1], [0, 1], [0, 0], [1, 0]],
        [[1, 0], [2, 0], [2, 1], [1, 1]],
        [[5, 5], [6, 5], [6, 6], [5, 6], [5, 5]],
        [[8, 8], [9, 8], [9, 9], [8, 9], [8, 8]]
      ]
    }"#;

    #[test]
    fn shared_arcs_are_walked_both_ways_and_null_geometries_are_skipped() {
        let topo: Topology = serde_json::from_str(RAW).unwrap();
        let f = features(&topo, Some("regions")).unwrap();
        assert_eq!(f.len(), 3, "the null geometry is not a feature");
        assert_eq!(f[0].properties["name"], "West");
        let west = match &f[0].geometry { Geometry::Polygon(p) => p, g => panic!("{g:?}") };
        assert!((area(west) - 1.0).abs() < 1e-9);
        assert_eq!(west.exterior().0.len(), 5, "closed ring, joining points written once");
        let east = match &f[1].geometry { Geometry::Polygon(p) => p, g => panic!("{g:?}") };
        assert!((area(east) - 1.0).abs() < 1e-9);
        assert_eq!(east.exterior().0.first(), Some(&Coord { x: 1.0, y: 1.0 }), "the reversed arc starts at its far end");
        let islands = match &f[2].geometry { Geometry::MultiPolygon(m) => m, g => panic!("{g:?}") };
        assert_eq!(islands.0.len(), 2);
        assert_eq!(features(&topo, None).unwrap().len(), 3, "the first object by name");
        assert!(features(&topo, Some("nope")).is_err());
    }

    #[test]
    fn a_quantised_topology_is_delta_decoded_through_its_transform() {
        let quantised = r#"{
          "type": "Topology",
          "transform": {"scale": [0.5, 0.5], "translate": [10, 20]},
          "objects": {"o": {"type": "GeometryCollection", "geometries": [
            {"type": "Polygon", "arcs": [[0]], "properties": {"id": "q"}}
          ]}},
          "arcs": [[[0, 0], [2, 0], [0, 2], [-2, 0], [0, -2]]]
        }"#;
        let topo: Topology = serde_json::from_str(quantised).unwrap();
        let f = features(&topo, None).unwrap();
        let p = match &f[0].geometry { Geometry::Polygon(p) => p, g => panic!("{g:?}") };
        let pts: Vec<(f64, f64)> = p.exterior().0.iter().map(|c| (c.x, c.y)).collect();
        assert_eq!(pts, vec![(10.0, 20.0), (11.0, 20.0), (11.0, 21.0), (10.0, 21.0), (10.0, 20.0)]);
    }

    #[test]
    fn a_geometry_kind_this_does_not_decode_is_an_error_not_an_empty_layer() {
        let lines = r#"{"type":"Topology","objects":{"o":{"type":"GeometryCollection","geometries":[{"type":"LineString","arcs":[0]}]}},"arcs":[[[0,0],[1,1]]]}"#;
        let topo: Topology = serde_json::from_str(lines).unwrap();
        assert!(features(&topo, None).unwrap_err().contains("LineString"));
    }
}
