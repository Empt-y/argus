//! BGP churn at each RIS collector, from RIS Live.
//!
//! RIPE's Routing Information Service keeps route collectors at two dozen
//! internet exchanges — LINX, AMS-IX, DE-CIX, Equinix Singapore, NAP
//! Africa — each peering with dozens of networks, and RIS Live streams
//! every BGP update they hear as JSON over a websocket. All of it is far
//! too much to keep: 4,580 messages a second, 88,000 distinct prefixes in
//! twenty seconds when this was measured, and geolocating each prefix is
//! out of the question. What can be kept is the rate. A collector at an
//! exchange is a place, and how many updates it hears a minute — how many
//! prefixes announced, how many withdrawn, from how many peers — is the
//! internet's pulse taken at that place. A withdrawal storm at RRC19 in
//! Johannesburg is a story on the map even when no one prefix is.
//!
//! This is the first websocket source. The socket is held open by one
//! background task for the process's life, reconnecting with a backoff
//! when it drops; it does nothing but bump per-collector counters. Every
//! poll drains the counters into one [`EntityKind::Measure`] per
//! collector, so a poll is instant and the store sees a row a minute per
//! collector. A poll with no message heard for five minutes fails, so a
//! silent socket shows as a degraded source rather than a flat line.
//!
//! Collector positions are a table, at city precision: `rrc-info` names
//! the city and exchange for each but gives no coordinates. A collector
//! the table does not know is counted and logged, never guessed.

use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const URL: &str = "wss://ris-live.ripe.net/v1/ws/?client=argus";
const CADENCE_SECS: u64 = 60;
/// A socket that has said nothing for this long is not a quiet internet.
const SILENCE: Duration = Duration::from_secs(5 * 60);
const RECONNECT_MIN: Duration = Duration::from_secs(5);
const RECONNECT_MAX: Duration = Duration::from_secs(5 * 60);

/// A collector, where it is, and the exchange it sits at.
pub struct Collector {
    pub host: &'static str,
    pub city: &'static str,
    pub ixp: &'static str,
    pub lat: f64,
    pub lon: f64,
}

/// The active RRCs per RIPEstat `rrc-info` on 2026-09-17, with the
/// deactivated ones (RRC02, 08, 09) left out. Positions are the city.
pub const COLLECTORS: &[Collector] = &[
    Collector { host: "rrc00", city: "Amsterdam", ixp: "RIPE NCC multihop", lat: 52.37, lon: 4.90 },
    Collector { host: "rrc01", city: "London", ixp: "LINX / LONAP", lat: 51.51, lon: -0.13 },
    Collector { host: "rrc03", city: "Amsterdam", ixp: "AMS-IX / NL-IX", lat: 52.37, lon: 4.90 },
    Collector { host: "rrc04", city: "Geneva", ixp: "CIXP", lat: 46.20, lon: 6.14 },
    Collector { host: "rrc05", city: "Vienna", ixp: "VIX", lat: 48.21, lon: 16.37 },
    Collector { host: "rrc06", city: "Tokyo", ixp: "DIX-IE / JPIX", lat: 35.68, lon: 139.77 },
    Collector { host: "rrc07", city: "Stockholm", ixp: "Netnod", lat: 59.33, lon: 18.07 },
    Collector { host: "rrc10", city: "Milan", ixp: "MIX", lat: 45.46, lon: 9.19 },
    Collector { host: "rrc11", city: "New York", ixp: "NYIIX", lat: 40.71, lon: -74.01 },
    Collector { host: "rrc12", city: "Frankfurt", ixp: "DE-CIX", lat: 50.11, lon: 8.68 },
    Collector { host: "rrc13", city: "Moscow", ixp: "MSK-IX", lat: 55.76, lon: 37.62 },
    Collector { host: "rrc14", city: "Palo Alto", ixp: "PAIX", lat: 37.44, lon: -122.14 },
    Collector { host: "rrc15", city: "São Paulo", ixp: "PTTMetro", lat: -23.55, lon: -46.63 },
    Collector { host: "rrc16", city: "Miami", ixp: "NOTA", lat: 25.77, lon: -80.19 },
    Collector { host: "rrc18", city: "Barcelona", ixp: "CATNIX", lat: 41.39, lon: 2.17 },
    Collector { host: "rrc19", city: "Johannesburg", ixp: "NAP Africa JB", lat: -26.20, lon: 28.05 },
    Collector { host: "rrc20", city: "Zurich", ixp: "SwissIX", lat: 47.38, lon: 8.54 },
    Collector { host: "rrc21", city: "Paris", ixp: "France-IX", lat: 48.86, lon: 2.35 },
    Collector { host: "rrc22", city: "Bucharest", ixp: "InterLAN", lat: 44.43, lon: 26.10 },
    Collector { host: "rrc23", city: "Singapore", ixp: "Equinix SG", lat: 1.29, lon: 103.85 },
    Collector { host: "rrc24", city: "Montevideo", ixp: "LACNIC multihop", lat: -34.90, lon: -56.16 },
    Collector { host: "rrc25", city: "Amsterdam", ixp: "RIPE NCC multihop", lat: 52.37, lon: 4.90 },
    Collector { host: "rrc26", city: "Dubai", ixp: "UAE-IX", lat: 25.20, lon: 55.27 },
];

