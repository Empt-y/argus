//! The layer catalogue: what a client needs to draw a layer before it has seen
//! a single feature from it.
//!
//! Layers are discovered from the running source registry — a layer exists
//! because some driver feeds it — so nothing here decides *which* layers there
//! are. What lives here is the presentation contract: the geometry a layer
//! produces, the zoom below which it is not worth requesting, and a palette
//! slot. Both clients read it from `GET /v1/layers` rather than hard-coding it,
//! so a new driver lights up in the Android app without an app release.
//!
//! Style is deliberately *hints*, not a stylesheet. Handing clients a MapLibre
//! style fragment would tie the server to one renderer, and the web client is
//! CesiumJS. A colour and a geometry class are things both can act on.

use crate::entity::EntityKind;
use serde::{Deserialize, Serialize};

/// What shape a layer's features take, which is what decides whether a client
/// needs a symbol layer, a line layer or a fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeometryClass {
    /// Discrete markers: aircraft, vessels, quakes, stations.
    Point,
    /// Routes and runs: cables, transmission lines, rail.
    Line,
    /// Areas: alert polygons, fire perimeters, forecast cones.
    Area,
    /// Points and areas together — a cyclone has a centre and a cone. Clients
    /// should instantiate both a symbol and a fill layer for these.
    Mixed,
}

/// Presentation hints for one layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerStyle {
    pub geometry: GeometryClass,
    /// sRGB hex, `#rrggbb`. A starting point a client may override; the point
    /// is that two layers never collide by accident.
    pub color: String,
    /// Below this zoom the layer is either invisible or so dense as to be
    /// meaningless, and the tiler will refuse to build it. Saves a client
    /// requesting a world-wide tile of ten thousand aircraft.
    pub min_zoom: u8,
    /// Above this zoom nothing new appears; clients may overzoom the last tile.
    pub max_zoom: u8,
    /// Whether features rotate to their course/heading when drawn.
    pub rotates_with_course: bool,
}

impl LayerStyle {
    /// The fallback, derived from the entity kind alone.
    ///
    /// Every layer gets a usable style even if nobody ever adds it to
    /// [`OVERRIDES`] — a driver author should not have to touch this file for
    /// their layer to render at all.
    pub fn for_kind(kind: EntityKind) -> Self {
        let (geometry, color, min_zoom, rotates) = match kind {
            EntityKind::Aircraft => (GeometryClass::Point, "#f2c94c", 4, true),
            EntityKind::Vessel => (GeometryClass::Point, "#56ccf2", 5, true),
            EntityKind::Vehicle => (GeometryClass::Point, "#f2994a", 8, true),
            EntityKind::Satellite => (GeometryClass::Point, "#bb6bd9", 0, false),
            EntityKind::Event => (GeometryClass::Mixed, "#eb5757", 0, false),
            EntityKind::Station => (GeometryClass::Point, "#6fcf97", 6, false),
            EntityKind::Feature => (GeometryClass::Mixed, "#828282", 2, false),
            EntityKind::Measure => (GeometryClass::Point, "#f2994a", 4, false),
        };
        Self {
            geometry,
            color: color.to_string(),
            min_zoom,
            max_zoom: 16,
            rotates_with_course: rotates,
        }
    }

    /// The style for a layer: its override if it has one, otherwise the
    /// kind-derived default.
    pub fn for_layer(layer_id: &str, kind: EntityKind) -> Self {
        let mut style = Self::for_kind(kind);
        if let Some(over) = OVERRIDES.iter().find(|(id, _)| *id == layer_id) {
            (over.1)(&mut style);
        }
        style
    }
}

