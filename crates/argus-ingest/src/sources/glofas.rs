//! River discharge from the GloFAS model, sampled at England's river
//! gauges through Open-Meteo's flood API.
//!
//! GloFAS (the Global Flood Awareness System, Copernicus) routes rainfall
//! through a 5 km river network and publishes the discharge on every
//! cell, daily. Open-Meteo serves it through the same point API as its
//! weather grids — ask for a point, get the cell containing it — and a
//! lattice would be the wrong way to ask: most cells hold no river worth
//! the name, and a discharge of 0.16 m³/s in a field says nothing. The
//! points to ask about are where rivers are, and the Environment Agency's
//! river-level gauge list is exactly that: 2,307 stations, every one with
//! a river name, in 1,078 tenth-degree cells. The driver reads that list
//! (300 KB, one request), collapses it to cells, and asks the flood API
//! for the discharge at each, today and the seven days before, a hundred
//! points a request. The cell is labelled with the rivers the gauges in
//! it stand on, so a sample reads "River Thames at Kingston" rather than
//! as a coordinate.
//!
//! Model output, marked [`Quality::Modeled`], dated by the model day.
//! Daily: GloFAS runs once a day. About 1,100 of Open-Meteo's 10,000
//! free calls, on top of the air and sea lattices' 6,700 — and since a
//! hundred points count as a hundred calls against the 600-a-minute
//! limit too, the batches go out ten seconds apart.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Quota, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use std::collections::BTreeMap;

const GAUGES_URL: &str = "https://environment.data.gov.uk/flood-monitoring/id/stations?parameter=level&type=SingleLevel&_limit=10000";
const FLOOD_URL: &str = "https://flood-api.open-meteo.com/v1/flood";

const CADENCE_SECS: u64 = 24 * 3600;

/// Gauges are collapsed to cells of this size before asking. GloFAS is
/// 0.05°; two gauges five kilometres apart on the same river read the
/// same reach.
const CELL_DEG: f64 = 0.1;

/// Points per flood-API request. A hundred is a 3 KB URL.
const BATCH: usize = 100;

/// Days of history asked for alongside today, for the trend.
const PAST_DAYS: u32 = 7;

/// Open-Meteo counts a request of a hundred points as a hundred calls
/// against its 600-a-minute limit, so a batch every ten seconds is the
/// most it allows; eleven batches ran into 429s when sent back to back.
const BATCH_GAP: std::time::Duration = std::time::Duration::from_secs(10);

pub struct RiverDischarge {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl RiverDischarge {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("glofas"),
                layer_id: LayerId::new("river-discharge"),
                display_name: "River discharge, modelled (GloFAS via Open-Meteo)".into(),
                kind: EntityKind::Measure,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "Open-Meteo / Copernicus GloFAS; gauge sites from the Environment Agency".into(),
                    url: "https://open-meteo.com/en/docs/flood-api".into(),
                    license: "CC BY 4.0, non-commercial".into(),
                    notice: Some("River discharge by Open-Meteo.com from Copernicus GloFAS; sampled at Environment Agency gauge sites (OGL v3)".into()),
                },
                base_quality: Quality::Modeled,
                quota: Some(Quota {
                    limit: 10_000,
                    window: std::time::Duration::from_secs(86_400),
                    cost_per_poll: 1_100,
                }),
            },
            http,
        }
    }
}

#[async_trait::async_trait]
impl Source for RiverDischarge {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let gauges: Envelope = self.http.get_json(GAUGES_URL).await?;
        let cells = cells(&gauges.items);
        if cells.is_empty() {
            return Err(SourceError::Decode("the gauge list had no station with a river and a position".into()));
        }
        let now = Utc::now();
        let mut out = Vec::with_capacity(cells.len());
        let mut failed = 0;
        let mut empty = 0;
        for (i, batch) in cells.chunks(BATCH).enumerate() {
            if i > 0 {
                tokio::time::sleep(BATCH_GAP).await;
            }
            let lats: Vec<String> = batch.iter().map(|c| format!("{:.3}", c.lat)).collect();
            let lons: Vec<String> = batch.iter().map(|c| format!("{:.3}", c.lon)).collect();
            let url = format!(
                "{FLOOD_URL}?latitude={}&longitude={}&daily=river_discharge&past_days={PAST_DAYS}&forecast_days=1&timezone=UTC",
                lats.join(","),
                lons.join(",")
            );
            let bytes = match self.http.get_bytes(&url).await {
                Ok(b) => b,
                Err(err) => {
                    failed += 1;
                    tracing::warn!(source = %self.descriptor.id, %err, "flood batch failed");
                    continue;
                }
            };
            let answers: Vec<Answer> = match serde_json::from_slice::<Vec<Answer>>(&bytes) {
                Ok(a) => a,
                Err(_) => match serde_json::from_slice::<Answer>(&bytes) {
                    Ok(a) => vec![a],
                    Err(err) => {
                        failed += 1;
                        tracing::warn!(source = %self.descriptor.id, %err, "flood batch did not decode");
                        continue;
                    }
                },
            };
            let (obs, e) = decode(batch, &answers, &self.descriptor.id, now);
            empty += e;
            out.extend(obs);
        }
        tracing::info!(source = %self.descriptor.id, gauges = gauges.items.len(), cells = cells.len(), written = out.len(), empty, failed, "river discharge read");
        if out.is_empty() && failed > 0 {
            return Err(SourceError::Transport(format!("{failed} flood batches failed and none answered")));
        }
        Ok(out)
    }
}