fn collector(host: &str) -> Option<&'static Collector> {
    COLLECTORS.iter().find(|c| c.host == host)
}

/// What one collector has heard since the last drain.
#[derive(Debug, Default, Clone)]
pub struct Counters {
    pub updates: u64,
    pub announced_prefixes: u64,
    pub withdrawn_prefixes: u64,
    pub peers: HashSet<String>,
}

/// The shared tally the reader fills and the poll drains.
#[derive(Debug)]
pub struct Tally {
    pub since: Instant,
    pub last_message: Option<Instant>,
    pub by_host: HashMap<String, Counters>,
    pub unknown_hosts: HashSet<String>,
    pub connects: u64,
}

impl Tally {
    fn new() -> Self {
        Self { since: Instant::now(), last_message: None, by_host: HashMap::new(), unknown_hosts: HashSet::new(), connects: 0 }
    }
}

pub struct RisLive {
    descriptor: SourceDescriptor,
    tally: Arc<Mutex<Tally>>,
    started: AtomicBool,
}

impl RisLive {
    pub fn new() -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("ris-live"),
                layer_id: LayerId::new("bgp-churn"),
                display_name: "BGP churn at RIS collectors (RIPE RIS Live)".into(),
                kind: EntityKind::Measure,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "RIPE NCC Routing Information Service".into(),
                    url: "https://ris-live.ripe.net/".into(),
                    license: "RIPE NCC RIS data, free to use with attribution".into(),
                    notice: Some("BGP update rates from RIPE NCC's RIS Live".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            tally: Arc::new(Mutex::new(Tally::new())),
            started: AtomicBool::new(false),
        }
    }
}

impl Default for RisLive {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Source for RisLive {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        if !self.started.swap(true, Ordering::SeqCst) {
            let tally = self.tally.clone();
            let id = self.descriptor.id.clone();
            tokio::spawn(async move { reader(tally, id).await });
            // Nothing heard yet; the first minute's rates come next poll.
            return Ok(Vec::new());
        }
        let now = Utc::now();
        let drained = {
            let mut t = self.tally.lock().expect("ris tally lock");
            let window = t.since.elapsed();
            let silent_for = t.last_message.map_or(window, |m| m.elapsed());
            if silent_for > SILENCE {
                return Err(SourceError::Transport(format!("no BGP message from RIS Live for {} s", silent_for.as_secs())));
            }
            let by_host = std::mem::take(&mut t.by_host);
            let unknown = std::mem::take(&mut t.unknown_hosts);
            t.since = Instant::now();
            (window, by_host, unknown)
        };
        let (window, by_host, unknown) = drained;
        if !unknown.is_empty() {
            let mut names: Vec<_> = unknown.into_iter().collect();
            names.sort();
            tracing::warn!(source = %self.descriptor.id, "collectors not in the position table: {}", names.join(", "));
        }
        let observations = observations(&by_host, window, now, &self.descriptor.id);
        tracing::debug!(source = %self.descriptor.id, collectors = observations.len(), window_s = window.as_secs(), "churn drained");
        Ok(observations)
    }
}

