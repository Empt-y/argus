//! Buses: every vehicle reporting to the Bus Open Data Service, which is
//! every local bus in England that the Department for Transport requires to
//! report, which is nearly all of them. 28,000 on a weekday morning, 375
//! operators, Transport for London's fleet among them.
//!
//! The wire format is SIRI-VM, an XML standard for vehicle monitoring, and
//! the query is a bounding box. The service also publishes GTFS-RT, which is
//! an eighth of the size on the wire and was not used: it carries a numeric
//! route id that means nothing without the static timetable, no operator, no
//! line name and no destination, and its vehicle id (`3300`) is only unique
//! within an operator. SIRI-VM has all of those, and `OperatorRef` plus
//! `VehicleRef` is unique across the whole feed — checked across all 28,088
//! records in one response, not assumed from the schema.
//!
//! ## This layer costs what the config says it costs
//!
//! The whole of England is 31 MB of XML per request, uncompressed, because
//! the service sends no `Content-Encoding` however it is asked. At a
//! thirty-second cadence that is 90 GB a day down the wire and, after the
//! store's dedupe, some 50 million rows — twenty times everything else in
//! this workspace put together. So the source is [`Coverage::Bounded`] and
//! asks only about the declared areas of interest, and the default cadence is
//! a minute rather than the twenty seconds aircraft get. A deployment whose
//! areas of interest cover the country gets the country, at the price the
//! country costs; `cadence_secs` in the source's config is the dial.
//!
//! The bounding box is honest, which is worth saying after the METAR query
//! API: England split into a northern and a southern half returned 19,054
//! and 9,036 vehicles against 28,092 for the whole, which is the two buses
//! that moved between requests and not a thinning.
//!
//! ## A quarter of the feed is not where it says
//!
//! Every response lists the last position the service has for every vehicle
//! it has ever heard from recently, and "recently" is generous: in one
//! response 7,269 of 28,088 records were more than five minutes old, 5,349
//! more than an hour, 2,009 more than six hours, and the oldest was a day.
//! Those buses are in depots. A record older than [`STALE`] is dropped here
//! rather than drawn at a stop it left this morning, and the count is logged
//! so a feed that has stopped updating shows as a layer emptying out rather
//! than one quietly freezing.
//!
//! Names arrive with underscores for spaces from about half the operators
//! (`Joyce_Green_Lane_Terminus`, `Barrack_Row`); 12,915 of 27,361 destination
//! names in one response. They are shown to a person, so the underscores
//! become spaces.

use crate::http::HttpClient;
use argus_core::BoundingBox;
use argus_core::entity::{
    AltitudeDatum, EntityId, EntityKind, Kinematics, Observation, Position, Quality,
};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

const FEED_URL: &str = "https://data.bus-data.dft.gov.uk/api/v1/datafeed/";

/// The key the config supplies under `[sources.buses] credentials`.
pub const API_KEY: &str = "api_key";

/// Buses report every thirty seconds or so and the feed's own
/// `ShortestPossibleCycle` is five. A minute is the trade the module
/// documentation describes: half the fidelity of aircraft for a layer that
/// produces twenty times the rows.
const CADENCE_SECS: u64 = 60;

/// A vehicle whose last report is older than this is not drawn. Ten minutes
/// is twenty missed reports for a bus in service, and well short of the
/// hours-old records the feed carries for buses that have finished.
const STALE: Duration = Duration::minutes(10);

/// Where the service has vehicles. A global or larger box is clipped to this
/// before asking, so a deployment with no areas of interest asks for England
/// rather than for the planet — the service answers either way, but the
/// request should say what it means.
const EXTENT: BoundingBox = BoundingBox {
    west: -6.5,
    south: 49.8,
    east: 2.0,
    north: 55.9,
};

pub struct Buses {
    descriptor: SourceDescriptor,
    http: HttpClient,
    api_key: Option<String>,
}

