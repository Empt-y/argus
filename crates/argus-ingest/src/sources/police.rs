//! Street-level crime in England, Wales and Northern Ireland, from
//! data.police.uk.
//!
//! The Home Office publishes every recorded crime and anti-social
//! behaviour incident as a point with a category, a month, a street and,
//! for most, the latest outcome. The API answers for a polygon at a time
//! and refuses with a 503 when the answer would be over ten thousand
//! records — which a square five kilometres on a side in central London
//! is, every month. So an area is asked for as a lattice of half-degree
//! tiles, and any tile the API refuses is cut in four and asked again,
//! down to a floor a few hundred metres across. Rural tiles answer in a
//! second; the ones over cities take ten and split; the sea and Scotland
//! (which is not in this dataset) answer with nothing at once.
//!
//! ## Two things a reader has to be told
//!
//! The date is a month. The API carries no day, and stamping a crime at
//! the first of its month would put it on the DVR at midnight on a day it
//! did not happen. Poll time is the observation time instead, as for the
//! other layers whose upstream has no useful clock, the month is carried
//! as `month`, and the card says "in July 2026" and nothing more precise.
//! The quality is [`Quality::Delayed`]: the newest month available is
//! typically two months old, and that lag is stated, not hidden.
//!
//! The location is not where it happened. Every point is snapped to one
//! of a set of anonymised locations — the middle of a street, a named
//! place — chosen so that no point identifies a house. `location_type`
//! and `street` are carried, the card says the point is snapped, and
//! nothing in this layer should be read as an address.
//!
//! ## Cost
//!
//! A poll of the British Isles area is a few hundred requests spread over
//! the better part of an hour at the shared one-a-second pace. Nothing
//! changes for a month at a time, so the cadence is three days: inside
//! the seven-day event horizon with a failed poll's room to spare, and
//! the month is checked first so a poll that would rewrite the same month
//! it wrote three days ago still rewrites it, because that is what keeps
//! the points on the map. Tiles are remembered across the areas of one
//! poll, so the home area inside the British Isles area is not crawled
//! twice.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use argus_core::BoundingBox;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;

const DATES_URL: &str = "https://data.police.uk/api/crimes-street-dates";
const CRIMES_URL: &str = "https://data.police.uk/api/crimes-street/all-crime";

/// Three days. The data is monthly; the event horizon is a week.
const CADENCE_SECS: u64 = 3 * 24 * 3600;

/// The starting tile. Half a degree is fifty kilometres by thirty at this
/// latitude: under ten thousand records nearly everywhere outside a city.
const TILE_DEG: f64 = 0.5;

/// A tile is not split below this. 0.02° is about 1.4 km by 2.2 km; a
/// tile that size still over the limit is a data error, not a dense one.
const MIN_TILE_DEG: f64 = 0.02;

/// A stop on runaway splitting: the British Isles area at the starting
/// tile is about 600 tiles, and the cities add a few hundred more.
const MAX_REQUESTS_PER_POLL: usize = 2_000;

pub struct StreetCrime {
    descriptor: SourceDescriptor,
    http: HttpClient,
    /// Tiles already read this poll cycle, by their bbox text, with the
    /// month they were read for. Cleared when the month moves on.
    done: Mutex<Done>,
}

#[derive(Default)]
struct Done {
    month: String,
    tiles: HashMap<String, DateTime<Utc>>,
}

