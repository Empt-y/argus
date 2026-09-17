//! BGP hijacks, route leaks and their look-alikes, from GRIP.
//!
//! GRIP (Georgia Tech's Global Routing Intelligence Platform) watches the
//! same collectors IODA does for the routing events that matter: a prefix
//! suddenly announced by two origins (MOAS), a more-specific of someone's
//! prefix appearing from a different origin (sub-MOAS — the classic
//! hijack), an origin announcing a more-specific of its own prefix
//! (defcon), a new edge in the AS graph. Each event gets an inference — a
//! suspicion level 0–100 and a label such as incident, suspicious,
//! misconfig or legitimate — and the AS names, organisations and
//! countries of everyone involved.
//!
//! A routing event happens on a prefix, and a prefix is somewhere: the
//! event is drawn at the place RIPEstat's `geoloc` gives the first prefix
//! it names (the victim's, for a hijack). Counted over 500 events before
//! building: 270 defcon, 136 sub-MOAS, 94 MOAS, five hours' worth; 445
//! distinct first prefixes, so a fresh start asks RIPEstat about most of
//! them once; suspicion is 80 for 237 of them and 20 or below for the
//! rest, and GRIP labels the low ones legitimate — a peering change, a
//! provider's aggregate — so the layer keeps only suspicion 20 and up,
//! which is still hundreds a day. MOAS events name no victim, only the
//! two origins; a pfx_event is `prefix` for MOAS and `sub_pfx`/`super_pfx`
//! for the others. The API 301s from `/json/events` to `/v1/json/events`
//! and pages DataTables-style; `length` alone with a dedup key is enough
//! for a feed this size.

use crate::http::HttpClient;
use crate::ripestat::RipeStat;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

const API: &str = "https://api.grip.inetintel.cc.gatech.edu/v1/json/events";
const CADENCE_SECS: u64 = 10 * 60;
/// About two hours of events at the rate seen; each poll overlaps the
/// last and the key dedups.
const LENGTH: usize = 300;
/// Below this GRIP's own inference calls the event legitimate.
const MIN_SUSPICION: i64 = 20;
/// 500 events were 4.6 MB: `asinfo` repeats per event.
const MAX_BYTES: usize = 48 * 1024 * 1024;

pub struct Grip {
    descriptor: SourceDescriptor,
    http: HttpClient,
    ripestat: Arc<RipeStat>,
}

impl Grip {
    pub fn new(http: HttpClient, ripestat: Arc<RipeStat>) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("grip"),
                layer_id: LayerId::new("bgp-incidents"),
                display_name: "BGP hijacks and leaks (GRIP)".into(),
                kind: EntityKind::Event,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "GRIP, Georgia Tech Internet Intelligence Lab".into(),
                    url: "https://grip.inetintel.cc.gatech.edu/".into(),
                    license: "GRIP data, © Georgia Tech Research Corporation; free for non-commercial use with attribution".into(),
                    notice: Some("BGP event data from GRIP (Georgia Tech); prefix locations from RIPEstat".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http: http.with_max_bytes(MAX_BYTES),
            ripestat,
        }
    }
}

#[async_trait::async_trait]
impl Source for Grip {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let bytes = self.http.get_bytes(&format!("{API}?length={LENGTH}")).await?;
        let events = decode_events(&bytes)?;
        let mut observations = Vec::with_capacity(events.len());
        let (mut quiet, mut unplaced, mut no_prefix) = (0, 0, 0);
        for event in &events {
            if event.suspicion() < MIN_SUSPICION {
                quiet += 1;
                continue;
            }
            let Some(prefix) = event.summary.prefixes.first() else {
                no_prefix += 1;
                continue;
            };
            match self.ripestat.prefix_point(prefix).await? {
                Some(place) => observations.push(observation(event, &place, &self.descriptor.id)),
                None => unplaced += 1,
            }
        }
        if observations.is_empty() && events.len() > quiet {
            return Err(SourceError::Decode(format!("{} events above the suspicion floor and none could be placed", events.len() - quiet)));
        }
        tracing::info!(source = %self.descriptor.id, events = events.len(), placed = observations.len(), below_floor = quiet, unplaced, no_prefix, "routing events read");
        Ok(observations)
    }
}

