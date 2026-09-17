//! What an entity looks like to a person.
//!
//! Every driver stores what its feed said, in the feed's own terms: `wx:
//! "-RA BR"`, `hazard: "TURB"`, `squawk: "7700"`, `data_mode: "A"`. Those
//! are the right thing to store — they are what the source asserted — and
//! the wrong thing to show. A card that prints `wind_speed_kt 16` and a
//! raw TAF is a debugging view, and both clients were showing it.
//!
//! This crate turns a layer's attributes into a [`Card`]: a title, a
//! one-line summary, and sections of labelled rows, in words and units a
//! person can read. It lives on the server for the same reason the layer
//! catalogue does: a new driver should read properly in the web client and
//! on the phone the day it lands, without either client learning what a
//! METAR is. The clients render the card and keep the raw attributes
//! behind a fold for anyone who wants them.
//!
//! Each layer with domain knowledge has a presenter; every other layer gets
//! the generic one, which prettifies keys and units well enough that
//! nothing ever falls back to a raw dump again. A presenter takes the keys
//! it understands and leaves the rest to a trailing "Also" section, so an
//! attribute a driver adds later is shown rather than lost.

mod format;
mod generic;
mod layers;
mod weather;

use serde::Serialize;

pub use format::*;

/// A card: what a client shows when an entity is tapped.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Card {
    /// What this is, for a person: "Homer Airport (PAHO)", "Bus 477 to
    /// Walnuts Centre", "Severe turbulence, Brisbane FIR".
    pub title: String,
    /// One line under the title, when there is something worth one line:
    /// the layer in words, or the state that matters most.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// The reading in a sentence, when the layer has one: "Wind 240° at 16
    /// gusting 30 kt, 10 mi, broken cloud at 3,800 ft. 10.6 °C."
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub sections: Vec<Section>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Section {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading: Option<String>,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Row {
    pub label: String,
    pub value: String,
    /// A second, quieter line under the value: the raw code behind a
    /// decoded word, a caveat, a time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Link {
    pub label: String,
    pub url: String,
}

/// What the presenter is given: everything a detail response knows.
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    pub layer_id: &'a str,
    pub kind: &'a str,
    pub key: &'a str,
    pub label: Option<&'a str>,
    pub attrs: &'a serde_json::Value,
}

/// The card for an entity.
pub fn card(subject: Subject<'_>) -> Card {
    let empty = serde_json::Map::new();
    let attrs = subject.attrs.as_object().unwrap_or(&empty);
    let mut used: Vec<&str> = Vec::new();
    let mut card = match subject.layer_id {
        "metars" => weather::metar(subject, attrs, &mut used),
        "sigmets" => weather::sigmet(subject, attrs, &mut used),
        "buoys" => layers::buoy(subject, attrs, &mut used),
        "weather-alerts" => layers::weather_alert(subject, attrs, &mut used),
        "flood-warnings" => layers::flood_warning(subject, attrs, &mut used),
        "river-gauges" => layers::river_gauge(subject, attrs, &mut used),
        "storm-overflows" => layers::storm_overflow(subject, attrs, &mut used),
        "earthquakes" => layers::earthquake(subject, attrs, &mut used),
        "flights" => layers::flight(subject, attrs, &mut used),
        "radiosondes" => layers::radiosonde(subject, attrs, &mut used),
        "satellites" => layers::satellite(subject, attrs, &mut used),
        "buses" => layers::bus(subject, attrs, &mut used),
        "argo-floats" => layers::argo(subject, attrs, &mut used),
        "meteors" => layers::meteor(subject, attrs, &mut used),
        "ground-stations" => layers::ground_station(subject, attrs, &mut used),
        "road-disruptions" => layers::road_disruption(subject, attrs, &mut used),
        "carbon-intensity" => layers::carbon(subject, attrs, &mut used),
        "offshore-platforms" => layers::platform(subject, attrs, &mut used),
        "wind-farms" => layers::wind_farm(subject, attrs, &mut used),
        "air-quality" => layers::air_quality(subject, attrs, &mut used),
        "sea-state" => layers::sea_state(subject, attrs, &mut used),
        "seismographs" => layers::seismograph(subject, attrs, &mut used),
        "street-crime" => layers::street_crime(subject, attrs, &mut used),
        "submarine-cables" => layers::cable(subject, attrs, &mut used),
        "cable-landings" => layers::cable_landing(subject, attrs, &mut used),
        "power-grid" => layers::power(subject, attrs, &mut used),
        "fires" => layers::fire(subject, attrs, &mut used),
        "fireballs" => layers::fireball(subject, attrs, &mut used),
        "airports" => layers::airport(subject, attrs, &mut used),
        "food-hygiene" => layers::food_hygiene(subject, attrs, &mut used),
        "geomagnetic-activity" => layers::geomagnetic(subject, attrs, &mut used),
        "internet-outages" => layers::internet_outage(subject, attrs, &mut used),
        "river-discharge" => layers::river_discharge(subject, attrs, &mut used),
        "hf-propagation" => layers::hf_path(subject, attrs, &mut used),
        _ => generic::card(subject, attrs, &mut used),
    };
    // Whatever the presenter did not claim is still shown, prettified, so a
    // driver can add an attribute without touching this crate and a person
    // still sees it.
    let rest = generic::rows(attrs, &used);
    if !rest.is_empty() {
        card.sections.push(Section {
            heading: Some("Also".into()),
            rows: rest,
        });
    }
    // Links the driver put in `url` are links, not text.
    if let Some(url) = attrs.get("url").and_then(|v| v.as_str())
        && card.links.iter().all(|l| l.url != url)
    {
        card.links.push(Link {
            label: "Source page".into(),
            url: url.to_string(),
        });
    }
    if card.title.is_empty() {
        card.title = subject.label.unwrap_or(subject.key).to_string();
    }
    card
}