/// Per-layer deviations from the kind default.
///
/// Only layers that genuinely differ appear here. `weather-alerts` is an
/// `Event` layer whose meaning *is* its polygon, and it must be visible when
/// zoomed right out — the whole point of a severe-weather layer is seeing it
/// before you go looking for it.
type Override = fn(&mut LayerStyle);
static OVERRIDES: &[(&str, Override)] = &[
    ("weather-alerts", |s| {
        s.geometry = GeometryClass::Area;
        s.color = "#f2994a".into();
        s.min_zoom = 0;
        s.max_zoom = 12;
    }),
    ("earthquakes", |s| {
        s.geometry = GeometryClass::Point;
        s.color = "#eb5757".into();
        s.min_zoom = 0;
    }),
    ("satellites", |s| {
        // Orbits are propagated, not observed, so they are cheap at any zoom
        // and pointless past the scale at which a footprint fills the screen.
        s.max_zoom = 8;
    }),
    ("flights", |s| {
        s.max_zoom = 14;
    }),
    ("sigmets", |s| {
        // An area layer whose meaning is its polygon, like weather-alerts, and
        // wanted at low zoom for the same reason: an aviation hazard is
        // something to see before you go looking for it.
        s.geometry = GeometryClass::Area;
        s.color = "#eb5757".into();
        s.min_zoom = 0;
        s.max_zoom = 10;
    }),
    ("flood-warnings", |s| {
        // An area layer whose meaning is its polygon, and wanted at the lowest
        // zoom of anything here: a severe flood warning is the one thing on
        // this map you want to see without having gone looking for it.
        s.geometry = GeometryClass::Area;
        s.color = "#2f80ed".into();
        s.min_zoom = 0;
        s.max_zoom = 12;
    }),
    ("river-gauges", |s| {
        // Five and a half thousand points, so not below z7 — closer in than the
        // outfalls, which are the layer people go looking for. Blue against the
        // overflows' brown: the same rivers, measured rather than discharged
        // into.
        s.color = "#56ccf2".into();
        s.min_zoom = 7;
    }),
    ("carbon-intensity", |s| {
        // Fourteen regions that tile Great Britain: an area layer, wanted
        // from the lowest zoom because the whole point is the pattern across
        // the country, and pointless past the scale at which one region
        // fills the screen. Green, for the reading it is best when it is.
        s.geometry = GeometryClass::Area;
        s.color = "#27ae60".into();
        s.min_zoom = 0;
        s.max_zoom = 9;
    }),
    ("road-disruptions", |s| {
        // Points with an area on a quarter of them, so Mixed stays; London
        // only, so nothing to see below z8. Amber, the colour of a road sign.
        s.color = "#f5a623".into();
        s.min_zoom = 8;
    }),
    ("ground-stations", |s| {
        // Four and a half thousand worldwide, most of them dark, so from
        // z3 — the question is where the network can hear from, asked of a
        // continent. The satellites' violet, paler: the same system, seen
        // from the ground.
        s.color = "#d7a9f0".into();
        s.min_zoom = 3;
    }),
    ("meteors", |s| {
        // A line from where it lit to where it went out, ten kilometres at
        // the median, so a line layer; and four thousand a night worldwide,
        // so from z2 — the question is where the network saw the sky fall
        // last night, asked of a continent. A pale gold streak.
        s.geometry = GeometryClass::Line;
        s.color = "#ffe08a".into();
        s.min_zoom = 2;
        s.max_zoom = 12;
    }),
    ("argo-floats", |s| {
        // Four thousand across every ocean and nowhere dense, so from z2:
        // the question is what the array looks like, which is asked of a
        // basin. Deep blue, for instruments that spend nine days in ten a
        // kilometre down; the buoys' teal is the surface.
        s.color = "#3b5bdb".into();
        s.min_zoom = 2;
    }),
    ("buoys", |s| {
        // Under nine hundred points across every ocean, so visible from z3:
        // the question is "what is the sea doing off this coast", which is
        // asked at a regional scale. Teal, between the vessels' blue and the
        // gauges' cyan — the same water, stood still and measured.
        s.color = "#2dd4bf".into();
        s.min_zoom = 3;
    }),
    ("metars", |s| {
        // Five thousand aerodromes worldwide, a third of them in the United
        // States, so from z4: the question is "what is the weather along this
        // route", asked at the scale of a country. Sky blue, and paler than
        // the water layers around it — this is the air above the aerodrome,
        // not the sea beside it.
        s.color = "#90caf9".into();
        s.min_zoom = 4;
    }),
    ("buses", |s| {
        // Twenty-eight thousand on a weekday, dense along every high street,
        // so not below z8 — "where is my bus" is asked of a town, and a
        // county of them is a smear. Orange, the colour of nothing else that
        // moves here: aircraft are yellow, vessels blue, and a bus must read
        // as neither at a glance.
        s.color = "#f2994a".into();
        s.min_zoom = 8;
        s.max_zoom = 16;
    }),
    ("storm-overflows", |s| {
        // Brown, and the one layer here where that is a description rather than
        // a palette choice. Visible from z5 because the question is regional —
        // "is anything discharging into my river" is asked of a county, not a
        // street — but not lower, because fifteen thousand outfalls at national
        // zoom is a solid mass that says nothing.
        s.color = "#a1683a".into();
        s.min_zoom = 5;
    }),
    ("radiosondes", |s| {
        // Aircraft-coloured by default, which would make a balloon
        // indistinguishable from an airliner. Cyan, and visible from further
        // out than aircraft: there are only ever a few dozen airborne, so they
        // cost nothing at low zoom and are easy to lose if hidden until z4.
        s.color = "#56ccf2".into();
        s.min_zoom = 2;
    }),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_gets_a_usable_style_without_an_override() {
        for kind in [
            EntityKind::Aircraft,
            EntityKind::Vessel,
            EntityKind::Vehicle,
            EntityKind::Satellite,
            EntityKind::Event,
            EntityKind::Station,
            EntityKind::Feature,
            EntityKind::Measure,
        ] {
            let style = LayerStyle::for_layer("a-layer-nobody-listed", kind);
            assert!(style.color.starts_with('#') && style.color.len() == 7);
            assert!(style.min_zoom <= style.max_zoom);
        }
    }

    #[test]
    fn an_override_wins_over_the_kind_default() {
        // weather-alerts is an Event, whose default geometry is Mixed; the
        // override makes it an area, because the polygon is the whole message.
        let default = LayerStyle::for_kind(EntityKind::Event);
        assert_eq!(default.geometry, GeometryClass::Mixed);
        let alerts = LayerStyle::for_layer("weather-alerts", EntityKind::Event);
        assert_eq!(alerts.geometry, GeometryClass::Area);
    }

    #[test]
    fn overrides_never_invert_the_zoom_range() {
        for (id, _) in OVERRIDES {
            for kind in [EntityKind::Aircraft, EntityKind::Event, EntityKind::Feature] {
                let s = LayerStyle::for_layer(id, kind);
                assert!(s.min_zoom <= s.max_zoom, "{id} has an inverted zoom range");
            }
        }
    }
}