// --- wire format --------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    data: Vec<Event>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub id: String,
    pub event_type: String,
    pub view_ts: i64,
    #[serde(default)]
    pub finished_ts: Option<i64>,
    #[serde(default)]
    pub summary: Summary,
    #[serde(default)]
    pub asinfo: HashMap<String, AsInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Summary {
    #[serde(default)]
    pub prefixes: Vec<String>,
    #[serde(default)]
    pub victims: Vec<String>,
    #[serde(default)]
    pub attackers: Vec<String>,
    #[serde(default)]
    pub ases: Vec<String>,
    #[serde(default)]
    pub newcomers: Vec<String>,
    #[serde(default)]
    pub tr_worthy: bool,
    #[serde(default)]
    pub inference_result: InferenceResult,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct InferenceResult {
    #[serde(default)]
    pub primary_inference: Option<Inference>,
    #[serde(default)]
    pub inferences: Vec<Inference>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Inference {
    #[serde(default)]
    pub suspicion_level: i64,
    #[serde(default)]
    pub confidence: i64,
    #[serde(default)]
    pub explanation: String,
    #[serde(default)]
    pub inference_id: String,
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AsInfo {
    #[serde(default)]
    pub asrank: Option<AsRank>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AsRank {
    #[serde(rename = "asnName", default)]
    pub name: String,
    #[serde(default)]
    pub organization: Option<Organization>,
    #[serde(default)]
    pub rank: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Organization {
    #[serde(rename = "orgName", default)]
    pub name: String,
    #[serde(default)]
    pub country: Option<Country>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Country {
    #[serde(default)]
    pub iso: String,
}

impl Event {
    /// The primary inference's suspicion, or the highest of any.
    pub fn suspicion(&self) -> i64 {
        let r = &self.summary.inference_result;
        r.primary_inference
            .as_ref()
            .map(|i| i.suspicion_level)
            .or_else(|| r.inferences.iter().map(|i| i.suspicion_level).max())
            .unwrap_or(0)
    }

    fn primary(&self) -> Option<&Inference> {
        let r = &self.summary.inference_result;
        r.primary_inference.as_ref().or_else(|| r.inferences.iter().max_by_key(|i| i.suspicion_level))
    }

    /// An AS as a card can name it.
    fn as_words(&self, asn: &str) -> serde_json::Value {
        let mut v = serde_json::json!({ "asn": asn.parse::<u64>().ok() });
        if let Some(rank) = self.asinfo.get(asn).and_then(|a| a.asrank.as_ref()) {
            if !rank.name.is_empty() {
                v["name"] = serde_json::json!(rank.name);
            }
            if let Some(org) = &rank.organization {
                if !org.name.is_empty() {
                    v["org"] = serde_json::json!(org.name);
                }
                if let Some(c) = &org.country
                    && !c.iso.is_empty()
                {
                    v["country"] = serde_json::json!(c.iso);
                }
            }
            if let Some(r) = rank.rank {
                v["rank"] = serde_json::json!(r);
            }
        }
        v
    }
}

pub fn decode_events(bytes: &[u8]) -> Result<Vec<Event>, SourceError> {
    let env: Envelope = serde_json::from_slice(bytes).map_err(|e| SourceError::Decode(format!("GRIP events: {e}")))?;
    Ok(env.data)
}

pub fn observation(event: &Event, place: &crate::ripestat::Located, source_id: &SourceId) -> Observation {
    let seen = DateTime::from_timestamp(event.view_ts, 0).unwrap_or_else(Utc::now);
    let primary = event.primary();
    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };
    put("event_type", serde_json::json!(event.event_type));
    put("suspicion", serde_json::json!(event.suspicion()));
    put("labels", serde_json::json!(primary.map(|p| p.labels.clone()).unwrap_or_default()));
    put("explanation", serde_json::json!(primary.map(|p| p.explanation.clone()).filter(|e| !e.is_empty())));
    put("confidence", serde_json::json!(primary.map(|p| p.confidence)));
    put("prefixes", serde_json::json!(event.summary.prefixes));
    put("victims", serde_json::json!(event.summary.victims.iter().map(|a| event.as_words(a)).collect::<Vec<_>>()));
    put("attackers", serde_json::json!(event.summary.attackers.iter().map(|a| event.as_words(a)).collect::<Vec<_>>()));
    put("newcomers", serde_json::json!(event.summary.newcomers.iter().map(|a| event.as_words(a)).collect::<Vec<_>>()));
    put("traceroute_worthy", serde_json::json!(event.summary.tr_worthy.then_some(true)));
    put("finished", serde_json::json!(event.finished_ts.and_then(|t| DateTime::from_timestamp(t, 0)).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))));
    put("place", serde_json::json!(if place.city.is_empty() { place.country.clone() } else { format!("{}, {}", place.city, place.country) }));
    put("place_covers_percent", serde_json::json!(place.covered_percent));
    put("url", serde_json::json!(format!("https://grip.inetintel.cc.gatech.edu/events/{}/{}", event.event_type, event.id)));
    let prefix = event.summary.prefixes.first().cloned().unwrap_or_default();
    let label = match event.event_type.as_str() {
        "moas" => format!("MOAS {prefix}"),
        "submoas" => format!("Sub-MOAS {prefix}"),
        "defcon" => format!("Defcon {prefix}"),
        "edges" => format!("New edge {prefix}"),
        other => format!("{other} {prefix}"),
    };
    Observation::new(source_id.clone(), EntityId::new(EntityKind::Event, format!("grip:{}", event.id)), seen, Quality::Live)
        .with_position(Position { lon: place.lon, lat: place.lat, alt_m: None, datum: AltitudeDatum::Geoid })
        .with_label(label)
        .with_attrs(serde_json::Value::Object(attrs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ripestat::Located;

    const EVENTS: &str = r#"{"copyright":"x","draw":0,"recordsFiltered":0,"recordsTotal":10000,"data":[
      {"id":"submoas-1789651500-137897_10753","event_type":"submoas","view_ts":1789651500,"finished_ts":null,"duration":null,"insert_ts":1,"last_modified_ts":1,"debug":{},"event_metrics":{},"tr_metrics":{},
       "pfx_events":[{"finished_ts":null,"sub_pfx":"151.242.182.0/24","super_pfx":"151.242.180.0/22","tags":[],"inferences":[{"confidence":50,"explanation":"signs of incident, but no explanation","inference_id":"unclassified","labels":["incident"],"suspicion_level":80}]}],
       "summary":{"ases":["10753","137897"],"attackers":["10753"],"victims":["137897"],"newcomers":["10753","137897"],"prefixes":["151.242.183.0/24","151.242.180.0/22"],"tags":[],"tr_worthy":false,
         "inference_result":{"inferences":[{"confidence":50,"explanation":"signs of incident, but no explanation","inference_id":"unclassified","labels":["incident"],"suspicion_level":80}],"primary_inference":{"confidence":50,"explanation":"signs of incident, but no explanation","inference_id":"unclassified","labels":["incident"],"suspicion_level":80}}},
       "asinfo":{"137897":{"asrank":{"asn":"137897","asnName":"NEXTGEN-AS-IN","organization":{"country":{"iso":"IN"},"orgId":"x","orgName":"Nextgen Broadband"},"rank":30000},"hegemony":0},"10753":{"asrank":{"asn":"10753","asnName":"LVLT-10753","organization":{"country":{"iso":"US"},"orgId":"y","orgName":"Level 3 Parent, LLC"},"rank":12},"hegemony":0.01}}},
      {"id":"moas-1789651500-154132_204966","event_type":"moas","view_ts":1789651500,"finished_ts":1789652100,
       "pfx_events":[{"finished_ts":null,"prefix":"23.226.128.0/24","tags":[],"inferences":[{"confidence":90,"explanation":"same organisation","inference_id":"same-org","labels":["legitimate"],"suspicion_level":5}]}],
       "summary":{"ases":["154132","204966"],"attackers":["204966","154132"],"victims":[],"newcomers":["204966","154132"],"prefixes":["23.226.128.0/24"],"tags":[],"tr_worthy":false,
         "inference_result":{"inferences":[],"primary_inference":{"confidence":90,"explanation":"same organisation","inference_id":"same-org","labels":["legitimate"],"suspicion_level":5}}},
       "asinfo":{}}
    ]}"#;

    #[test]
    fn events_decode_with_their_suspicion_and_the_low_one_is_legitimate() {
        let e = decode_events(EVENTS.as_bytes()).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].suspicion(), 80);
        assert_eq!(e[1].suspicion(), 5);
        assert!(e[1].suspicion() < MIN_SUSPICION);
    }

    #[test]
    fn a_hijack_is_placed_at_its_prefix_and_names_everyone_involved() {
        let e = decode_events(EVENTS.as_bytes()).unwrap();
        let place = Located { lon: 77.2, lat: 28.6, country: "IN".into(), city: "New Delhi".into(), covered_percent: 100.0 };
        let o = observation(&e[0], &place, &SourceId::new("grip"));
        assert_eq!(o.entity.key, "grip:submoas-1789651500-137897_10753");
        assert_eq!(o.entity.kind, EntityKind::Event);
        assert_eq!(o.observed_at.timestamp(), 1789651500);
        assert_eq!(o.label.as_deref(), Some("Sub-MOAS 151.242.183.0/24"));
        assert_eq!(o.attrs["suspicion"], 80);
        assert_eq!(o.attrs["labels"][0], "incident");
        assert_eq!(o.attrs["victims"][0]["name"], "NEXTGEN-AS-IN");
        assert_eq!(o.attrs["victims"][0]["country"], "IN");
        assert_eq!(o.attrs["attackers"][0]["org"], "Level 3 Parent, LLC");
        assert_eq!(o.attrs["attackers"][0]["asn"], 10753);
        assert_eq!(o.attrs["place"], "New Delhi, IN");
        assert!(o.attrs.get("finished").is_none());
        assert!(o.attrs.get("traceroute_worthy").is_none());
        let p = o.position.unwrap();
        assert!((p.lon - 77.2).abs() < 1e-9);
        let moas = observation(&e[1], &place, &SourceId::new("grip"));
        assert_eq!(moas.label.as_deref(), Some("MOAS 23.226.128.0/24"));
        assert_eq!(moas.attrs["finished"], "2026-09-17T13:35:00Z");
        assert_eq!(moas.attrs["attackers"][0]["asn"], 204966, "no asinfo still names the number");
        assert!(moas.attrs["attackers"][0].get("name").is_none());
    }
}