/// One measure per collector heard in the window.
pub fn observations(by_host: &HashMap<String, Counters>, window: Duration, now: chrono::DateTime<Utc>, source_id: &SourceId) -> Vec<Observation> {
    let minutes = (window.as_secs_f64() / 60.0).max(1.0 / 60.0);
    let mut hosts: Vec<_> = by_host.iter().collect();
    hosts.sort_by(|a, b| a.0.cmp(b.0));
    hosts
        .into_iter()
        .filter_map(|(host, c)| {
            let place = collector(host)?;
            let attrs = serde_json::json!({
                "collector": host.to_uppercase(),
                "city": place.city,
                "ixp": place.ixp,
                "window_s": window.as_secs(),
                "updates": c.updates,
                "updates_per_min": (c.updates as f64 / minutes).round(),
                "announced_prefixes": c.announced_prefixes,
                "announced_per_min": (c.announced_prefixes as f64 / minutes).round(),
                "withdrawn_prefixes": c.withdrawn_prefixes,
                "withdrawn_per_min": (c.withdrawn_prefixes as f64 / minutes).round(),
                "peers_heard": c.peers.len(),
                "url": format!("https://ris-live.ripe.net/?host={host}.ripe.net"),
            });
            Some(
                Observation::new(source_id.clone(), EntityId::new(EntityKind::Measure, format!("ris:{host}")), now, Quality::Live)
                    .with_position(Position { lon: place.lon, lat: place.lat, alt_m: None, datum: AltitudeDatum::Geoid })
                    .with_label(format!("{} — {}", host.to_uppercase(), place.city))
                    .with_attrs(attrs),
            )
        })
        .collect()
}

// --- the socket ------------------------------------------------------------------

/// The parts of a RIS Live frame the tally needs; everything else — the
/// path, communities, next hops — is skipped by the deserialiser.
#[derive(Debug, Deserialize)]
struct Frame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    data: Option<Payload>,
}

