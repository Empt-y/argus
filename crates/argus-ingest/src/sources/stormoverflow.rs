//! Storm overflows: where the UK sewer network discharges into rivers and sea,
//! and which of them are doing it right now.
//!
//! When rainfall exceeds what a combined sewer can carry, the excess is
//! released at a storm overflow rather than backing up into streets and houses.
//! Every English and Welsh water company, and Scottish Water, publishes the
//! live state of its monitored overflows under the Water UK "Stream" commitment
//! — an event-duration monitor on each outfall, reporting discharging, not
//! discharging, or offline.
//!
//! Nine companies, nine keyless ArcGIS FeatureServers, ~15,300 outfalls. Eight
//! of them share one schema; Scottish Water publishes its own, richer one. Both
//! are decoded here into the same two things:
//!
//!   A [`EntityKind::Station`] per outfall — a fixed installation reporting a
//!   state, which is exactly what an EDM is.
//!
//!   A [`EntityKind::Event`] per discharge in the last 48 hours, keyed by the
//!   outfall and the instant the discharge began. That key is what makes a
//!   discharge alertable: a new spill is a new entity crossing a geofence,
//!   where the outfall itself is a fixture that never moves and would never
//!   trigger anything after the first time it was seen.
//!
//! ## Why the poll time is the observation time
//!
//! It is tempting to use the company's own `LastUpdated` stamp instead. It
//! looks more honest, and it is nearly free: the store's
//! `(kind, key, observed_at, source)` conflict key would collapse every poll
//! between two republishes into no writes at all.
//!
//! It does not survive contact with the feeds, because the nine companies do
//! not mean the same thing by the field. Seven stamp it in bulk when the feed
//! is republished — 2,251 of United Utilities' 2,252 records carry one
//! identical value, an hour old. Northumbrian Water stamps each record when
//! that record last *changed*: its median stamp is eight days old and its
//! oldest is fifty-seven. Both are reasonable readings of "last updated"; only
//! one of them is a statement about the feed.
//!
//! Dated by that field, 1,301 of Northumbrian's 1,575 outfalls fall outside
//! the 24-hour `Station` horizon and the entire North East silently leaves the
//! map — not because the monitors are broken, but because one company's field
//! means something different. (`StatusStart` fails the same way and harder:
//! its values go back to 2024.)
//!
//! So the observation time is when this driver fetched the feed, which is the
//! one thing that is true of all nine: the company served this assertion now.
//! What the company's stamp actually tells us — how long since this particular
//! record moved — is kept, but as [`Quality`] and an attribute rather than as
//! the clock. An outfall whose own stamp is older than the horizon, or whose
//! monitor reports offline, is marked [`Quality::Stale`]: still drawn, because
//! a monitor that stopped reporting is worth seeing, but never dressed up as a
//! live reading.
//!
//! The cost of not deduping is 15,300 rows a poll, four polls an hour. Set
//! against the ADS-B layer's ~10,000 rows every fifteen seconds it is under
//! three per cent of what the store already absorbs, which is the right price
//! for not letting a field's spelling decide whether a county exists.
//!
//! ## Licence
//!
//! The Stream portal asserts open terms, but not one of the nine FeatureServers
//! carries a non-empty `copyrightText` — Northumbrian's says only "Northumbrian
//! Water". The attribution below therefore credits each company by name and
//! says plainly that the terms are the company's own, rather than asserting an
//! open licence the endpoint does not.

use crate::http::HttpClient;
use argus_core::entity::{EntityId, EntityKind, Observation, Position, Quality};
use argus_core::BoundingBox;
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use std::collections::HashSet;

/// The shared layer. Nine sources feed it; a client toggles "storm overflows",
/// not "Wessex Water".
const LAYER: &str = "storm-overflows";

/// The companies publish on their own cycle — hourly at worst, every fifteen
/// minutes at best. Polling at fifteen costs nothing when nothing has changed,
/// because an unchanged feed republishes nothing new to write.
const CADENCE_SECS: u64 = 900;

/// How far back a finished discharge stays interesting.
///
/// Scottish Water picked this window itself — it publishes
/// `TOTAL_DURATION_PAST_48_HRS` — and it is the right one: whether a river had
/// sewage in it yesterday is the question people actually ask, and the
/// `Event` horizon of seven days is comfortably longer, so nothing is
/// emitted that the store would then hide.
const EVENT_WINDOW_HOURS: i64 = 48;

/// ArcGIS caps a page at 2,000 features — 1,000 on Anglian's server. Asking for
/// 1,000 is the largest page every one of the nine will actually serve.
const PAGE_SIZE: usize = 1000;

/// A stop on the paging loop, so a server that answers every offset with a full
/// page cannot spin forever. The largest feed is Severn Trent at 2,412.
const MAX_PAGES: usize = 40;

/// Great Britain and Northern Ireland. These feeds only ever describe here, and
/// declaring it keeps the source out of the per-AOI polling path.
const GB: BoundingBox = BoundingBox {
    west: -8.7,
    south: 49.8,
    east: 2.0,
    north: 61.0,
};