// --- the gauge list ------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    items: Vec<Gauge>,
}

/// The EA's JSON-LD flattens a repeated value into an array, on any field.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    fn first(&self) -> Option<&T> {
        match self {
            Self::One(v) => Some(v),
            Self::Many(v) => v.first(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Gauge {
    label: Option<OneOrMany<String>>,
    lat: Option<OneOrMany<f64>>,
    long: Option<OneOrMany<f64>>,
    #[serde(rename = "riverName")]
    river_name: Option<OneOrMany<String>>,
    town: Option<OneOrMany<String>>,
}

/// One cell to ask about: its centre and the rivers gauged in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub lat: f64,
    pub lon: f64,
    /// River names, most gauged first.
    pub rivers: Vec<String>,
    /// "River Thames at Kingston" for the best-known gauge, for the label.
    pub place: Option<String>,
    pub gauges: usize,
}

fn cells(gauges: &[Gauge]) -> Vec<Cell> {
    let mut by_cell: BTreeMap<(i64, i64), Cell> = BTreeMap::new();
    let mut river_counts: BTreeMap<(i64, i64), BTreeMap<String, usize>> = BTreeMap::new();
    for g in gauges {
        let (Some(lat), Some(lon)) = (g.lat.as_ref().and_then(|v| v.first()).copied(), g.long.as_ref().and_then(|v| v.first()).copied()) else { continue };
        let Some(river) = g.river_name.as_ref().and_then(|v| v.first()).map(|s| s.trim()).filter(|s| !s.is_empty()) else { continue };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            continue;
        }
        let key = ((lat / CELL_DEG).floor() as i64, (lon / CELL_DEG).floor() as i64);
        let cell = by_cell.entry(key).or_insert_with(|| Cell {
            lat: (key.0 as f64 + 0.5) * CELL_DEG,
            lon: (key.1 as f64 + 0.5) * CELL_DEG,
            rivers: Vec::new(),
            place: None,
            gauges: 0,
        });
        cell.gauges += 1;
        *river_counts.entry(key).or_default().entry(river.to_string()).or_default() += 1;
        if cell.place.is_none() {
            let town = g.town.as_ref().and_then(|v| v.first()).map(|s| s.trim()).filter(|s| !s.is_empty());
            let label = g.label.as_ref().and_then(|v| v.first()).map(|s| s.trim()).filter(|s| !s.is_empty());
            cell.place = match (town, label) {
                (Some(t), _) => Some(format!("{river} at {t}")),
                (None, Some(l)) => Some(format!("{river} at {l}")),
                (None, None) => Some(river.to_string()),
            };
        }
    }
    for (key, cell) in by_cell.iter_mut() {
        let mut rivers: Vec<(String, usize)> = river_counts.remove(key).unwrap_or_default().into_iter().collect();
        rivers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        cell.rivers = rivers.into_iter().map(|(r, _)| r).collect();
    }
    by_cell.into_values().collect()
}

// --- the flood API -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Answer {
    latitude: f64,
    longitude: f64,
    #[serde(default)]
    daily: Daily,
}

#[derive(Debug, Deserialize, Default)]
struct Daily {
    #[serde(default)]
    time: Vec<String>,
    #[serde(default)]
    river_discharge: Vec<Option<f64>>,
}

