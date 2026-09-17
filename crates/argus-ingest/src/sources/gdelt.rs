//! The world's news as events, from GDELT.
//!
//! GDELT reads news in a hundred languages and every fifteen minutes
//! publishes what it found as coded events: who did what to whom, where,
//! in CAMEO's vocabulary — a visit hosted, a demand made, a demonstration,
//! an airstrike — with a Goldstein score for how cooperative or hostile
//! the act is, how many articles and sources reported it, the tone they
//! took, and the article the event was read from. Around 1,340 events a
//! file, 130,000 a day. Ash chose to keep all of it rather than only the
//! conflict classes, with actor names as GDELT gives them; the card says
//! what happened and links the article.
//!
//! Placement is GDELT's own: the action's geography at whatever precision
//! the text allowed — a city or landmark for two thirds of events, a
//! country's centroid for the rest — and the card states which. Events
//! with no place at all (about 3%) are counted and skipped.
//!
//! Each file is a ZIP holding one tab-separated CSV with no header and 61
//! columns; the archive is read by [`crate::zip`], the columns by
//! position. `lastupdate.txt` names the newest file, and both it and the
//! master list name files that do not exist yet — up to an hour of them
//! were missing when this was written — so the driver walks forward from
//! the last file it has, one fifteen-minute slot at a time, and stops at
//! the first 404 to try again next poll — and the other way too, a file
//! named for a quarter-hour not yet reached has turned up, so an event's
//! date is its file's stamp or the time it was read, whichever is
//! earlier. A fresh start begins an hour back; a long outage resumes from
//! an hour back too, not from where it left off — the master list is 128
//! MB and yesterday's news is the DVR's problem, not the poll's. The `Day` column is a year off for a
//! percent of events (GDELT's own bug); the file's stamp is the date.

use crate::http::HttpClient;
use crate::sources::cameo;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use std::sync::Mutex;

const BASE: &str = "https://data.gdeltproject.org/gdeltv2";
const CADENCE_SECS: u64 = 15 * 60;
/// How far back a fresh start looks.
const BACKFILL_MINUTES: i64 = 60;
/// Slots walked in one poll; an hour's catch-up plus the new one.
const MAX_SLOTS_PER_POLL: usize = 8;
const SLOT_MINUTES: i64 = 15;
const COLUMNS: usize = 61;

pub struct Gdelt {
    descriptor: SourceDescriptor,
    http: HttpClient,
    /// The stamp of the newest file read, `None` before the first poll.
    last_stamp: Mutex<Option<DateTime<Utc>>>,
}

impl Gdelt {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("gdelt"),
                layer_id: LayerId::new("news-events"),
                display_name: "News events (GDELT)".into(),
                kind: EntityKind::Event,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "The GDELT Project".into(),
                    url: "https://www.gdeltproject.org/".into(),
                    license: "GDELT data, free for use with attribution".into(),
                    notice: Some("Event data from the GDELT Project".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
            last_stamp: Mutex::new(None),
        }
    }
}

/// A stamp rounded down to its fifteen-minute slot.
fn slot(t: DateTime<Utc>) -> DateTime<Utc> {
    let secs = t.timestamp();
    DateTime::from_timestamp(secs - secs.rem_euclid(SLOT_MINUTES * 60), 0).unwrap_or(t)
}

fn stamp_text(t: DateTime<Utc>) -> String {
    t.format("%Y%m%d%H%M%S").to_string()
}

/// The newest export stamp `lastupdate.txt` names.
pub fn decode_lastupdate(text: &str) -> Result<DateTime<Utc>, SourceError> {
    let line = text.lines().find(|l| l.contains(".export.CSV.zip")).ok_or_else(|| SourceError::Decode("lastupdate.txt names no export file".into()))?;
    let name = line.rsplit('/').next().unwrap_or(line);
    let stamp = name.get(..14).ok_or_else(|| SourceError::Decode(format!("odd export name: {name}")))?;
    parse_stamp(stamp).ok_or_else(|| SourceError::Decode(format!("odd export stamp: {stamp}")))
}