/// Which of the two wire schemas a company publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Schema {
    /// The Water UK common model: `Id`, `Status`, `StatusStart`,
    /// `LatestEventStart`, `LatestEventEnd`, `ReceivingWaterCourse`,
    /// `LastUpdated`. Eight of the nine.
    WaterUk,
    /// Scottish Water's own: asset names, licence numbers, overflow types,
    /// durations, and a four-state status description.
    Scotland,
}

struct Company {
    /// Short, stable prefix on every entity key from this company. Outfall ids
    /// are only unique within a company — Scottish Water and South West Water
    /// both number assets `CSO000042`-style — so the prefix is what stops two
    /// unrelated outfalls in different countries becoming one entity.
    slug: &'static str,
    name: &'static str,
    home: &'static str,
    /// Layer 0 of the FeatureServer. Discovered by title search against
    /// ArcGIS Online rather than by guessing: South West Water publishes theirs
    /// as `NEH_outlets_PROD` and Anglian as
    /// `stream_service_outfall_locations_view`, neither of which is derivable
    /// from the company name.
    base: &'static str,
    /// The layer's object-id field, used purely as a stable sort key for
    /// paging.
    ///
    /// Without an explicit order ArcGIS may return rows in any order per
    /// request, and an unordered `resultOffset` silently drops some outfalls
    /// and repeats others. Every one of the nine has such a field; they do not
    /// agree on its spelling — `OBJECTID` on seven, `ObjectId` on Anglian and
    /// South West Water — which is why it is data here rather than a constant.
    object_id: &'static str,
    schema: Schema,
}

static COMPANIES: &[Company] = &[
    Company {
        slug: "angl",
        name: "Anglian Water",
        home: "https://www.anglianwater.co.uk/",
        base: "https://services3.arcgis.com/VCOY1atHWVcDlvlJ/arcgis/rest/services/stream_service_outfall_locations_view/FeatureServer/0",
        object_id: "ObjectId",
        schema: Schema::WaterUk,
    },
    Company {
        slug: "nwl",
        name: "Northumbrian Water",
        home: "https://www.nwl.co.uk/",
        base: "https://services-eu1.arcgis.com/MSNNjkZ51iVh8yBj/arcgis/rest/services/Northumbrian_Water_Storm_Overflow_Activity_2_view/FeatureServer/0",
        object_id: "OBJECTID",
        schema: Schema::WaterUk,
    },
    Company {
        slug: "swsc",
        name: "Scottish Water",
        home: "https://www.scottishwater.co.uk/",
        base: "https://services3.arcgis.com/Bb8lfThdhugyc4G3/arcgis/rest/services/Scottish_Water_Storm_Overflow_Activity/FeatureServer/0",
        object_id: "OBJECTID",
        schema: Schema::Scotland,
    },
    Company {
        slug: "stw",
        name: "Severn Trent Water",
        home: "https://www.stwater.co.uk/",
        base: "https://services1.arcgis.com/NO7lTIlnxRMMG9Gw/arcgis/rest/services/Severn_Trent_Water_Storm_Overflow_Activity/FeatureServer/0",
        object_id: "OBJECTID",
        schema: Schema::WaterUk,
    },
    Company {
        slug: "sww",
        name: "South West Water",
        home: "https://www.southwestwater.co.uk/",
        base: "https://services-eu1.arcgis.com/OMdMOtfhATJPcHe3/arcgis/rest/services/NEH_outlets_PROD/FeatureServer/0",
        object_id: "ObjectId",
        schema: Schema::WaterUk,
    },
    Company {
        slug: "tw",
        name: "Thames Water",
        home: "https://www.thameswater.co.uk/",
        base: "https://services2.arcgis.com/g6o32ZDQ33GpCIu3/arcgis/rest/services/Thames_Water_Storm_Overflow_Activity_(Production)_view/FeatureServer/0",
        object_id: "OBJECTID",
        schema: Schema::WaterUk,
    },
    Company {
        slug: "uu",
        name: "United Utilities",
        home: "https://www.unitedutilities.com/",
        base: "https://services5.arcgis.com/5eoLvR0f8HKb7HWP/arcgis/rest/services/United_Utilities_Storm_Overflow_Activity/FeatureServer/0",
        object_id: "OBJECTID",
        schema: Schema::WaterUk,
    },
    Company {
        slug: "wsx",
        name: "Wessex Water",
        home: "https://www.wessexwater.co.uk/",
        base: "https://services.arcgis.com/3SZ6e0uCvPROr4mS/arcgis/rest/services/Wessex_Water_Storm_Overflow_Activity/FeatureServer/0",
        object_id: "OBJECTID",
        schema: Schema::WaterUk,
    },
    Company {
        slug: "yw",
        name: "Yorkshire Water",
        home: "https://www.yorkshirewater.com/",
        base: "https://services-eu1.arcgis.com/1WqkK5cDKUbF0CkH/arcgis/rest/services/Yorkshire_Water_Storm_Overflow_Activity/FeatureServer/0",
        object_id: "OBJECTID",
        schema: Schema::WaterUk,
    },
];

pub struct StormOverflows {
    descriptor: SourceDescriptor,
    http: HttpClient,
    company: &'static Company,
}

