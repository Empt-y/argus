//! The presenter for a layer nobody has written one for, and the tail of
//! every other card: keys as words, units from the suffix, times as times.

use crate::format::{self, words};
use crate::util::row;
use crate::{Card, Row, Section, Subject};
use serde_json::{Map, Value};

/// Suffix conventions the drivers use, and the unit each means. Order
/// matters where one is a suffix of another.
const UNITS: [(&str, &str); 24] = [
    ("_gco2_kwh", "gCO₂/kWh"),
    ("_rev_per_day", "rev/day"),
    ("_arcsec", "″"),
    ("_inhg", "inHg"),
    ("_kms", "km/s"),
    ("_mhz", "MHz"),
    ("_hpa", "hPa"),
    ("_pct", "%"),
    ("_deg", "°"),
    ("_nmi", "nmi"),
    ("_km2", "km²"),
    ("_km", "km"),
    ("_kt", "kt"),
    ("_ms", "m/s"),
    ("_mw", "MW"),
    ("_hz", "Hz"),
    ("_ft", "ft"),
    ("_au", "AU"),
    ("_kg", "kg"),
    ("_mi", "mi"),
    ("_in", "in"),
    ("_c", "°C"),
    ("_m", "m"),
    ("_s", "s"),
];

/// A key as a label and unit: `wind_speed_kt` → ("Wind speed", Some("kt")).
fn label_and_unit(key: &str) -> (String, Option<&'static str>) {
    for (suffix, unit) in UNITS {
        if let Some(stem) = key.strip_suffix(suffix)
            && !stem.is_empty()
        {
            return (words(stem), Some(unit));
        }
    }
    (words(key), None)
}

/// One value as text.
pub fn value(v: &Value, unit: Option<&str>) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => (if *b { "yes" } else { "no" }).to_string(),
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            let decimals = if n.is_i64() || n.is_u64() {
                0
            } else if f.abs() >= 100.0 {
                1
            } else {
                2
            };
            
            match unit {
                Some("Hz") if f >= 1e6 => format!("{} MHz", format::num(f / 1e6, 3)),
                Some("m") if f.abs() >= 10_000.0 => format::metres(f),
                Some(u) => format!("{} {u}", format::num(f, decimals)),
                None => format::num(f, decimals),
            }
        }
        Value::String(s) => {
            if s.len() >= 10 && s.as_bytes()[4] == b'-' {
                format::when(s).unwrap_or_else(|| s.clone())
            } else {
                s.clone()
            }
        }
        Value::Array(items) => items
            .iter()
            .map(|i| value(i, None))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| {
                let (label, unit) = label_and_unit(k);
                format!("{}: {}", label.to_lowercase(), value(v, unit))
            })
            .collect::<Vec<_>>()
            .join(" · "),
    }
}

/// Rows for every attribute not in `used`, in the driver's order.
pub fn rows(attrs: &Map<String, Value>, used: &[&str]) -> Vec<Row> {
    attrs
        .iter()
        .filter(|(k, v)| !used.contains(&k.as_str()) && !v.is_null() && k.as_str() != "url")
        .filter_map(|(k, v)| {
            let (label, unit) = label_and_unit(k);
            let text = value(v, unit);
            (!text.is_empty()).then(|| row(label, text))
        })
        .collect()
}

pub fn card<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let rows = rows(attrs, used);
    // Everything is now "used": the generic card *is* the also-section.
    used.extend(attrs.keys().map(String::as_str));
    Card {
        title: subject.label.unwrap_or(subject.key).to_string(),
        subtitle: Some(words(subject.layer_id)),
        summary: None,
        sections: if rows.is_empty() {
            Vec::new()
        } else {
            vec![Section {
                heading: None,
                rows,
            }]
        },
        links: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_become_labels_with_units_from_their_suffix() {
        assert_eq!(label_and_unit("wind_speed_kt"), ("Wind speed".into(), Some("kt")));
        assert_eq!(label_and_unit("height_begin_km"), ("Height begin".into(), Some("km")));
        assert_eq!(label_and_unit("intensity_gco2_kwh"), ("Intensity".into(), Some("gCO₂/kWh")));
        assert_eq!(label_and_unit("status"), ("Status".into(), None));
        // `_s` must not eat `stations`.
        assert_eq!(label_and_unit("stations"), ("Stations".into(), None));
    }

    #[test]
    fn values_read_as_text_with_times_and_booleans_in_words() {
        assert_eq!(value(&serde_json::json!(16.0), Some("kt")), "16 kt");
        assert_eq!(value(&serde_json::json!(true), None), "yes");
        assert_eq!(value(&serde_json::json!("2026-09-16T12:00:00Z"), None), "16 Sep 12:00 UTC");
        assert_eq!(value(&serde_json::json!(465988000), Some("Hz")), "465.988 MHz");
        assert_eq!(value(&serde_json::json!(["UHF", "VHF"]), None), "UHF, VHF");
    }
}