/// Answers come back in the order asked, so the i-th answer is the i-th
/// cell; the model's own cell centre is what is stored.
fn decode(asked: &[Cell], answers: &[Answer], source_id: &SourceId, now: DateTime<Utc>) -> (Vec<Observation>, usize) {
    let mut out = Vec::with_capacity(answers.len());
    let mut empty = 0;
    let mut seen = std::collections::HashSet::new();
    for (i, a) in answers.iter().enumerate() {
        let Some(cell) = asked.get(i) else { break };
        let series: Vec<(NaiveDate, f64)> = a
            .daily
            .time
            .iter()
            .zip(a.daily.river_discharge.iter())
            .filter_map(|(t, v)| Some((NaiveDate::parse_from_str(t, "%Y-%m-%d").ok()?, (*v)?)))
            .collect();
        let Some(&(day, today)) = series.last() else {
            empty += 1;
            continue;
        };
        let key = format!("flood:{:.3}:{:.3}", a.latitude, a.longitude);
        if !seen.insert(key.clone()) {
            continue;
        }
        let at = day.and_hms_opt(0, 0, 0).map(|t| t.and_utc()).unwrap_or(now).min(now);
        let week_ago = series.first().map(|(_, v)| *v);
        let peak = series.iter().map(|(_, v)| *v).fold(f64::MIN, f64::max);

        let mut attrs = serde_json::Map::new();
        attrs.insert("discharge_m3s".into(), serde_json::json!(today));
        attrs.insert("discharge_7d_m3s".into(), serde_json::json!(series.iter().map(|(_, v)| *v).collect::<Vec<_>>()));
        if let Some(w) = week_ago {
            attrs.insert("discharge_week_ago_m3s".into(), serde_json::json!(w));
        }
        attrs.insert("discharge_7d_peak_m3s".into(), serde_json::json!(peak));
        attrs.insert("model_day".into(), serde_json::json!(day.to_string()));
        attrs.insert("rivers".into(), serde_json::json!(cell.rivers));
        if let Some(p) = &cell.place {
            attrs.insert("place".into(), serde_json::json!(p));
        }
        attrs.insert("gauges_in_cell".into(), serde_json::json!(cell.gauges));

        let label = match &cell.place {
            Some(p) => format!("{p}: {} m³/s", fmt_flow(today)),
            None => format!("{} m³/s", fmt_flow(today)),
        };
        out.push(
            Observation::new(source_id.clone(), EntityId::new(EntityKind::Measure, key), at, Quality::Modeled)
                .with_position(Position { lon: a.longitude, lat: a.latitude, alt_m: None, datum: AltitudeDatum::AboveGround })
                .with_label(label)
                .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    (out, empty)
}

fn fmt_flow(v: f64) -> String {
    if v >= 10.0 { format!("{v:.0}") } else { format!("{v:.1}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAUGES: &str = r#"{"items":[
      {"label":"Kingston","lat":51.4149,"long":-0.3087,"riverName":"River Thames","town":"Kingston upon Thames"},
      {"label":"Teddington Lock","lat":[51.4302,51.4303],"long":-0.3228,"riverName":"River Thames","town":"Teddington"},
      {"label":"Ham","lat":51.44,"long":-0.31,"riverName":"River Crane"},
      {"label":"No river","lat":52.0,"long":-1.0},
      {"label":"Far away","lat":54.0,"long":-2.0,"riverName":"River Lune","town":"Lancaster"}
    ]}"#;

    #[test]
    fn gauges_collapse_to_cells_named_by_their_rivers() {
        let e: Envelope = serde_json::from_str(GAUGES).unwrap();
        let c = cells(&e.items);
        assert_eq!(c.len(), 2, "three Thames-side gauges share a cell; the one without a river is not asked about");
        let thames = &c[0];
        assert_eq!(thames.gauges, 3);
        assert_eq!(thames.rivers, vec!["River Thames", "River Crane"]);
        assert_eq!(thames.place.as_deref(), Some("River Thames at Kingston upon Thames"));
        assert!((thames.lat - 51.45).abs() < 1e-9 && (thames.lon + 0.35).abs() < 1e-9, "cell centre: {thames:?}");
        assert_eq!(c[1].place.as_deref(), Some("River Lune at Lancaster"));
    }

    #[test]
    fn a_sample_is_a_modelled_measure_dated_by_the_model_day_with_its_week() {
        let e: Envelope = serde_json::from_str(GAUGES).unwrap();
        let c = cells(&e.items);
        let answers: Vec<Answer> = serde_json::from_str(r#"[
          {"latitude":51.425003,"longitude":-0.32499695,"daily":{"time":["2026-09-10","2026-09-11","2026-09-12","2026-09-13","2026-09-14","2026-09-15","2026-09-16"],"river_discharge":[3.14,1.67,0.80,0.53,0.42,0.76,0.34]}},
          {"latitude":54.025,"longitude":-2.025,"daily":{"time":["2026-09-16"],"river_discharge":[null]}}
        ]"#).unwrap();
        let now: DateTime<Utc> = "2026-09-16T12:00:00Z".parse().unwrap();
        let (obs, empty) = decode(&c, &answers, &SourceId::new("glofas"), now);
        assert_eq!(empty, 1);
        assert_eq!(obs.len(), 1);
        let o = &obs[0];
        assert_eq!(o.entity.key, "flood:51.425:-0.325");
        assert_eq!(o.quality, Quality::Modeled);
        assert_eq!(o.observed_at, "2026-09-16T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(o.attrs["discharge_m3s"], 0.34);
        assert_eq!(o.attrs["discharge_week_ago_m3s"], 3.14);
        assert_eq!(o.attrs["discharge_7d_peak_m3s"], 3.14);
        assert_eq!(o.label.as_deref(), Some("River Thames at Kingston upon Thames: 0.3 m³/s"));
    }
}