impl StreetCrime {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("police-uk"),
                layer_id: LayerId::new("street-crime"),
                display_name: "Street-level crime (data.police.uk)".into(),
                kind: EntityKind::Event,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Bounded,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "Home Office / data.police.uk".into(),
                    url: "https://data.police.uk/".into(),
                    license: "Open Government Licence v3.0".into(),
                    notice: Some("Contains public sector information licensed under the Open Government Licence v3.0".into()),
                },
                base_quality: Quality::Delayed,
                quota: None,
            },
            http,
            done: Mutex::new(Done::default()),
        }
    }

    /// The newest month the API has data for.
    async fn latest_month(&self) -> Result<String, SourceError> {
        let dates: Vec<DateEntry> = self.http.get_json(DATES_URL).await?;
        dates
            .into_iter()
            .filter_map(|d| d.date)
            .max()
            .ok_or_else(|| SourceError::Decode("crimes-street-dates listed no months".into()))
    }

    /// Whether this tile was already read for this month, within the
    /// cadence; and mark it read if not.
    fn claim(&self, month: &str, tile: &BoundingBox, now: DateTime<Utc>) -> bool {
        let mut done = self.done.lock().expect("tile lock poisoned");
        if done.month != month {
            done.month = month.to_string();
            done.tiles.clear();
        }
        let key = tile_key(tile);
        let fresh = chrono::Duration::seconds(CADENCE_SECS as i64 - 3600);
        if done.tiles.get(&key).is_some_and(|t| now - *t < fresh) {
            return false;
        }
        done.tiles.insert(key, now);
        true
    }

    /// One tile's crimes, splitting on the API's "too many" refusal.
    async fn crawl(
        &self,
        tile: BoundingBox,
        month: &str,
        now: DateTime<Utc>,
        requests: &mut usize,
        out: &mut Vec<Observation>,
        stats: &mut Stats,
    ) {
        if *requests >= MAX_REQUESTS_PER_POLL {
            stats.abandoned += 1;
            return;
        }
        if !self.claim(month, &tile, now) {
            stats.skipped += 1;
            return;
        }
        *requests += 1;
        let url = format!("{CRIMES_URL}?poly={}&date={month}", poly(&tile));
        match self.http.get_bytes(&url).await {
            Ok(bytes) => match decode(&bytes, &self.descriptor.id, now) {
                Ok(decoded) => {
                    stats.records += decoded.records;
                    stats.unplaced += decoded.unplaced;
                    out.extend(decoded.observations);
                }
                Err(err) => {
                    stats.failed += 1;
                    self.release(&tile);
                    tracing::warn!(source = %self.descriptor.id, %err, tile = %tile_key(&tile), "tile did not decode");
                }
            },
            Err(err) if is_too_many(&err) => {
                let width = tile.east - tile.west;
                if width <= MIN_TILE_DEG {
                    stats.failed += 1;
                    tracing::warn!(source = %self.descriptor.id, tile = %tile_key(&tile), "over ten thousand records in a tile at the floor; skipped");
                    return;
                }
                stats.split += 1;
                for quarter in quarters(&tile) {
                    Box::pin(self.crawl(quarter, month, now, requests, out, stats)).await;
                }
            }
            Err(err) => {
                stats.failed += 1;
                self.release(&tile);
                tracing::warn!(source = %self.descriptor.id, %err, tile = %tile_key(&tile), "tile failed");
            }
        }
    }

    /// Forget a claim, so a tile that failed is asked again on the next
    /// attempt rather than after the next cadence.
    fn release(&self, tile: &BoundingBox) {
        let mut done = self.done.lock().expect("tile lock poisoned");
        done.tiles.remove(&tile_key(tile));
    }
}

#[derive(Default, Debug)]
struct Stats {
    records: usize,
    unplaced: usize,
    split: usize,
    skipped: usize,
    failed: usize,
    abandoned: usize,
}

#[async_trait::async_trait]
impl Source for StreetCrime {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let Some(bbox) = ctx.bbox else {
            return Err(SourceError::Decode("street crime is a bounded source and was polled without an area".into()));
        };
        let now = Utc::now();
        let month = self.latest_month().await?;
        let mut out = Vec::new();
        let mut stats = Stats::default();
        let mut requests = 0;
        for tile in tiles(&bbox, TILE_DEG) {
            self.crawl(tile, &month, now, &mut requests, &mut out, &mut stats).await;
        }
        tracing::info!(
            source = %self.descriptor.id,
            %month,
            requests,
            written = out.len(),
            ?stats,
            "street crime read"
        );
        if out.is_empty() && stats.failed > 0 && stats.records == 0 {
            return Err(SourceError::Transport(format!("{} tiles failed and none answered", stats.failed)));
        }
        Ok(out)
    }
}