fn parse_stamp(s: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s, "%Y%m%d%H%M%S").ok().map(|t| t.and_utc())
}

#[async_trait::async_trait]
impl Source for Gdelt {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let text = self.http.get_bytes(&format!("{BASE}/lastupdate.txt")).await?;
        let newest = decode_lastupdate(&String::from_utf8_lossy(&text))?;
        let mut cursor = {
            let last = *self.last_stamp.lock().expect("gdelt stamp lock");
            last.unwrap_or_else(|| slot(Utc::now() - Duration::minutes(BACKFILL_MINUTES)) - Duration::minutes(SLOT_MINUTES))
        };
        let mut observations = Vec::new();
        let (mut files, mut unplaced, mut malformed) = (0, 0, 0);
        let mut missing: Option<DateTime<Utc>> = None;
        while cursor < newest && files < MAX_SLOTS_PER_POLL {
            let next = cursor + Duration::minutes(SLOT_MINUTES);
            let url = format!("{BASE}/{}.export.CSV.zip", stamp_text(next));
            let Some(bytes) = self.http.get_bytes_if_present(&url).await? else {
                // Listed, not there yet. Stop; the next poll asks again.
                missing = Some(next);
                break;
            };
            let (_, csv) = crate::zip::first_entry(&bytes).map_err(|e| SourceError::Decode(format!("{}: {e}", stamp_text(next))))?;
            // GDELT's stamps are its own: a file named 16:45 was fetched
            // at 16:39, and an event cannot have been read before it was.
            let decoded = decode_file(&csv, next.min(Utc::now()), &self.descriptor.id);
            unplaced += decoded.unplaced;
            malformed += decoded.malformed;
            observations.extend(decoded.observations);
            files += 1;
            cursor = next;
            *self.last_stamp.lock().expect("gdelt stamp lock") = Some(cursor);
        }
        if files == 0 {
            // Nothing new is not a failure; a file that never appears is
            // visible as a growing gap between the cursor and `newest`.
            if let Some(m) = missing {
                tracing::info!(source = %self.descriptor.id, listed = %stamp_text(newest), waiting_for = %stamp_text(m), "newest listed file is not published yet");
            }
            return Ok(Vec::new());
        }
        tracing::info!(source = %self.descriptor.id, files, events = observations.len(), unplaced, malformed, behind_by_slots = ((newest - cursor).num_minutes() / SLOT_MINUTES).max(0), "news events read");
        Ok(observations)
    }
}

// --- wire format --------------------------------------------------------------

#[derive(Debug)]
pub struct Decoded {
    pub observations: Vec<Observation>,
    pub unplaced: usize,
    pub malformed: usize,
}

fn precision_words(t: &str) -> Option<&'static str> {
    match t {
        "1" => Some("country"),
        "2" => Some("US state"),
        "3" => Some("state or province"),
        "4" => Some("city"),
        "5" => Some("landmark"),
        _ => None,
    }
}

/// An actor's columns as one object; empty fields left out, and no object
/// at all for an empty actor.
fn actor(cols: &[&str], first: usize) -> serde_json::Value {
    let get = |i: usize| cols.get(first + i).map(|s| s.trim()).filter(|s| !s.is_empty());
    let mut v = serde_json::Map::new();
    for (i, key) in [(0, "code"), (1, "name"), (2, "country"), (3, "known_group"), (4, "ethnic"), (5, "religion"), (6, "religion2"), (7, "type"), (8, "type2"), (9, "type3")] {
        if let Some(s) = get(i) {
            v.insert(key.into(), serde_json::json!(s));
        }
    }
    if v.is_empty() { serde_json::Value::Null } else { serde_json::Value::Object(v) }
}