impl StormOverflows {
    /// Every company, as separate sources sharing one layer.
    ///
    /// Separate rather than chained: a chain is a failover between providers of
    /// the *same* data, and these are nine disjoint regions. Thames being down
    /// must not stop Yorkshire being polled, and the source-health panel should
    /// name the company that is failing.
    pub fn all(http: HttpClient) -> Vec<Self> {
        COMPANIES.iter().map(|c| Self::new(http.clone(), c)).collect()
    }

    fn new(http: HttpClient, company: &'static Company) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new(format!("storm-overflows-{}", company.slug)),
                layer_id: LayerId::new(LAYER),
                display_name: format!("Storm overflows ({})", company.name),
                kind: EntityKind::Station,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Fixed { bbox: GB },
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: company.name.into(),
                    url: company.home.into(),
                    // Not asserted as open. The Stream portal describes these as
                    // open data, but every one of the nine FeatureServers
                    // publishes an empty or bare `copyrightText`, and repeating
                    // a licence the endpoint does not state would be the same
                    // kind of quiet lie this project avoids elsewhere.
                    license: "Published by the water company under its own terms".into(),
                    notice: Some(format!(
                        "Storm overflow monitoring data published by {}",
                        company.name
                    )),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
            company,
        }
    }

    /// Page through every feature in the layer.
    ///
    /// Paged rather than asked for in one request because the servers cap a
    /// page at 2,000 features and Anglian's at 1,000, and a capped response is
    /// not an error — it is a truncation that looks exactly like a complete
    /// answer. In GeoJSON the `exceededTransferLimit` flag that would warn of it
    /// is tucked inside a `properties` object that is *absent* on the last page,
    /// so the reliable stop is a page shorter than the one requested.
    async fn fetch(&self) -> Result<Vec<Feature>, SourceError> {
        let mut features = Vec::new();
        for page in 0..MAX_PAGES {
            let url = format!(
                "{}/query?where=1%3D1&outFields=*&returnGeometry=true&f=geojson\
                 &orderByFields={}&resultOffset={}&resultRecordCount={PAGE_SIZE}",
                self.company.base,
                self.company.object_id,
                page * PAGE_SIZE,
            );
            let batch: FeatureCollection = self.http.get_json(&url).await?;
            let n = batch.features.len();
            features.extend(batch.features);
            if n < PAGE_SIZE {
                return Ok(features);
            }
        }
        // Not an error: MAX_PAGES of outfalls is far more than any company has,
        // so reaching it means the server is ignoring `resultOffset` and
        // re-serving page one. Keep what came back rather than discarding a
        // usable picture, and say so.
        tracing::warn!(
            source = %self.descriptor.id,
            pages = MAX_PAGES,
            "paging stopped at the cap; the server may be ignoring resultOffset"
        );
        Ok(features)
    }
}

#[async_trait::async_trait]
impl Source for StormOverflows {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let features = self.fetch().await?;
        Ok(decode(
            features,
            self.company,
            &self.descriptor.id,
            Utc::now(),
        ))
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FeatureCollection {
    #[serde(default)]
    features: Vec<Feature>,
}

#[derive(Debug, Deserialize)]
struct Feature {
    geometry: Option<PointGeometry>,
    properties: Properties,
}

#[derive(Debug, Deserialize)]
struct PointGeometry {
    /// `[lon, lat]`. GeoJSON output is always WGS84, which matters more than it
    /// looks: Scottish Water's layer is natively EPSG:27700, and asking for
    /// `f=json` returns British National Grid eastings and northings that would
    /// deserialise perfectly and place every Scottish outfall in the Gulf of
    /// Guinea. `f=geojson` is what makes the server reproject.
    coordinates: Vec<f64>,
}

/// Both schemas in one struct, because seven of the eleven Water UK fields
/// differ only in case between companies and serde aliases cost less than a
/// second decoder.
///
/// South West Water publishes the common model in lowerCamelCase —
/// `status`, `statusStart`, `latestEventStart` — where the other seven use
/// PascalCase. Field-for-field the same data.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Properties {
    // -- Water UK common model
    #[serde(rename = "Id", alias = "id", alias = "ID")]
    id: Option<String>,
    #[serde(rename = "Status", alias = "status")]
    status: Option<i64>,
    #[serde(rename = "StatusStart", alias = "statusStart")]
    status_start: Option<i64>,
    #[serde(rename = "LatestEventStart", alias = "latestEventStart")]
    latest_event_start: Option<i64>,
    #[serde(rename = "LatestEventEnd", alias = "latestEventEnd")]
    latest_event_end: Option<i64>,
    #[serde(rename = "ReceivingWaterCourse", alias = "receivingWaterCourse")]
    receiving_water_course: Option<String>,
    #[serde(rename = "LastUpdated", alias = "lastUpdated")]
    last_updated: Option<i64>,