/// The API's refusal for an answer over ten thousand records is a bare
/// 503, which the client reports as a transport failure with the status
/// in the text.
fn is_too_many(err: &SourceError) -> bool {
    matches!(err, SourceError::Transport(msg) if msg.contains("503"))
}

fn tile_key(b: &BoundingBox) -> String {
    format!("{:.3},{:.3},{:.3},{:.3}", b.west, b.south, b.east, b.north)
}

/// `lat,lng:lat,lng:…`, the API's polygon syntax, for a box.
fn poly(b: &BoundingBox) -> String {
    format!(
        "{:.4},{:.4}:{:.4},{:.4}:{:.4},{:.4}:{:.4},{:.4}",
        b.south, b.west, b.south, b.east, b.north, b.east, b.north, b.west
    )
}

/// Cut an area into tiles of `size` degrees, aligned to the size so the
/// same tile is the same tile whichever area asked for it.
pub fn tiles(bbox: &BoundingBox, size: f64) -> Vec<BoundingBox> {
    // An area smaller than a tile is asked for as itself: aligning it
    // would crawl up to four tiles for a box a city block across.
    if bbox.east - bbox.west <= size && bbox.north - bbox.south <= size {
        return vec![*bbox];
    }
    let mut out = Vec::new();
    let mut south = (bbox.south / size).floor() * size;
    while south < bbox.north {
        let mut west = (bbox.west / size).floor() * size;
        while west < bbox.east {
            out.push(BoundingBox::new(west, south, west + size, south + size));
            west += size;
        }
        south += size;
    }
    out
}