impl Buses {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("buses"),
                layer_id: LayerId::new("buses"),
                display_name: "Buses (Bus Open Data Service)".into(),
                kind: EntityKind::Vehicle,
                cadence: Cadence::every(CADENCE_SECS),
                // Asked about a box at a time, so the areas of interest decide
                // the cost — see the module documentation.
                coverage: Coverage::Bounded,
                auth: AuthRequirement::Required {
                    config_key: API_KEY.into(),
                },
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "Department for Transport, Bus Open Data Service".into(),
                    url: "https://www.bus-data.dft.gov.uk/".into(),
                    license: "Open Government Licence v3.0".into(),
                    notice: Some(
                        "Contains public sector information licensed under the Open Government Licence v3.0"
                            .into(),
                    ),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
            api_key: None,
        }
    }

    pub fn with_api_key(mut self, key: Option<String>) -> Self {
        self.api_key = key.filter(|k| !k.trim().is_empty());
        self
    }

    fn url(&self, bbox: &BoundingBox) -> Result<String, SourceError> {
        let key = self
            .api_key
            .as_deref()
            .ok_or_else(|| SourceError::Auth("no BODS api_key configured".into()))?;
        // The service's order is minLng,minLat,maxLng,maxLat.
        Ok(format!(
            "{FEED_URL}?api_key={key}&boundingBox={:.4},{:.4},{:.4},{:.4}",
            bbox.west, bbox.south, bbox.east, bbox.north
        ))
    }
}

/// The box to ask about: the requested one, clipped to where the service
/// has anything. `None` if they do not overlap at all, in which case there
/// is nothing to ask.
fn clip(requested: Option<BoundingBox>) -> Option<BoundingBox> {
    let r = requested.unwrap_or(BoundingBox::GLOBAL);
    let b = BoundingBox {
        west: r.west.max(EXTENT.west),
        south: r.south.max(EXTENT.south),
        east: r.east.min(EXTENT.east),
        north: r.north.min(EXTENT.north),
    };
    (b.west < b.east && b.south < b.north).then_some(b)
}