/// Helpers shared by presenters.
pub(crate) mod util {
    use super::*;
    use serde_json::{Map, Value};

    /// A builder that records which keys it read, so the generic pass can
    /// show the rest.
    pub struct Take<'a, 'u> {
        pub attrs: &'a Map<String, Value>,
        pub used: &'u mut Vec<&'a str>,
    }

    impl<'a, 'u> Take<'a, 'u> {
        pub fn new(attrs: &'a Map<String, Value>, used: &'u mut Vec<&'a str>) -> Self {
            Self { attrs, used }
        }

        fn mark(&mut self, key: &str) -> Option<&'a Value> {
            let (k, v) = self.attrs.get_key_value(key)?;
            if !self.used.contains(&k.as_str()) {
                self.used.push(k.as_str());
            }
            Some(v)
        }

        pub fn str(&mut self, key: &str) -> Option<&'a str> {
            self.mark(key)?.as_str().map(str::trim).filter(|s| !s.is_empty())
        }

        pub fn f64(&mut self, key: &str) -> Option<f64> {
            self.mark(key)?.as_f64()
        }

        pub fn i64(&mut self, key: &str) -> Option<i64> {
            let v = self.mark(key)?;
            v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
        }

        pub fn bool(&mut self, key: &str) -> Option<bool> {
            self.mark(key)?.as_bool()
        }

        pub fn value(&mut self, key: &str) -> Option<&'a Value> {
            self.mark(key)
        }

        /// Mark a key as handled without reading it.
        pub fn skip(&mut self, key: &str) {
            let _ = self.mark(key);
        }

        pub fn strings(&mut self, key: &str) -> Vec<String> {
            self.mark(key)
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        }
    }

    pub fn row(label: impl Into<String>, value: impl Into<String>) -> Row {
        Row {
            label: label.into(),
            value: value.into(),
            note: None,
        }
    }

    pub fn row_note(label: impl Into<String>, value: impl Into<String>, note: impl Into<String>) -> Row {
        Row {
            label: label.into(),
            value: value.into(),
            note: Some(note.into()),
        }
    }

    /// Push a row only when there is a value.
    pub fn push(rows: &mut Vec<Row>, label: &str, value: Option<String>) {
        if let Some(v) = value {
            rows.push(row(label, v));
        }
    }

    pub fn section(heading: Option<&str>, rows: Vec<Row>) -> Option<Section> {
        (!rows.is_empty()).then(|| Section {
            heading: heading.map(str::to_string),
            rows,
        })
    }
}