fn quarters(b: &BoundingBox) -> [BoundingBox; 4] {
    let mx = (b.west + b.east) / 2.0;
    let my = (b.south + b.north) / 2.0;
    [
        BoundingBox::new(b.west, b.south, mx, my),
        BoundingBox::new(mx, b.south, b.east, my),
        BoundingBox::new(b.west, my, mx, b.north),
        BoundingBox::new(mx, my, b.east, b.north),
    ]
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DateEntry {
    date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Crime {
    id: Option<i64>,
    category: Option<String>,
    location_type: Option<String>,
    location_subtype: Option<String>,
    location: Option<Location>,
    context: Option<String>,
    outcome_status: Option<Outcome>,
    persistent_id: Option<String>,
    month: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Location {
    latitude: Option<String>,
    longitude: Option<String>,
    street: Option<Street>,
}

#[derive(Debug, Deserialize)]
struct Street {
    id: Option<i64>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Outcome {
    category: Option<String>,
    date: Option<String>,
}

#[derive(Debug)]
pub struct Decoded {
    pub observations: Vec<Observation>,
    pub records: usize,
    pub unplaced: usize,
}

/// The API's category slugs, in the words the police use for them.
pub fn category_words(slug: &str) -> &str {
    match slug {
        "anti-social-behaviour" => "Anti-social behaviour",
        "bicycle-theft" => "Bicycle theft",
        "burglary" => "Burglary",
        "criminal-damage-arson" => "Criminal damage and arson",
        "drugs" => "Drugs",
        "other-theft" => "Other theft",
        "possession-of-weapons" => "Possession of weapons",
        "public-order" => "Public order",
        "robbery" => "Robbery",
        "shoplifting" => "Shoplifting",
        "theft-from-the-person" => "Theft from the person",
        "vehicle-crime" => "Vehicle crime",
        "violent-crime" => "Violence and sexual offences",
        "other-crime" => "Other crime",
        other => other,
    }
}

pub fn decode(bytes: &[u8], source_id: &SourceId, now: DateTime<Utc>) -> Result<Decoded, SourceError> {
    let crimes: Vec<Crime> = serde_json::from_slice(bytes).map_err(|e| SourceError::Decode(e.to_string()))?;
    let mut observations = Vec::with_capacity(crimes.len());
    let mut unplaced = 0;
    let records = crimes.len();
    for c in crimes {
        let Some(id) = c.id else { continue };
        let Some(loc) = c.location.as_ref() else {
            unplaced += 1;
            continue;
        };
        let (Some(lat), Some(lon)) = (
            loc.latitude.as_deref().and_then(|s| s.parse::<f64>().ok()),
            loc.longitude.as_deref().and_then(|s| s.parse::<f64>().ok()),
        ) else {
            unplaced += 1;
            continue;
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) || (lat == 0.0 && lon == 0.0) {
            unplaced += 1;
            continue;
        }
        let category = c.category.as_deref().unwrap_or("other-crime");
        let words = category_words(category);
        let street = loc.street.as_ref().and_then(|s| s.name.as_deref()).map(str::trim).filter(|s| !s.is_empty());

        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        put("category", serde_json::json!(category));
        put("month", serde_json::json!(c.month));
        put("street", serde_json::json!(street));
        put("street_id", serde_json::json!(loc.street.as_ref().and_then(|s| s.id)));
        put("location_type", serde_json::json!(c.location_type.as_deref().filter(|s| !s.is_empty())));
        put("location_subtype", serde_json::json!(c.location_subtype.as_deref().filter(|s| !s.is_empty())));
        put("context", serde_json::json!(c.context.as_deref().map(str::trim).filter(|s| !s.is_empty())));
        put("outcome", serde_json::json!(c.outcome_status.as_ref().and_then(|o| o.category.as_deref())));
        put("outcome_month", serde_json::json!(c.outcome_status.as_ref().and_then(|o| o.date.as_deref())));
        put("persistent_id", serde_json::json!(c.persistent_id.as_deref().filter(|s| !s.is_empty())));
        put("snapped", serde_json::json!(true));

        let label = match street {
            Some(s) => format!("{words}, {}", s.strip_prefix("On or near ").unwrap_or(s)),
            None => words.to_string(),
        };
        observations.push(
            Observation::new(
                source_id.clone(),
                EntityId::new(EntityKind::Event, format!("police-{id}")),
                now,
                Quality::Delayed,
            )
            .with_position(Position {
                lon,
                lat,
                alt_m: None,
                datum: AltitudeDatum::AboveGround,
            })
            .with_label(label)
            .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    Ok(Decoded {
        observations,
        records,
        unplaced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &[u8] = br#"[{"category":"anti-social-behaviour","location_type":"Force","location":{"latitude":"51.528630","street":{"id":1682385,"name":"On or near Earlstoke Street"},"longitude":"-0.103138"},"context":"","outcome_status":null,"persistent_id":"","id":136368915,"location_subtype":"","month":"2026-07"},
    {"category":"bicycle-theft","location_type":"BTP","location":{"latitude":"51.530000","street":{"id":1682390,"name":"On or near Kings Cross"},"longitude":"-0.123000"},"context":"","outcome_status":{"category":"Investigation complete; no suspect identified","date":"2026-08"},"persistent_id":"abc123","id":136368916,"location_subtype":"Station","month":"2026-07"},
    {"category":"drugs","location_type":"Force","location":null,"context":"","outcome_status":null,"persistent_id":"","id":136368917,"location_subtype":"","month":"2026-07"}]"#;

    #[test]
    fn a_crime_is_an_event_stamped_at_poll_time_with_its_month_carried() {
        let now = Utc::now();
        let d = decode(PAGE, &SourceId::new("police-uk"), now).unwrap();
        assert_eq!(d.records, 3);
        assert_eq!(d.unplaced, 1);
        assert_eq!(d.observations.len(), 2);
        let asb = &d.observations[0];
        assert_eq!(asb.entity.key, "police-136368915");
        assert_eq!(asb.entity.kind, EntityKind::Event);
        assert_eq!(asb.observed_at, now);
        assert_eq!(asb.quality, Quality::Delayed);
        assert_eq!(asb.attrs["month"], "2026-07");
        assert_eq!(asb.attrs["street"], "On or near Earlstoke Street");
        assert!(asb.attrs.get("persistent_id").is_none(), "an empty persistent id is absent, not empty");
        assert!(asb.attrs.get("outcome").is_none());
        assert_eq!(asb.label.as_deref(), Some("Anti-social behaviour, Earlstoke Street"));
        let bike = &d.observations[1];
        assert_eq!(bike.attrs["outcome"], "Investigation complete; no suspect identified");
        assert_eq!(bike.attrs["location_type"], "BTP");
        assert_eq!(bike.attrs["location_subtype"], "Station");
        assert_eq!(bike.attrs["persistent_id"], "abc123");
    }

    #[test]
    fn tiles_are_aligned_so_two_areas_share_them() {
        let home = tiles(&BoundingBox::new(-2.5, 51.0, 0.5, 52.5), 0.5);
        assert_eq!(home.len(), 6 * 3);
        let isles = tiles(&BoundingBox::new(-11.0, 49.5, 2.0, 61.0), 0.5);
        assert_eq!(isles.len(), 26 * 23);
        for t in &home {
            assert!(isles.iter().any(|i| tile_key(i) == tile_key(t)), "{} is not a British Isles tile", tile_key(t));
        }
        // An area that does not start on a tile edge still gets whole
        // tiles: three columns and two rows here, all six aligned.
        let off = tiles(&BoundingBox::new(-0.3, 51.3, 0.6, 51.6), 0.5);
        assert_eq!(off.len(), 6);
        assert_eq!(tile_key(&off[0]), "-0.500,51.000,0.000,51.500");
        // But an area smaller than a tile is itself the tile.
        let small = tiles(&BoundingBox::new(-0.15, 51.48, -0.05, 51.55), 0.5);
        assert_eq!(small.len(), 1);
        assert_eq!(tile_key(&small[0]), "-0.150,51.480,-0.050,51.550");
    }

    #[test]
    fn the_polygon_is_the_api_syntax_and_a_quarter_is_a_quarter() {
        let t = BoundingBox::new(-0.5, 51.0, 0.0, 51.5);
        assert_eq!(poly(&t), "51.0000,-0.5000:51.0000,0.0000:51.5000,0.0000:51.5000,-0.5000");
        let q = quarters(&t);
        assert_eq!(tile_key(&q[0]), "-0.500,51.000,-0.250,51.250");
        assert_eq!(tile_key(&q[3]), "-0.250,51.250,0.000,51.500");
        assert!(is_too_many(&SourceError::Transport("upstream returned 503 Service Unavailable".into())));
        assert!(!is_too_many(&SourceError::Transport("upstream returned 500 Internal Server Error".into())));
    }

    #[test]
    fn a_tile_is_claimed_once_per_month_per_cycle() {
        let http = HttpClient::new(std::time::Duration::from_secs(5)).unwrap();
        let s = StreetCrime::new(http);
        let t = BoundingBox::new(-0.5, 51.0, 0.0, 51.5);
        let now = Utc::now();
        assert!(s.claim("2026-07", &t, now));
        assert!(!s.claim("2026-07", &t, now), "the home area inside the isles is not crawled twice");
        assert!(s.claim("2026-08", &t, now), "a new month is a new crawl");
        assert!(s.claim("2026-08", &t, now + chrono::Duration::seconds(CADENCE_SECS as i64)), "and so is the next cycle");
    }
}