    // -- Scottish Water
    #[serde(rename = "ASSET_ID")]
    asset_id: Option<String>,
    #[serde(rename = "ASSET_NAME")]
    asset_name: Option<String>,
    /// 13 overflowing, 14 recent overflow, 15 no overflows, 16 no data.
    #[serde(rename = "STATUS_ID")]
    status_id: Option<i64>,
    #[serde(rename = "STATUS_DESCRIPTION")]
    status_description: Option<String>,
    /// ISO-8601 in a *string* field, not an ArcGIS date — so it arrives as
    /// text, and an absent value arrives as `""` rather than null.
    #[serde(rename = "START_DATETIME")]
    start_datetime: Option<String>,
    #[serde(rename = "END_DATETIME")]
    end_datetime: Option<String>,
    #[serde(rename = "DURATION_MIN")]
    duration_min: Option<f64>,
    #[serde(rename = "TOTAL_DURATION_PAST_48_HRS")]
    duration_48h_min: Option<i64>,
    #[serde(rename = "RECEIVING_WATER")]
    receiving_water: Option<String>,
    #[serde(rename = "OVERFLOW_TYPE")]
    overflow_type: Option<String>,
    #[serde(rename = "LICENCE_NUMBER")]
    licence_number: Option<String>,
    #[serde(rename = "LOCAL_AUTHORITY_NAME")]
    local_authority: Option<String>,
    #[serde(rename = "LAST_TRANSMITTED_DATETIME")]
    last_transmitted: Option<String>,
    /// "true" / "false", as text.
    #[serde(rename = "IS_OPERATING_IN_DRY_WEATHER")]
    dry_weather: Option<String>,
}

// --- decoding --------------------------------------------------------------

/// What an outfall is doing, normalised across the two schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Discharging,
    NotDischarging,
    /// The monitor is not reporting. Deliberately its own state and never
    /// folded into `NotDischarging`: "we do not know" and "it is not
    /// discharging" are different claims, and 460 of the country's outfalls
    /// were in this state when this driver was written. Saying a river is clear
    /// because nobody is watching it is the worst thing this layer could do.
    Offline,
}

impl State {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Discharging => "discharging",
            Self::NotDischarging => "not_discharging",
            Self::Offline => "offline",
        }
    }
}

/// One outfall, after the two schemas have been reconciled.
struct Outfall<'a> {
    id: &'a str,
    name: Option<&'a str>,
    state: State,
    /// The publisher's own words for the state, where it gives them.
    state_text: Option<&'a str>,
    water: Option<&'a str>,
    /// When the company last published this record.
    published_at: Option<DateTime<Utc>>,
    /// When the outfall entered its current state.
    state_since: Option<DateTime<Utc>>,
    event_start: Option<DateTime<Utc>>,
    event_end: Option<DateTime<Utc>>,
}

fn decode(
    features: Vec<Feature>,
    company: &Company,
    source_id: &SourceId,
    now: DateTime<Utc>,
) -> Vec<Observation> {
    let mut out = Vec::with_capacity(features.len());
    let mut seen: HashSet<String> = HashSet::with_capacity(features.len());

    for feature in &features {
        let Some((lon, lat)) = point(feature.geometry.as_ref()) else {
            continue;
        };
        let Some(outfall) = reconcile(&feature.properties, company.schema) else {
            continue;
        };

        // Keyed on the outfall's own id, never on `OBJECTID`. Anglian publishes
        // AWS00528 twice — two rows identical in every field except the object
        // id — and object ids are a server-side artefact that can be renumbered
        // by a republish. Keying on the id makes the duplicate collapse into
        // one outfall, which is what it is.
        let key = format!("{}:{}", company.slug, outfall.id);
        if !seen.insert(key.clone()) {
            continue;
        }

        let position = Position {
            lon,
            lat,
            alt_m: None,
            datum: argus_core::entity::AltitudeDatum::Geoid,
        };

        out.push(station(&outfall, &key, position, source_id, company, now));

        // A discharge is only emitted once its start is known. An outfall that
        // is discharging with no recorded start gets no event — inventing one
        // at `now` would restart it on every poll and fire a fresh alert each
        // time.
        if let Some(start) = outfall.event_start
            && (now - start).num_hours() < EVENT_WINDOW_HOURS
            && start <= now
        {
            out.push(discharge(&outfall, start, position, source_id, company));
        }
    }
    out
}

fn point(geometry: Option<&PointGeometry>) -> Option<(f64, f64)> {
    let coords = &geometry?.coordinates;
    match coords[..] {
        [lon, lat, ..] => Some((lon, lat)),
        _ => None,
    }
}