pub fn decode_file(csv: &[u8], file_stamp: DateTime<Utc>, source_id: &SourceId) -> Decoded {
    let text = String::from_utf8_lossy(csv);
    let mut observations = Vec::with_capacity(1500);
    let (mut unplaced, mut malformed) = (0, 0);
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() != COLUMNS || cols[0].is_empty() {
            malformed += 1;
            continue;
        }
        let (Ok(lat), Ok(lon)) = (cols[56].trim().parse::<f64>(), cols[57].trim().parse::<f64>()) else {
            unplaced += 1;
            continue;
        };
        let precision = precision_words(cols[51].trim());
        if precision.is_none() || !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) || (lat == 0.0 && lon == 0.0) {
            unplaced += 1;
            continue;
        }
        let f = |i: usize| cols[i].trim();
        let num = |i: usize| f(i).parse::<f64>().ok();
        let int = |i: usize| f(i).parse::<i64>().ok();
        let event_code = f(26);
        let root_code = f(28);
        let event_words = cameo::words(event_code).unwrap_or("event");
        let place = f(52);
        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        put("event_code", serde_json::json!(event_code));
        put("event", serde_json::json!(event_words));
        put("root_code", serde_json::json!(root_code));
        put("root", serde_json::json!(cameo::root_words(root_code)));
        put("quad_class", serde_json::json!(int(29)));
        put("quad", serde_json::json!(cameo::quad_words(f(29))));
        put("goldstein", serde_json::json!(num(30)));
        put("mentions", serde_json::json!(int(31)));
        put("sources", serde_json::json!(int(32)));
        put("articles", serde_json::json!(int(33)));
        put("tone", serde_json::json!(num(34).map(|t| (t * 100.0).round() / 100.0)));
        put("root_event", serde_json::json!(if f(25) == "1" { Some(true) } else { None }));
        put("actor1", actor(&cols, 5));
        put("actor2", actor(&cols, 15));
        put("place", serde_json::json!(if place.is_empty() { None } else { Some(place) }));
        put("place_precision", serde_json::json!(precision));
        put("place_country", serde_json::json!(Some(f(53)).filter(|s| !s.is_empty())));
        put("adm1", serde_json::json!(Some(f(54)).filter(|s| !s.is_empty())));
        put("day", serde_json::json!(Some(f(1)).filter(|s| !s.is_empty())));
        put("source_url", serde_json::json!(Some(f(60)).filter(|s| s.starts_with("http"))));
        put("gdelt_id", serde_json::json!(f(0)));
        let label = if place.is_empty() { event_words.to_string() } else { format!("{event_words}: {place}") };
        observations.push(
            Observation::new(source_id.clone(), EntityId::new(EntityKind::Event, format!("gdelt:{}", f(0))), file_stamp, Quality::Live)
                .with_position(Position { lon, lat, alt_m: None, datum: AltitudeDatum::Geoid })
                .with_label(label)
                .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    Decoded { observations, unplaced, malformed }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LASTUPDATE: &str = "99439 58399973c58dd6158223b7debcb50cb0 http://data.gdeltproject.org/gdeltv2/20260917160000.export.CSV.zip\n140005 b9293817a833dad078e08fcdc6022f83 http://data.gdeltproject.org/gdeltv2/20260917160000.mentions.CSV.zip\n6647426 76bdaffab573552d1c5033214aed0600 http://data.gdeltproject.org/gdeltv2/20260917160000.gkg.csv.zip\n";

    // Two real rows (one country-placed, one city-placed), one with no place, one short.
    const CSV: &str = "1323535517\t20250917\t202509\t2025\t2025.7041\tEGY\tEGYPTIAN\tEGY\t\t\t\t\t\t\t\tGBR\tUNITED KINGDOM\tGBR\t\t\t\t\t\t\t\t0\t042\t042\t04\t1\t1.9\t6\t1\t6\t0.31152647975078\t4\tCairo, Al Qahirah, Egypt\tEG\tEG11\t65350\t30.05\t31.25\t-290692\t1\tUnited Kingdom\tUK\tUK\t\t54\t-4\tUK\t1\tUnited Kingdom\tUK\tUK\t\t54\t-4\tUK\t20260917150000\thttp://www.bignewsnetwork.com/news/279313099/switzerland-returns-ancient-golden-mask-to-egypt-photos\n\
1323535600\t20260917\t202609\t2026\t2026.7041\tGEOCOP\tPOLICE\tGEO\t\t\t\t\tCOP\t\t\tGEOCVL\tPROTESTER\tGEO\t\t\t\t\tCVL\t\t\t1\t1452\t145\t14\t3\t-7.5\t12\t1\t12\t-4.1\t4\tTbilisi, T'bilisi, Georgia\tGG\tGG51\t\t41.6941\t44.8337\t-2088927\t4\tTbilisi, T'bilisi, Georgia\tGG\tGG51\t\t41.6941\t44.8337\t-2088927\t4\tTbilisi, T'bilisi, Georgia\tGG\tGG51\t\t41.6941\t44.8337\t-2088927\t20260917150000\thttps://example.org/tbilisi\n\
1323535601\t20260917\t202609\t2026\t2026.7041\t\t\t\t\t\t\t\t\t\t\tUSA\tUNITED STATES\tUSA\t\t\t\t\t\t\t\t0\t010\t010\t01\t1\t0.0\t2\t1\t2\t1.0\t0\t\t\t\t\t\t\t\t0\t\t\t\t\t\t\t\t0\t\t\t\t\t\t\t\t20260917150000\thttp://example.org/nowhere\n\
short\tline\n";

    #[test]
    fn lastupdate_names_the_newest_file_and_a_stamp_rounds_to_its_slot() {
        let t = decode_lastupdate(LASTUPDATE).unwrap();
        assert_eq!(stamp_text(t), "20260917160000");
        let odd: DateTime<Utc> = "2026-09-17T15:59:59Z".parse().unwrap();
        assert_eq!(stamp_text(slot(odd)), "20260917154500");
        assert!(decode_lastupdate("nothing here").is_err());
    }

    #[test]
    fn events_are_placed_named_and_worded_and_the_unplaceable_are_counted() {
        let stamp: DateTime<Utc> = "2026-09-17T15:00:00Z".parse().unwrap();
        let d = decode_file(CSV.as_bytes(), stamp, &SourceId::new("gdelt"));
        assert_eq!(d.malformed, 1);
        assert_eq!(d.unplaced, 1);
        assert_eq!(d.observations.len(), 2);
        let visit = &d.observations[0];
        assert_eq!(visit.entity.key, "gdelt:1323535517");
        assert_eq!(visit.entity.kind, EntityKind::Event);
        assert_eq!(visit.observed_at, stamp, "the file stamp, not the Day column that says 2025");
        assert_eq!(visit.label.as_deref(), Some("make a visit: United Kingdom"));
        assert_eq!(visit.attrs["event"], "make a visit");
        assert_eq!(visit.attrs["place_precision"], "country");
        assert_eq!(visit.attrs["actor1"]["name"], "EGYPTIAN");
        assert_eq!(visit.attrs["actor2"]["country"], "GBR");
        assert_eq!(visit.attrs["day"], "20250917");
        assert_eq!(visit.attrs["tone"], 0.31);
        assert!(visit.attrs.get("root_event").is_none());
        let riot = &d.observations[1];
        assert_eq!(riot.attrs["event"], "engage in violent protest for policy change");
        assert_eq!(riot.attrs["root"], "protest");
        assert_eq!(riot.attrs["quad"], "verbal conflict");
        assert_eq!(riot.attrs["goldstein"], -7.5);
        assert_eq!(riot.attrs["actor1"]["type"], "COP");
        assert_eq!(riot.attrs["actor2"]["name"], "PROTESTER");
        assert_eq!(riot.attrs["place"], "Tbilisi, T'bilisi, Georgia");
        assert_eq!(riot.attrs["root_event"], true);
        assert!((riot.position.unwrap().lat - 41.6941).abs() < 1e-9);
    }
}