#[async_trait::async_trait]
impl Source for Buses {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let Some(bbox) = clip(ctx.bbox) else {
            return Ok(Vec::new());
        };
        let url = self.url(&bbox)?;
        let bytes = self.http.get_bytes(&url).await?;
        let text = String::from_utf8_lossy(&bytes);
        let decoded = decode(&text, &self.descriptor.id, Utc::now())?;
        tracing::debug!(
            source = %self.descriptor.id,
            emitted = decoded.observations.len(),
            stale = decoded.stale,
            malformed = decoded.malformed,
            "buses decoded"
        );
        Ok(decoded.observations)
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Siri {
    #[serde(rename = "ServiceDelivery")]
    service_delivery: ServiceDelivery,
}

#[derive(Debug, Deserialize)]
struct ServiceDelivery {
    #[serde(rename = "VehicleMonitoringDelivery", default)]
    deliveries: Vec<Delivery>,
}

#[derive(Debug, Deserialize)]
struct Delivery {
    #[serde(rename = "VehicleActivity", default)]
    activities: Vec<Activity>,
}

#[derive(Debug, Deserialize)]
struct Activity {
    #[serde(rename = "RecordedAtTime")]
    recorded_at: Option<String>,
    #[serde(rename = "MonitoredVehicleJourney")]
    journey: Option<Journey>,
}

/// Everything is optional text. Across 28,088 live records only the
/// operator, vehicle, direction and position were present on every one;
/// bearing was missing from 4,900 and the destination from 700. An element
/// that is present but empty (`<Bearing/>`) is an empty string, which is why
/// the numbers are parsed by hand rather than declared numeric.
#[derive(Debug, Deserialize)]
struct Journey {
    #[serde(rename = "LineRef")]
    line_ref: Option<String>,
    #[serde(rename = "PublishedLineName")]
    line_name: Option<String>,
    #[serde(rename = "DirectionRef")]
    direction: Option<String>,
    #[serde(rename = "OperatorRef")]
    operator: Option<String>,
    #[serde(rename = "OriginName")]
    origin: Option<String>,
    #[serde(rename = "DestinationName")]
    destination: Option<String>,
    #[serde(rename = "OriginAimedDepartureTime")]
    aimed_departure: Option<String>,
    #[serde(rename = "DestinationAimedArrivalTime")]
    aimed_arrival: Option<String>,
    #[serde(rename = "VehicleLocation")]
    location: Option<Location>,
    #[serde(rename = "Bearing")]
    bearing: Option<String>,
    #[serde(rename = "VehicleRef")]
    vehicle_ref: Option<String>,
    #[serde(rename = "BlockRef")]
    block: Option<String>,
    #[serde(rename = "Occupancy")]
    occupancy: Option<String>,
    #[serde(rename = "FramedVehicleJourneyRef")]
    framed_journey: Option<FramedJourney>,
    #[serde(rename = "VehicleJourneyRef")]
    journey_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Location {
    #[serde(rename = "Longitude")]
    lon: Option<String>,
    #[serde(rename = "Latitude")]
    lat: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FramedJourney {
    #[serde(rename = "DatedVehicleJourneyRef")]
    dated_ref: Option<String>,
}

/// What a decode produced, and what it refused.
pub struct Decoded {
    pub observations: Vec<Observation>,
    /// Records older than [`STALE`]: buses that have finished for the day
    /// and are still listed.
    pub stale: usize,
    /// Records with no usable position, time, operator or vehicle.
    pub malformed: usize,
}

/// Decode one SIRI-VM response.
pub fn decode(
    text: &str,
    source_id: &SourceId,
    now: DateTime<Utc>,
) -> Result<Decoded, SourceError> {
    let siri: Siri =
        quick_xml::de::from_str(text).map_err(|e| SourceError::Decode(format!("SIRI-VM: {e}")))?;
    let mut decoded = Decoded {
        observations: Vec::new(),
        stale: 0,
        malformed: 0,
    };
    for activity in siri
        .service_delivery
        .deliveries
        .into_iter()
        .flat_map(|d| d.activities)
    {
        match decode_activity(activity, source_id, now) {
            Row::Observed(o) => decoded.observations.push(*o),
            Row::Stale => decoded.stale += 1,
            Row::Malformed => decoded.malformed += 1,
        }
    }
    Ok(decoded)
}

enum Row {
    Observed(Box<Observation>),
    Stale,
    Malformed,
}

/// A text element with something in it.
fn text(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// A name as a person would write it: the feed's underscores are spaces.
fn name(s: &Option<String>) -> Option<String> {
    text(s).map(|s| s.replace('_', " "))
}

fn number(s: &Option<String>) -> Option<f64> {
    text(s).and_then(|s| s.parse().ok())
}

fn decode_activity(a: Activity, source_id: &SourceId, now: DateTime<Utc>) -> Row {
    let Some(j) = a.journey else {
        return Row::Malformed;
    };
    let (Some(operator), Some(vehicle)) = (text(&j.operator), text(&j.vehicle_ref)) else {
        return Row::Malformed;
    };
    let Some(recorded_at) = text(&a.recorded_at).and_then(|s| s.parse::<DateTime<Utc>>().ok())
    else {
        return Row::Malformed;
    };
    let (Some(lon), Some(lat)) = (
        j.location.as_ref().and_then(|l| number(&l.lon)),
        j.location.as_ref().and_then(|l| number(&l.lat)),
    ) else {
        return Row::Malformed;
    };
    if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
        return Row::Malformed;
    }
    if now - recorded_at > STALE {
        return Row::Stale;
    }

    // Operator and vehicle together. Vehicle refs are fleet numbers (`3300`)
    // or registrations, and the fleet numbers repeat across operators.
    let key = format!("{operator}:{vehicle}");

    // Bearing is the direction of travel, which for a bus is also the way
    // it is pointing. Given as both, so an icon rotates and dead reckoning
    // has a course, and clamped to a compass: the live feed never exceeded
    // 359 but a schema does not promise that.
    let bearing = number(&j.bearing).filter(|b| (0.0..=360.0).contains(b));

    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };
    put("operator", serde_json::json!(operator));
    put("vehicle", serde_json::json!(vehicle));
    put(
        "line",
        serde_json::json!(name(&j.line_name).or_else(|| name(&j.line_ref))),
    );
    put("direction", serde_json::json!(text(&j.direction)));
    put("origin", serde_json::json!(name(&j.origin)));
    put("destination", serde_json::json!(name(&j.destination)));
    put(
        "aimed_departure",
        serde_json::json!(text(&j.aimed_departure)),
    );
    put("aimed_arrival", serde_json::json!(text(&j.aimed_arrival)));
    put(
        "journey",
        serde_json::json!(
            j.framed_journey
                .as_ref()
                .and_then(|f| text(&f.dated_ref))
                .or_else(|| text(&j.journey_ref))
        ),
    );
    put("block", serde_json::json!(text(&j.block)));
    put("occupancy", serde_json::json!(text(&j.occupancy)));

    // The line is the label: it is what is written on the front of the bus
    // and what a person looking for one knows it by.
    let label = name(&j.line_name)
        .or_else(|| name(&j.line_ref))
        .unwrap_or_else(|| vehicle.to_string());

    Row::Observed(Box::new(
        Observation::new(
            source_id.clone(),
            EntityId::new(EntityKind::Vehicle, key),
            recorded_at,
            Quality::Live,
        )
        .with_position(Position {
            lon,
            lat,
            alt_m: None,
            datum: AltitudeDatum::AboveGround,
        })
        .with_kinematics(Kinematics {
            course_deg: bearing,
            heading_deg: bearing,
            ground_speed_mps: None,
            vertical_rate_mps: None,
        })
        .with_label(label)
        .with_attrs(serde_json::Value::Object(attrs)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn source() -> SourceId {
        SourceId::new("buses")
    }

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid instant")
    }

    /// 2026-09-16T10:32:30Z, half a minute after the response below.
    const NOW: i64 = 1_789_554_750;

    /// A trimmed live response: the envelope as the service sends it, one
    /// full record with underscored names and a bearing, one with the empty
    /// elements and no bearing that a fifth of the feed has, one a day old,
    /// one with no position.
    const RESPONSE: &str = r#"<Siri version="2.0" xmlns="http://www.siri.org.uk/siri" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.siri.org.uk/siri http://www.siri.org.uk/schema/2.0/xsd/siri.xsd"><ServiceDelivery><ResponseTimestamp>2026-09-16T10:32:03.103+00:00</ResponseTimestamp><ProducerRef>DepartmentForTransport</ProducerRef><VehicleMonitoringDelivery><ResponseTimestamp>2026-09-16T10:32:03.103+00:00</ResponseTimestamp><RequestMessageRef>879a1a80-4cd1-4f2f-8673-e3e180b02800</RequestMessageRef><ValidUntil>2026-09-16T10:37:03.103+00:00</ValidUntil><ShortestPossibleCycle>PT5S</ShortestPossibleCycle><VehicleActivity><RecordedAtTime>2026-09-16T10:31:30+00:00</RecordedAtTime><ItemIdentifier>38e14dab-09cd-4370-b49e-965f48ca2e7c</ItemIdentifier><ValidUntilTime>2026-09-16T10:37:03.103+00:00</ValidUntilTime><MonitoredVehicleJourney><LineRef>480</LineRef><DirectionRef>inbound</DirectionRef><FramedVehicleJourneyRef><DataFrameRef>2026-09-16</DataFrameRef><DatedVehicleJourneyRef>1079</DatedVehicleJourneyRef></FramedVehicleJourneyRef><PublishedLineName>480</PublishedLineName><OperatorRef>AMTM</OperatorRef><OriginRef>2400109565</OriginRef><OriginName>Barrack_Row</OriginName><DestinationRef>240096588</DestinationRef><DestinationName>Joyce_Green_Lane_Terminus</DestinationName><OriginAimedDepartureTime>2026-09-16T09:42:00+00:00</OriginAimedDepartureTime><DestinationAimedArrivalTime>2026-09-16T10:39:00+00:00</DestinationAimedArrivalTime><VehicleLocation><Longitude>0.218424</Longitude><Latitude>51.457531</Latitude></VehicleLocation><Bearing>234.0</Bearing><Occupancy>seatsAvailable</Occupancy><BlockRef>4801</BlockRef><VehicleRef>6412</VehicleRef></MonitoredVehicleJourney><Extensions><VehicleJourney><Operational><TicketMachine><TicketMachineServiceCode>480</TicketMachineServiceCode><JourneyCode>1079</JourneyCode></TicketMachine></Operational><VehicleUniqueId>6412</VehicleUniqueId></VehicleJourney></Extensions></VehicleActivity><VehicleActivity><RecordedAtTime>2026-09-16T10:31:58+00:00</RecordedAtTime><ItemIdentifier>b59cdaec-f306-49ac-8a94-9843535a8601</ItemIdentifier><ValidUntilTime>2026-09-16T10:37:03.103+00:00</ValidUntilTime><MonitoredVehicleJourney><LineRef></LineRef><DirectionRef>1</DirectionRef><PublishedLineName></PublishedLineName><OperatorRef>TFLO</OperatorRef><OriginName></OriginName><DestinationName></DestinationName><VehicleLocation><Longitude>-0.118303</Longitude><Latitude>51.509</Latitude></VehicleLocation><Bearing></Bearing><VehicleRef>LX61DDA</VehicleRef></MonitoredVehicleJourney></VehicleActivity><VehicleActivity><RecordedAtTime>2026-09-15T10:31:30+00:00</RecordedAtTime><ItemIdentifier>40175c34-0261-4bc7-91dd-9adab886a8ef</ItemIdentifier><ValidUntilTime>2026-09-16T10:37:03.103+00:00</ValidUntilTime><MonitoredVehicleJourney><LineRef>X1</LineRef><DirectionRef>outbound</DirectionRef><PublishedLineName>X1</PublishedLineName><OperatorRef>FECS</OperatorRef><VehicleLocation><Longitude>0.243975</Longitude><Latitude>52.395843</Latitude></VehicleLocation><Bearing>242.0</Bearing><VehicleRef>3303</VehicleRef></MonitoredVehicleJourney></VehicleActivity><VehicleActivity><RecordedAtTime>2026-09-16T10:31:00+00:00</RecordedAtTime><ItemIdentifier>c0ffee00-0000-0000-0000-000000000000</ItemIdentifier><MonitoredVehicleJourney><LineRef>7</LineRef><OperatorRef>SCEM</OperatorRef><VehicleLocation><Longitude></Longitude><Latitude></Latitude></VehicleLocation><VehicleRef>3303</VehicleRef></MonitoredVehicleJourney></VehicleActivity></VehicleMonitoringDelivery></ServiceDelivery></Siri>"#;

    #[test]
    fn the_live_shape_decodes_and_the_stale_and_unplaced_are_counted() {
        let d = decode(RESPONSE, &source(), at(NOW)).expect("the live envelope decodes");
        assert_eq!(d.observations.len(), 2, "two current, placed buses");
        assert_eq!(
            d.stale, 1,
            "the bus last heard from yesterday is in a depot"
        );
        assert_eq!(d.malformed, 1, "empty coordinates are not a position");
        for o in &d.observations {
            assert_eq!(o.entity.kind, EntityKind::Vehicle);
            assert_eq!(o.quality, Quality::Live);
        }
    }

    #[test]
    fn the_key_is_operator_and_vehicle_because_fleet_numbers_repeat() {
        // FECS 3303 and SCEM 3303 are both in the captured feed and are two
        // different buses a hundred miles apart.
        let d = decode(RESPONSE, &source(), at(NOW)).unwrap();
        assert_eq!(d.observations[0].entity.key, "AMTM:6412");
        assert_eq!(d.observations[1].entity.key, "TFLO:LX61DDA");
    }

    #[test]
    fn underscores_become_spaces_and_the_line_is_the_label() {
        let d = decode(RESPONSE, &source(), at(NOW)).unwrap();
        let o = &d.observations[0];
        assert_eq!(o.label.as_deref(), Some("480"));
        assert_eq!(
            o.attrs["destination"],
            serde_json::json!("Joyce Green Lane Terminus")
        );
        assert_eq!(o.attrs["origin"], serde_json::json!("Barrack Row"));
        assert_eq!(o.attrs["operator"], serde_json::json!("AMTM"));
        assert_eq!(o.attrs["journey"], serde_json::json!("1079"));
        assert_eq!(o.attrs["occupancy"], serde_json::json!("seatsAvailable"));
        assert_eq!(o.attrs["direction"], serde_json::json!("inbound"));
    }

    #[test]
    fn the_bearing_is_both_course_and_heading_and_an_empty_one_is_neither() {
        let d = decode(RESPONSE, &source(), at(NOW)).unwrap();
        let k = d.observations[0].kinematics.expect("kinematics");
        assert_eq!(k.course_deg, Some(234.0));
        assert_eq!(k.heading_deg, Some(234.0));
        // `<Bearing></Bearing>` — present, empty, and not zero.
        let k = d.observations[1].kinematics.expect("kinematics");
        assert_eq!(k.course_deg, None);
        assert_eq!(k.heading_deg, None);
    }

    #[test]
    fn empty_elements_are_absent_attributes_not_empty_strings() {
        // TfL sends `<LineRef></LineRef>` and `<DestinationName></DestinationName>`
        // on a bus between journeys. A card should show nothing, not "".
        let d = decode(RESPONSE, &source(), at(NOW)).unwrap();
        let o = &d.observations[1];
        assert!(o.attrs.get("line").is_none());
        assert!(o.attrs.get("destination").is_none());
        assert!(o.attrs.get("origin").is_none());
        // With no line, the vehicle is the label rather than nothing.
        assert_eq!(o.label.as_deref(), Some("LX61DDA"));
    }

    #[test]
    fn the_observation_time_is_the_bus_report_not_the_fetch() {
        let d = decode(RESPONSE, &source(), at(NOW)).unwrap();
        assert_eq!(
            d.observations[0].observed_at,
            at(1_789_554_690),
            "10:31:30Z"
        );
    }

    #[test]
    fn a_record_just_inside_the_horizon_is_kept_and_just_outside_is_not() {
        let fresh = decode(RESPONSE, &source(), at(1_789_554_690 + 599)).unwrap();
        assert_eq!(fresh.observations.len(), 2);
        let late = decode(RESPONSE, &source(), at(1_789_554_690 + 601)).unwrap();
        assert!(
            late.observations
                .iter()
                .all(|o| o.entity.key != "AMTM:6412")
        );
        assert_eq!(late.stale, 2);
    }

    #[test]
    fn the_requested_box_is_clipped_to_england_and_a_disjoint_one_asks_nothing() {
        let b = clip(None).expect("the global box overlaps England");
        assert_eq!((b.west, b.south, b.east, b.north), (-6.5, 49.8, 2.0, 55.9));

        let home = BoundingBox::new(-2.5, 51.0, 0.5, 52.5);
        let b = clip(Some(home)).expect("home overlaps");
        assert_eq!((b.west, b.south, b.east, b.north), (-2.5, 51.0, 0.5, 52.5));

        let atlantic = BoundingBox::new(-40.0, 30.0, -20.0, 40.0);
        assert!(
            clip(Some(atlantic)).is_none(),
            "nothing to ask about the mid-Atlantic"
        );
    }

    #[test]
    fn the_url_carries_the_key_and_the_box_in_the_service_order() {
        let http = HttpClient::new(std::time::Duration::from_secs(5)).unwrap();
        let src = Buses::new(http).with_api_key(Some("k3y".into()));
        let url = src.url(&BoundingBox::new(-2.5, 51.0, 0.5, 52.5)).unwrap();
        assert!(
            url.ends_with("?api_key=k3y&boundingBox=-2.5000,51.0000,0.5000,52.5000"),
            "{url}"
        );
    }

    #[test]
    fn a_blank_key_is_no_key() {
        // Matches the CredentialResolver's rule: an empty string in config is
        // "not set", and must not produce a request the service will 401.
        let http = HttpClient::new(std::time::Duration::from_secs(5)).unwrap();
        let src = Buses::new(http).with_api_key(Some("  ".into()));
        assert!(matches!(src.url(&EXTENT), Err(SourceError::Auth(_))));
    }
}