/// Fold whichever schema this is into the common shape.
fn reconcile(p: &Properties, schema: Schema) -> Option<Outfall<'_>> {
    match schema {
        Schema::WaterUk => Some(Outfall {
            id: p.id.as_deref()?,
            name: None,
            state: match p.status {
                Some(1) => State::Discharging,
                Some(0) => State::NotDischarging,
                // -1 is the published spelling of "offline", and anything
                // unrecognised is treated the same way: an unknown code is not
                // evidence that a river is clear.
                _ => State::Offline,
            },
            state_text: None,
            water: p.receiving_water_course.as_deref().filter(|s| !s.is_empty()),
            published_at: p.last_updated.and_then(epoch_ms),
            state_since: p.status_start.and_then(epoch_ms),
            event_start: p.latest_event_start.and_then(epoch_ms),
            event_end: p.latest_event_end.and_then(epoch_ms),
        }),
        Schema::Scotland => {
            let state = match p.status_id {
                Some(13) => State::Discharging,
                // 14 "Recent Overflow" and 15 "No Overflows" are both "not
                // discharging now"; the recency they distinguish is carried by
                // the event, which is the thing that actually knows when.
                Some(14 | 15) => State::NotDischarging,
                _ => State::Offline,
            };
            let start = p.start_datetime.as_deref().and_then(iso);
            // An empty `END_DATETIME` does not mean "still going". 603 of
            // Scottish Water's 2,073 assets carry an empty end while only 36
            // were overflowing — the field is simply blank for assets with no
            // completed event on record. The status is the only thing that
            // knows whether a discharge is in progress, so the end is reported
            // as absent and the *state* decides whether that means ongoing.
            let end = p.end_datetime.as_deref().and_then(iso);
            Some(Outfall {
                id: p.asset_id.as_deref()?,
                name: p.asset_name.as_deref().filter(|s| !s.is_empty()),
                state,
                state_text: p.status_description.as_deref().filter(|s| !s.is_empty()),
                water: p.receiving_water.as_deref().filter(|s| !s.is_empty()),
                published_at: p.last_transmitted.as_deref().and_then(iso),
                // Scottish Water publishes no equivalent of `StatusStart`.
                state_since: None,
                event_start: start,
                event_end: end,
            })
        }
    }
}

fn station(
    o: &Outfall<'_>,
    key: &str,
    position: Position,
    source_id: &SourceId,
    company: &Company,
    now: DateTime<Utc>,
) -> Observation {
    // Two different facts, and conflating them is how a layer starts lying.
    // The company is asserting this *now* — it served the record this second —
    // but the reading behind it may be months old, and an outfall whose monitor
    // stopped reporting must not be drawn like one that just checked in.
    let quality = if o.state == State::Offline
        || o.published_at.is_none_or(|t| now - t > station_horizon())
    {
        Quality::Stale
    } else {
        Quality::Live
    };

    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };
    put("state", serde_json::json!(o.state.as_str()));
    put("state_text", serde_json::json!(o.state_text));
    put("company", serde_json::json!(company.name));
    put("outfall_id", serde_json::json!(o.id));
    put("receiving_water", serde_json::json!(o.water));
    put("state_since", serde_json::json!(o.state_since.map(rfc3339)));
    // The company's own stamp, kept as data rather than used as the clock —
    // see the module docs. Seven companies mean "when the feed was published"
    // by it and Northumbrian means "when this record last changed", so it is
    // reported for what it is and never read as freshness.
    put("record_updated", serde_json::json!(o.published_at.map(rfc3339)));
    put("stale", serde_json::json!(quality == Quality::Stale));
    put("last_discharge_start", serde_json::json!(o.event_start.map(rfc3339)));
    put("last_discharge_end", serde_json::json!(o.event_end.map(rfc3339)));

    // The label is what a fence's `label_contains` matches and what a client
    // draws, so it names the water rather than the asset: someone watching a
    // river knows its name and will never know the outfall's reference.
    let label = match (o.water, o.name) {
        (Some(water), _) => format!("Storm overflow — {water}"),
        (None, Some(name)) => format!("Storm overflow — {name}"),
        (None, None) => format!("Storm overflow {}", o.id),
    };

    Observation::new(
        source_id.clone(),
        EntityId::new(EntityKind::Station, key.to_string()),
        // When the company served us this assertion. The one instant that means
        // the same thing across all nine feeds.
        now,
        quality,
    )
    .with_position(position)
    .with_label(label)
    .with_attrs(serde_json::Value::Object(attrs))
}

fn discharge(
    o: &Outfall<'_>,
    start: DateTime<Utc>,
    position: Position,
    source_id: &SourceId,
    company: &Company,
) -> Observation {
    let ongoing = o.state == State::Discharging;
    // An end that is not after its start is not an end. The Water UK feeds
    // carry pairs like start 09:07:40 / end 09:07:54 that are fine, and others
    // where the end belongs to the *previous* discharge and precedes the start
    // it is filed against; reporting that would give a discharge a negative
    // duration.
    let end = o.event_end.filter(|e| !ongoing && *e > start);

    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };
    put("company", serde_json::json!(company.name));
    put("outfall_id", serde_json::json!(o.id));
    put("receiving_water", serde_json::json!(o.water));
    put("started", serde_json::json!(rfc3339(start)));
    put("ended", serde_json::json!(end.map(rfc3339)));
    put("ongoing", serde_json::json!(ongoing));
    put(
        "duration_minutes",
        serde_json::json!(end.map(|e| (e - start).num_minutes())),
    );

    let water = o.water.unwrap_or("an unnamed watercourse");
    let label = if ongoing {
        format!("Discharging into {water}")
    } else {
        format!("Discharged into {water}")
    };

    Observation::new(
        source_id.clone(),
        // The start is part of the key, which is what makes each discharge its
        // own entity: a new spill at the same outfall is a new thing entering a
        // geofence, and the same spill re-read on the next poll is not.
        EntityId::new(
            EntityKind::Event,
            format!("{}:{}:{}", company.slug, o.id, start.timestamp()),
        ),
        // When the discharge began. An event is a fact about a moment.
        start,
        Quality::Live,
    )
    .with_position(position)
    .with_label(label)
    .with_attrs(serde_json::Value::Object(attrs))
}