#[derive(Debug, Deserialize)]
struct Payload {
    #[serde(default)]
    host: String,
    #[serde(default)]
    peer_asn: String,
    #[serde(default)]
    announcements: Vec<Announcement>,
    #[serde(default)]
    withdrawals: Vec<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Announcement {
    #[serde(default)]
    prefixes: Vec<String>,
}

/// Fold one frame into the tally. Returns false for a frame that is not
/// a BGP message, so the caller can count those separately.
pub fn absorb(tally: &mut Tally, text: &str) -> bool {
    let Ok(frame) = serde_json::from_str::<Frame>(text) else { return false };
    match frame.kind.as_str() {
        "ris_message" => {
            let Some(d) = frame.data else { return false };
            let host = d.host.trim_end_matches(".ripe.net").to_ascii_lowercase();
            tally.last_message = Some(Instant::now());
            if collector(&host).is_none() {
                tally.unknown_hosts.insert(host);
                return true;
            }
            let c = tally.by_host.entry(host).or_default();
            c.updates += 1;
            c.announced_prefixes += d.announcements.iter().map(|a| a.prefixes.len() as u64).sum::<u64>();
            c.withdrawn_prefixes += d.withdrawals.len() as u64;
            if !d.peer_asn.is_empty() {
                c.peers.insert(d.peer_asn);
            }
            true
        }
        "ris_error" => {
            tracing::warn!("RIS Live error frame: {}", frame.data.and_then(|d| d.message).unwrap_or_default());
            false
        }
        _ => false,
    }
}

async fn reader(tally: Arc<Mutex<Tally>>, source_id: SourceId) {
    let mut backoff = RECONNECT_MIN;
    loop {
        match tokio_tungstenite::connect_async(URL).await {
            Ok((mut socket, _)) => {
                tally.lock().expect("ris tally lock").connects += 1;
                let subscribe = serde_json::json!({"type": "ris_subscribe", "data": {"type": "UPDATE"}}).to_string();
                if let Err(err) = socket.send(tokio_tungstenite::tungstenite::Message::Text(subscribe)).await {
                    tracing::warn!(source = %source_id, %err, "RIS Live subscribe failed");
                } else {
                    tracing::info!(source = %source_id, "RIS Live connected");
                    backoff = RECONNECT_MIN;
                    while let Some(frame) = socket.next().await {
                        match frame {
                            Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                                absorb(&mut tally.lock().expect("ris tally lock"), &text);
                            }
                            Ok(tokio_tungstenite::tungstenite::Message::Ping(payload)) => {
                                let _ = socket.send(tokio_tungstenite::tungstenite::Message::Pong(payload)).await;
                            }
                            Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(err) => {
                                tracing::warn!(source = %source_id, %err, "RIS Live socket error");
                                break;
                            }
                        }
                    }
                    tracing::warn!(source = %source_id, "RIS Live socket closed; reconnecting in {} s", backoff.as_secs());
                }
            }
            Err(err) => tracing::warn!(source = %source_id, %err, "RIS Live connect failed; retrying in {} s", backoff.as_secs()),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGE: &str = r#"{"type":"ris_message","data":{"timestamp":1789647591.23,"peer":"196.60.9.84","peer_asn":"37697","id":"196.60.9.84-01a0af4f173e0006","host":"rrc19.ripe.net","type":"UPDATE","path":[37697,328810],"community":[[37697,210]],"origin":"IGP","announcements":[{"next_hop":"196.60.10.122","prefixes":["102.221.15.0/24","102.221.16.0/24"]}],"withdrawals":["41.0.0.0/24"]}}"#;

    #[test]
    fn frames_are_tallied_per_collector_and_a_collector_nobody_placed_is_noted() {
        let mut t = Tally::new();
        assert!(absorb(&mut t, MESSAGE));
        assert!(absorb(&mut t, MESSAGE));
        assert!(absorb(&mut t, &MESSAGE.replace("rrc19.ripe.net", "rrc99.ripe.net").replace("\"37697\"", "\"1\"")));
        assert!(!absorb(&mut t, r#"{"type":"ris_error","data":{"message":"bad subscription"}}"#));
        assert!(!absorb(&mut t, "not json"));
        let c = &t.by_host["rrc19"];
        assert_eq!(c.updates, 2);
        assert_eq!(c.announced_prefixes, 4);
        assert_eq!(c.withdrawn_prefixes, 2);
        assert_eq!(c.peers.len(), 1);
        assert!(t.unknown_hosts.contains("rrc99"));
        assert!(t.last_message.is_some());
    }

    #[test]
    fn a_minute_of_counts_becomes_one_measure_per_collector_with_rates() {
        let mut by_host = HashMap::new();
        let mut c = Counters { updates: 240, announced_prefixes: 600, withdrawn_prefixes: 30, ..Default::default() };
        c.peers.insert("1".into());
        c.peers.insert("2".into());
        by_host.insert("rrc01".to_string(), c);
        by_host.insert("rrc99".to_string(), Counters::default());
        let obs = observations(&by_host, Duration::from_secs(120), Utc::now(), &SourceId::new("ris-live"));
        assert_eq!(obs.len(), 1, "the unknown collector is not drawn");
        let o = &obs[0];
        assert_eq!(o.entity.key, "ris:rrc01");
        assert_eq!(o.entity.kind, EntityKind::Measure);
        assert_eq!(o.label.as_deref(), Some("RRC01 — London"));
        assert_eq!(o.attrs["updates_per_min"], 120.0);
        assert_eq!(o.attrs["announced_per_min"], 300.0);
        assert_eq!(o.attrs["withdrawn_per_min"], 15.0);
        assert_eq!(o.attrs["peers_heard"], 2);
        assert_eq!(o.attrs["ixp"], "LINX / LONAP");
        assert!((o.position.unwrap().lat - 51.51).abs() < 1e-9);
    }

    #[test]
    fn every_collector_in_the_table_is_distinct_and_on_earth() {
        let mut hosts = HashSet::new();
        for c in COLLECTORS {
            assert!(hosts.insert(c.host), "{} twice", c.host);
            assert!((-90.0..=90.0).contains(&c.lat) && (-180.0..=180.0).contains(&c.lon));
        }
        assert_eq!(COLLECTORS.len(), 23);
    }
}