/// Epoch milliseconds, as the Water UK feeds publish their dates.
///
/// Zero is rejected along with anything unrepresentable: several feeds use it
/// for "no date" rather than null, and 1970 in a `StatusStart` is a missing
/// value wearing a timestamp.
fn epoch_ms(ms: i64) -> Option<DateTime<Utc>> {
    if ms == 0 {
        return None;
    }
    Utc.timestamp_millis_opt(ms).single()
}

/// ISO-8601, as Scottish Water publishes its dates — in string fields, so an
/// absent value is `""` and not null.
fn iso(s: &str) -> Option<DateTime<Utc>> {
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Utc))
}

/// How old a `Station` may be and still be drawn. Read from the kind rather
/// than repeated, so the staleness mark and the store's horizon cannot drift
/// apart — a monitor marked live that the store then hides would be the worst
/// of both.
fn station_horizon() -> chrono::Duration {
    EntityKind::Station
        .live_horizon()
        .expect("stations have a live horizon")
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("a test instant")
            .with_timezone(&Utc)
    }

    fn ms(s: &str) -> i64 {
        at(s).timestamp_millis()
    }

    fn uu() -> &'static Company {
        COMPANIES.iter().find(|c| c.slug == "uu").expect("United Utilities")
    }

    fn scotland() -> &'static Company {
        COMPANIES.iter().find(|c| c.slug == "swsc").expect("Scottish Water")
    }

    fn feature(properties: Properties) -> Feature {
        Feature {
            geometry: Some(PointGeometry {
                coordinates: vec![-3.181, 54.742],
            }),
            properties,
        }
    }

    /// A live United Utilities record, field for field.
    fn water_uk(status: i64) -> Properties {
        Properties {
            id: Some("UUP01024".into()),
            status: Some(status),
            status_start: Some(ms("2026-09-08T02:22:00Z")),
            latest_event_start: Some(ms("2026-09-08T02:22:00Z")),
            latest_event_end: None,
            receiving_water_course: Some("River Ellen".into()),
            last_updated: Some(ms("2026-09-08T12:16:34Z")),
            ..Default::default()
        }
    }

    /// A live Scottish Water record, field for field.
    fn scottish(status_id: i64) -> Properties {
        Properties {
            asset_id: Some("CSO000042".into()),
            asset_name: Some("DALMELLINGTON WWTW CSO".into()),
            status_id: Some(status_id),
            status_description: Some("OF - Overflowing".into()),
            start_datetime: Some("2026-09-08T09:10:00.000Z".into()),
            end_datetime: Some(String::new()),
            receiving_water: Some("Cummock Water".into()),
            last_transmitted: Some("2026-09-08T09:25:00.000Z".into()),
            ..Default::default()
        }
    }

    fn source() -> SourceId {
        SourceId::new("storm-overflows-uu")
    }

    fn kinds(obs: &[Observation]) -> Vec<EntityKind> {
        obs.iter().map(|o| o.entity.kind).collect()
    }

    #[test]
    fn an_outfall_is_observed_when_the_company_served_it_not_when_it_last_changed() {
        // The company is asserting this now. Its own `LastUpdated` is kept as
        // data, because the nine feeds do not agree on what it means.
        let now = at("2026-09-08T13:00:00Z");
        let obs = decode(vec![feature(water_uk(0))], uu(), &source(), now);
        assert_eq!(obs[0].observed_at, now);
        assert_eq!(obs[0].quality, Quality::Live);
        assert_eq!(
            obs[0].attrs["record_updated"],
            serde_json::json!("2026-09-08T12:16:34Z")
        );
    }

    #[test]
    fn northumbrians_eight_day_old_stamps_do_not_erase_the_north_east() {
        // Northumbrian stamps each record when it last *changed*, not when the
        // feed was published: 1,301 of its 1,575 outfalls carry a stamp more
        // than a day old, the oldest fifty-seven days. Dated by that field they
        // all fall outside the 24-hour station horizon and the county vanishes.
        let mut p = water_uk(0);
        p.last_updated = Some(ms("2026-09-03T15:17:00Z"));
        let now = at("2026-09-08T13:00:00Z");
        let obs = decode(vec![feature(p)], uu(), &source(), now);

        let horizon = EntityKind::Station.live_horizon().expect("stations expire");
        assert!(
            now - obs[0].observed_at < horizon,
            "an outfall must stay inside the station horizon or it is invisible"
        );
        // Drawn, but never dressed up as a fresh reading.
        assert_eq!(obs[0].quality, Quality::Stale);
        assert_eq!(obs[0].attrs["stale"], serde_json::json!(true));
    }

    #[test]
    fn an_offline_monitor_is_stale_however_recently_the_feed_republished_it() {
        // The record is seconds old and the reading behind it does not exist.
        let mut p = water_uk(-1);
        p.last_updated = Some(ms("2026-09-08T12:59:00Z"));
        let obs = decode(vec![feature(p)], uu(), &source(), at("2026-09-08T13:00:00Z"));
        assert_eq!(obs[0].quality, Quality::Stale);
        assert_eq!(obs[0].attrs["state"], serde_json::json!("offline"));
    }

    #[test]
    fn how_long_an_outfall_has_been_in_its_state_is_reported_without_dating_it_that_way() {
        // Real `StatusStart` values go back to 2024. It is the fact someone
        // wants to read off the card, and it must never become the clock.
        let mut p = water_uk(0);
        p.status_start = Some(ms("2024-08-12T09:58:00Z"));
        let now = at("2026-09-08T13:00:00Z");
        let obs = decode(vec![feature(p)], uu(), &source(), now);
        assert_eq!(obs[0].observed_at, now);
        assert_eq!(obs[0].attrs["state_since"], serde_json::json!("2024-08-12T09:58:00Z"));
    }

    #[test]
    fn an_offline_monitor_is_never_reported_as_not_discharging() {
        // The single most important distinction in this feed: -1 means nobody
        // is watching, not that the river is clear.
        let obs = decode(vec![feature(water_uk(-1))], uu(), &source(), at("2026-09-08T13:00:00Z"));
        assert_eq!(obs[0].attrs["state"], serde_json::json!("offline"));

        // And an unrecognised code lands there too rather than defaulting clear.
        let obs = decode(vec![feature(water_uk(7))], uu(), &source(), at("2026-09-08T13:00:00Z"));
        assert_eq!(obs[0].attrs["state"], serde_json::json!("offline"));

        // Not discharging is a real reading and stays one.
        let obs = decode(vec![feature(water_uk(0))], uu(), &source(), at("2026-09-08T13:00:00Z"));
        assert_eq!(obs[0].attrs["state"], serde_json::json!("not_discharging"));
    }

    #[test]
    fn a_discharge_is_keyed_by_its_start_so_a_repoll_is_not_a_second_spill() {
        let now = at("2026-09-08T13:00:00Z");
        let first = decode(vec![feature(water_uk(1))], uu(), &source(), now);
        let again = decode(
            vec![feature(water_uk(1))],
            uu(),
            &source(),
            now + chrono::Duration::minutes(15),
        );
        assert_eq!(kinds(&first), vec![EntityKind::Station, EntityKind::Event]);
        let key = |o: &[Observation]| o[1].entity.key.clone();
        assert_eq!(key(&first), "uu:UUP01024:1788834120");
        assert_eq!(
            key(&first),
            key(&again),
            "the same spill polled twice must be one event, or every poll alerts again"
        );
        assert_eq!(first[1].observed_at, at("2026-09-08T02:22:00Z"));
        assert_eq!(first[1].attrs["ongoing"], serde_json::json!(true));
        assert_eq!(first[1].label.as_deref(), Some("Discharging into River Ellen"));
    }

    #[test]
    fn a_discharge_older_than_the_window_is_not_emitted() {
        let mut p = water_uk(0);
        p.latest_event_start = Some(ms("2026-09-04T09:00:00Z"));
        p.latest_event_end = Some(ms("2026-09-04T10:00:00Z"));
        let obs = decode(vec![feature(p)], uu(), &source(), at("2026-09-08T13:00:00Z"));
        assert_eq!(kinds(&obs), vec![EntityKind::Station], "the outfall stays, the stale spill goes");
        // But the station still says when it last spilled, which is the fact
        // the window drops the event for, not the fact itself.
        assert_eq!(
            obs[0].attrs["last_discharge_start"],
            serde_json::json!("2026-09-04T09:00:00Z")
        );
    }

    #[test]
    fn a_finished_discharge_reports_a_duration_and_a_recent_one_is_still_carried() {
        let mut p = water_uk(0);
        p.latest_event_start = Some(ms("2026-09-08T06:00:00Z"));
        p.latest_event_end = Some(ms("2026-09-08T09:30:00Z"));
        let obs = decode(vec![feature(p)], uu(), &source(), at("2026-09-08T13:00:00Z"));
        assert_eq!(kinds(&obs), vec![EntityKind::Station, EntityKind::Event]);
        assert_eq!(obs[1].attrs["ongoing"], serde_json::json!(false));
        assert_eq!(obs[1].attrs["duration_minutes"], serde_json::json!(210));
        assert_eq!(obs[1].label.as_deref(), Some("Discharged into River Ellen"));
    }

    #[test]
    fn an_end_that_precedes_its_start_is_dropped_rather_than_given_a_negative_duration() {
        let mut p = water_uk(0);
        p.latest_event_start = Some(ms("2026-09-08T09:00:00Z"));
        // The end of the *previous* spill, filed against this start.
        p.latest_event_end = Some(ms("2026-09-08T08:00:00Z"));
        let obs = decode(vec![feature(p)], uu(), &source(), at("2026-09-08T13:00:00Z"));
        assert!(obs[1].attrs.get("ended").is_none());
        assert!(obs[1].attrs.get("duration_minutes").is_none());
    }

    #[test]
    fn south_west_waters_lowercase_field_names_decode_as_the_same_thing() {
        // SWW publishes the common model in lowerCamelCase. Decoding it as an
        // unknown schema would silently produce 1,344 outfalls with no id, no
        // status and no position — the sort of failure that looks like an empty
        // county rather than an error.
        let json = serde_json::json!({
            "Id": "SWW00001",
            "status": 1,
            "statusStart": ms("2026-09-08T02:22:00Z"),
            "latestEventStart": ms("2026-09-08T02:22:00Z"),
            "latestEventEnd": null,
            "receivingWaterCourse": "River Otter",
            "lastUpdated": ms("2026-09-08T12:21:33Z"),
        });
        let p: Properties = serde_json::from_value(json).expect("the camelCase form decodes");
        let o = reconcile(&p, Schema::WaterUk).expect("an outfall");
        assert_eq!(o.state, State::Discharging);
        assert_eq!(o.water, Some("River Otter"));
        assert_eq!(o.published_at, Some(at("2026-09-08T12:21:33Z")));
    }

    #[test]
    fn an_empty_scottish_end_date_does_not_mean_the_spill_is_still_running() {
        // 603 of 2,073 Scottish assets carry an empty `END_DATETIME` while only
        // 36 were overflowing. Reading a blank end as "ongoing" would report
        // sixteen times more live sewage discharge into Scottish rivers than
        // there is.
        let obs = decode(
            vec![feature(scottish(15))],
            scotland(),
            &SourceId::new("storm-overflows-swsc"),
            at("2026-09-08T13:00:00Z"),
        );
        assert_eq!(obs[0].attrs["state"], serde_json::json!("not_discharging"));
        assert_eq!(obs[1].attrs["ongoing"], serde_json::json!(false));
        assert!(obs[1].attrs.get("ended").is_none(), "unknown, not invented");

        // The same record with the overflowing status *is* ongoing.
        let obs = decode(
            vec![feature(scottish(13))],
            scotland(),
            &SourceId::new("storm-overflows-swsc"),
            at("2026-09-08T13:00:00Z"),
        );
        assert_eq!(obs[0].attrs["state"], serde_json::json!("discharging"));
        assert_eq!(obs[1].attrs["ongoing"], serde_json::json!(true));
    }

    #[test]
    fn scottish_iso_timestamps_and_the_water_uk_epoch_produce_the_same_instant() {
        let scots = decode(
            vec![feature(scottish(13))],
            scotland(),
            &SourceId::new("storm-overflows-swsc"),
            at("2026-09-08T13:00:00Z"),
        );
        // The station is dated by the poll; the spill by when it began.
        assert_eq!(scots[0].observed_at, at("2026-09-08T13:00:00Z"));
        assert_eq!(
            scots[0].attrs["record_updated"],
            serde_json::json!("2026-09-08T09:25:00Z"),
            "the ISO-8601 string field parses to the same instant the epoch fields do"
        );
        assert_eq!(scots[1].observed_at, at("2026-09-08T09:10:00Z"));
    }

    #[test]
    fn an_outfall_published_twice_is_one_outfall_and_one_spill() {
        // Anglian publishes AWS00528 as two rows differing only in ObjectId.
        // The duplicate must cost neither a second station nor — which would be
        // worse — a second discharge, since a duplicated spill is a duplicated
        // alert about sewage that was only released once.
        let once = decode(vec![feature(water_uk(1))], uu(), &source(), at("2026-09-08T13:00:00Z"));
        let twice = decode(
            vec![feature(water_uk(1)), feature(water_uk(1))],
            uu(),
            &source(),
            at("2026-09-08T13:00:00Z"),
        );
        assert_eq!(kinds(&once), vec![EntityKind::Station, EntityKind::Event]);
        assert_eq!(kinds(&twice), kinds(&once));
    }

    #[test]
    fn two_companies_numbering_an_outfall_alike_stay_two_outfalls() {
        // South West Water and Scottish Water both use `CSO000042`-style asset
        // references. Without the company prefix they would merge into one
        // entity that flips between Devon and Ayrshire on alternate polls.
        let scots = decode(
            vec![feature(scottish(13))],
            scotland(),
            &SourceId::new("storm-overflows-swsc"),
            at("2026-09-08T13:00:00Z"),
        );
        let mut p = water_uk(0);
        p.id = Some("CSO000042".into());
        let english = decode(vec![feature(p)], uu(), &source(), at("2026-09-08T13:00:00Z"));
        assert_ne!(scots[0].entity.key, english[0].entity.key);
        assert_eq!(scots[0].entity.key, "swsc:CSO000042");
    }

    #[test]
    fn an_outfall_with_no_geometry_is_skipped_rather_than_placed_at_null_island() {
        let obs = decode(
            vec![Feature { geometry: None, properties: water_uk(1) }],
            uu(),
            &source(),
            at("2026-09-08T13:00:00Z"),
        );
        assert!(obs.is_empty());
    }

    #[test]
    fn every_company_has_a_distinct_slug_and_a_layer_zero_url() {
        // The slug is half of every entity key from that company; a duplicate
        // would silently merge two companies' outfalls.
        let mut slugs: Vec<&str> = COMPANIES.iter().map(|c| c.slug).collect();
        slugs.sort_unstable();
        let before = slugs.len();
        slugs.dedup();
        assert_eq!(slugs.len(), before, "duplicate company slug");
        assert_eq!(COMPANIES.len(), 9);
        for c in COMPANIES {
            assert!(c.base.ends_with("/FeatureServer/0"), "{} is not a layer URL", c.slug);
            assert!(
                matches!(c.object_id, "OBJECTID" | "ObjectId"),
                "{} has no usable paging sort key",
                c.slug
            );
        }
    }
}
