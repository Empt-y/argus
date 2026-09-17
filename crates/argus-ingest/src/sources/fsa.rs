//! Every food business the UK's local authorities have inspected, with
//! its hygiene rating, from the Food Standards Agency.
//!
//! The FSA publishes the Food Hygiene Rating Scheme (England, Wales and
//! Northern Ireland: a 0–5 rating) and the Food Hygiene Information Scheme
//! (Scotland: pass or improvement required) as one XML file per local
//! authority, 363 of them, refreshed nightly from each authority's own
//! extract. 613,379 establishments in 575 MB when this was written —
//! restaurants, takeaways, pubs, school kitchens, care homes, supermarkets,
//! mobile caterers. The list of files comes from the ratings API, which
//! answers only with `x-api-version: 2` on the request, and each file URL
//! 307-redirects to its real path.
//!
//! An [`EntityKind::Feature`]: a business does not move, and a rating
//! changes a few times a year at most, so the store versions each one and
//! a weekly re-read writes only what changed. That is also why nothing
//! that changes every night goes in the attributes — the per-authority
//! extract date would re-version every establishment in the authority
//! every week for no reason. It is logged instead.
//!
//! What the whole register says, counted before any of this was designed:
//! a quarter of establishments (157,539) have no geocode and cannot be
//! placed, most of them in a handful of authorities (Birmingham 3,774,
//! North Yorkshire 3,224); `RatingValue` spells "Awaiting Inspection" two
//! ways and Welsh authorities carry `cy-gb` keys in their English files,
//! so the rating is read from `RatingKey`, which has one vocabulary;
//! 70,980 rating dates are empty (the exempt and the uninspected); and 79
//! records carry a `RightToReply` of double-escaped HTML, which is not
//! stored. The scores are points *lost* — 0 is the best on each — on
//! three scales: hygiene and structural out of 25, confidence in
//! management out of 30.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::geo::BoundingBox;
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::time::Duration;

const AUTHORITIES_URL: &str = "https://api.ratings.food.gov.uk/Authorities";
const API_HEADERS: &[(&str, &str)] = &[("x-api-version", "2"), ("Accept", "application/json")];

const CADENCE_SECS: u64 = 7 * 24 * 3600;
/// Between files. 363 of them, mostly under a megabyte; nothing about the
/// endpoint asked for this, but a weekly crawl has no reason to hurry.
const BETWEEN_FILES: Duration = Duration::from_millis(250);
/// Birmingham's file is 10 MB; the client's default cap is 32.
const MAX_FILE_BYTES: usize = 48 * 1024 * 1024;

/// The scheme covers the UK; the same box the carbon-intensity layer uses.
const UNITED_KINGDOM: BoundingBox = BoundingBox {
    west: -8.7,
    south: 49.8,
    east: 1.8,
    north: 60.9,
};

pub struct FoodHygiene {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl FoodHygiene {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("fsa-hygiene"),
                layer_id: LayerId::new("food-hygiene"),
                display_name: "Food hygiene ratings (FSA)".into(),
                kind: EntityKind::Feature,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Fixed { bbox: UNITED_KINGDOM },
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "Food Standards Agency".into(),
                    url: "https://ratings.food.gov.uk/open-data".into(),
                    license: "Open Government Licence v3.0".into(),
                    notice: Some("Contains Food Standards Agency data © Crown copyright and database right, Open Government Licence v3.0".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http: http.with_max_bytes(MAX_FILE_BYTES),
        }
    }
}

#[async_trait::async_trait]
impl Source for FoodHygiene {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let list = self.http.get_bytes_with(AUTHORITIES_URL, API_HEADERS).await?;
        let authorities = decode_authorities(&list)?;
        let now = Utc::now();
        let mut observations = Vec::with_capacity(460_000);
        let mut unplaced = 0;
        let mut failed = Vec::new();
        let mut oldest_extract: Option<String> = None;
        for authority in &authorities {
            let Some(url) = authority.file_name.as_deref().filter(|u| !u.is_empty()) else {
                failed.push(authority.name.clone());
                continue;
            };
            let bytes = match self.http.get_bytes(url).await {
                Ok(b) => b,
                Err(err) => {
                    // The establishments this authority published last time
                    // stay current in the store; a file that did not arrive
                    // is a week's staleness for one council, not a gap.
                    tracing::warn!(source = %self.descriptor.id, authority = %authority.name, %err, "file failed; its establishments keep their last version");
                    failed.push(authority.name.clone());
                    continue;
                }
            };
            match decode_file(&bytes, &self.descriptor.id, now) {
                Ok(decoded) => {
                    if oldest_extract.as_deref().is_none_or(|o| decoded.extract_date.as_str() < o) {
                        oldest_extract = Some(decoded.extract_date.clone());
                    }
                    unplaced += decoded.unplaced;
                    observations.extend(decoded.observations);
                }
                Err(err) => {
                    tracing::warn!(source = %self.descriptor.id, authority = %authority.name, %err, "file did not decode; its establishments keep their last version");
                    failed.push(authority.name.clone());
                }
            }
            tokio::time::sleep(BETWEEN_FILES).await;
        }
        if observations.is_empty() {
            return Err(SourceError::Decode(format!("no establishment could be placed from {} authorities", authorities.len())));
        }
        tracing::info!(
            source = %self.descriptor.id,
            authorities = authorities.len() - failed.len(),
            failed = failed.len(),
            placed = observations.len(),
            unplaced,
            oldest_extract = oldest_extract.as_deref().unwrap_or("?"),
            "food hygiene register read"
        );
        if !failed.is_empty() {
            tracing::warn!(source = %self.descriptor.id, "authorities not read this poll: {}", failed.join(", "));
        }
        Ok(observations)
    }
}

// --- wire format: the authorities list ------------------------------------------

#[derive(Debug, Deserialize)]
struct AuthoritiesResponse {
    authorities: Vec<Authority>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Authority {
    pub name: String,
    pub file_name: Option<String>,
}

pub fn decode_authorities(bytes: &[u8]) -> Result<Vec<Authority>, SourceError> {
    let response: AuthoritiesResponse =
        serde_json::from_slice(bytes).map_err(|e| SourceError::Decode(format!("authorities list: {e}")))?;
    if response.authorities.is_empty() {
        return Err(SourceError::Decode("the authorities list was empty".into()));
    }
    Ok(response.authorities)
}

// --- wire format: one authority's file ------------------------------------------

#[derive(Debug, Deserialize)]
struct File {
    #[serde(rename = "Header")]
    header: Header,
    #[serde(rename = "EstablishmentCollection", default)]
    collection: Collection,
}

#[derive(Debug, Deserialize)]
struct Header {
    #[serde(rename = "ExtractDate", default)]
    extract_date: String,
    #[serde(rename = "ItemCount", default)]
    item_count: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct Collection {
    #[serde(rename = "EstablishmentDetail", default)]
    establishments: Vec<Establishment>,
}

/// One `<EstablishmentDetail>`. Every field is optional on the wire — an
/// empty element deserialises as an empty string — and `RightToReply` is
/// deliberately not here.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Establishment {
    #[serde(rename = "FHRSID", default)]
    fhrsid: String,
    #[serde(default)]
    business_name: String,
    #[serde(default)]
    business_type: String,
    #[serde(default)]
    address_line1: String,
    #[serde(default)]
    address_line2: String,
    #[serde(default)]
    address_line3: String,
    #[serde(default)]
    address_line4: String,
    #[serde(default)]
    post_code: String,
    #[serde(default)]
    rating_value: String,
    #[serde(default)]
    rating_key: String,
    #[serde(default)]
    rating_date: String,
    #[serde(default)]
    local_authority_code: String,
    #[serde(default)]
    local_authority_name: String,
    #[serde(default)]
    local_authority_web_site: String,
    #[serde(default)]
    scores: Option<Scores>,
    #[serde(default)]
    scheme_type: String,
    #[serde(default)]
    new_rating_pending: String,
    #[serde(default)]
    geocode: Option<Geocode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Scores {
    #[serde(default)]
    hygiene: String,
    #[serde(default)]
    structural: String,
    #[serde(default)]
    confidence_in_management: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Geocode {
    #[serde(default)]
    longitude: String,
    #[serde(default)]
    latitude: String,
}

#[derive(Debug)]
pub struct Decoded {
    pub observations: Vec<Observation>,
    pub extract_date: String,
    pub unplaced: usize,
}

/// A rating as one vocabulary, from the key rather than the display value.
///
/// Keys are `fhrs_5_en-GB`, `fhrs_awaitinginspection_en-GB`,
/// `fhis_improvement_required_en-GB`, `fhrs_ratingawaited_cy-gb` and so on:
/// scheme, value, language. Numeric for the 0–5 scale, a fixed word for the
/// rest; a key nobody has seen is kept as its middle so the card shows
/// something rather than nothing.
pub fn rating_from_key(key: &str) -> Option<serde_json::Value> {
    let key = key.trim().to_ascii_lowercase();
    let body = key.strip_prefix("fhrs_").or_else(|| key.strip_prefix("fhis_"))?;
    let value = body.rsplit_once('_').map_or(body, |(v, _lang)| v);
    if value.is_empty() {
        return None;
    }
    if let Ok(n) = value.parse::<u8>() {
        return (n <= 5).then(|| serde_json::json!(n));
    }
    let word = match value {
        "awaitinginspection" | "awaiting_inspection" | "ratingawaited" => "awaiting_inspection",
        "awaitingpublication" | "awaiting_publication" => "awaiting_publication",
        "exempt" => "exempt",
        "pass" => "pass",
        "pass_and_eat_safe" => "pass_and_eat_safe",
        "improvement_required" => "improvement_required",
        other => other,
    };
    Some(serde_json::json!(word))
}

pub fn decode_file(bytes: &[u8], source_id: &SourceId, now: DateTime<Utc>) -> Result<Decoded, SourceError> {
    let text = std::str::from_utf8(bytes).map_err(|e| SourceError::Decode(format!("not UTF-8: {e}")))?;
    let file: File = quick_xml::de::from_str(text).map_err(|e| SourceError::Decode(format!("FHRS xml: {e}")))?;
    let establishments = file.collection.establishments;
    if let Some(expected) = file.header.item_count
        && expected != establishments.len()
    {
        // A truncated file would decode cleanly and look like a small
        // council; the header's own count is the check.
        return Err(SourceError::Decode(format!("header says {expected} establishments, file has {}", establishments.len())));
    }
    let mut observations = Vec::with_capacity(establishments.len());
    let mut unplaced = 0;
    for e in establishments {
        let fhrsid = e.fhrsid.trim();
        if fhrsid.is_empty() {
            continue;
        }
        let position = e.geocode.as_ref().and_then(|g| {
            let lon = g.longitude.trim().parse::<f64>().ok()?;
            let lat = g.latitude.trim().parse::<f64>().ok()?;
            ((-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat) && !(lon == 0.0 && lat == 0.0)).then_some((lon, lat))
        });
        let Some((lon, lat)) = position else {
            unplaced += 1;
            continue;
        };
        let nonempty = |s: &str| {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        };
        let points = |s: &str| s.trim().parse::<u8>().ok();

        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        let name = nonempty(&e.business_name).unwrap_or_else(|| format!("FHRS {fhrsid}"));
        put("name", serde_json::json!(name));
        put("business_type", serde_json::json!(nonempty(&e.business_type)));
        put("scheme", serde_json::json!(nonempty(&e.scheme_type)));
        put("rating", rating_from_key(&e.rating_key).unwrap_or_else(|| serde_json::json!(nonempty(&e.rating_value))));
        put("rating_date", serde_json::json!(nonempty(&e.rating_date)));
        put("new_rating_pending", serde_json::json!(if e.new_rating_pending.trim().eq_ignore_ascii_case("true") { Some(true) } else { None }));
        if let Some(s) = &e.scores {
            put("hygiene_points", serde_json::json!(points(&s.hygiene)));
            put("structural_points", serde_json::json!(points(&s.structural)));
            put("management_points", serde_json::json!(points(&s.confidence_in_management)));
        }
        let address: Vec<String> = [&e.address_line1, &e.address_line2, &e.address_line3, &e.address_line4].into_iter().filter_map(|l| nonempty(l)).collect();
        put("address", serde_json::json!(if address.is_empty() { None } else { Some(address.join(", ")) }));
        put("postcode", serde_json::json!(nonempty(&e.post_code)));
        put("authority", serde_json::json!(nonempty(&e.local_authority_name)));
        put("authority_code", serde_json::json!(nonempty(&e.local_authority_code)));
        put("authority_url", serde_json::json!(nonempty(&e.local_authority_web_site).filter(|u| u.starts_with("http"))));
        put("fhrsid", serde_json::json!(fhrsid));
        put("url", serde_json::json!(format!("https://ratings.food.gov.uk/business/{fhrsid}")));

        observations.push(
            Observation::new(source_id.clone(), EntityId::new(EntityKind::Feature, format!("fhrs:{fhrsid}")), now, Quality::Live)
                .with_position(Position { lon, lat, alt_m: None, datum: AltitudeDatum::Geoid })
                .with_label(name)
                .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    Ok(Decoded { observations, extract_date: file.header.extract_date, unplaced })
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTHORITIES: &str = r#"{"authorities":[{"LocalAuthorityId":197,"LocalAuthorityIdCode":"760","Name":"Aberdeen City","FriendlyName":"aberdeen-city","Url":"http://www.aberdeencity.gov.uk","RegionName":"Scotland","FileName":"https://ratings.food.gov.uk/OpenDataFiles/FHRS760en-GB.xml","FileNameWelsh":null,"EstablishmentCount":2207,"LastPublishedDate":"2026-09-09T00:33:59.783","SchemeType":2},{"LocalAuthorityId":1,"LocalAuthorityIdCode":"402","Name":"Birmingham","FileName":"https://ratings.food.gov.uk/OpenDataFiles/FHRS402en-GB.xml","SchemeType":1}]}"#;

    const FILE: &str = r#"<?xml version="1.0"?><FHRSEstablishment xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><Header><ExtractDate>2026-09-08</ExtractDate><ItemCount>4</ItemCount><ReturnCode>Success</ReturnCode></Header><EstablishmentCollection><EstablishmentDetail><FHRSID>1608170</FHRSID><LocalAuthorityBusinessID>EHDC15239</LocalAuthorityBusinessID><BusinessName>(CURATED) MOROCCAN MARKET</BusinessName><BusinessType>Retailers - other</BusinessType><BusinessTypeID>4613</BusinessTypeID><AddressLine2>George Street</AddressLine2><AddressLine3>Aberdeen</AddressLine3><PostCode>AB25 1HZ</PostCode><RatingValue>Improvement Required</RatingValue><RatingKey>fhis_improvement_required_en-GB</RatingKey><RatingDate>2023-07-28</RatingDate><LocalAuthorityCode>760</LocalAuthorityCode><LocalAuthorityName>Aberdeen City</LocalAuthorityName><LocalAuthorityWebSite>http://www.aberdeencity.gov.uk</LocalAuthorityWebSite><LocalAuthorityEmailAddress>commercial@aberdeencity.gov.uk</LocalAuthorityEmailAddress><Scores /><SchemeType>FHIS</SchemeType><NewRatingPending>False</NewRatingPending><Geocode><Longitude>-2.10076438</Longitude><Latitude>57.14955652</Latitude></Geocode></EstablishmentDetail><EstablishmentDetail><FHRSID>982849</FHRSID><LocalAuthorityBusinessID>X</LocalAuthorityBusinessID><BusinessName>1 &amp; 30 DONALD DEWAR COURT</BusinessName><BusinessType>Hospitals/Childcare/Caring Premises</BusinessType><BusinessTypeID>5</BusinessTypeID><PostCode>AB16 5JB</PostCode><RatingValue>Awaiting Inspection</RatingValue><RatingKey>fhis_awaiting_inspection_en-GB</RatingKey><RatingDate /><LocalAuthorityCode>760</LocalAuthorityCode><LocalAuthorityName>Aberdeen City</LocalAuthorityName><LocalAuthorityWebSite>http://www.aberdeencity.gov.uk</LocalAuthorityWebSite><Scores /><SchemeType>FHIS</SchemeType><NewRatingPending>False</NewRatingPending><Geocode><Longitude /><Latitude /></Geocode></EstablishmentDetail><EstablishmentDetail><FHRSID>55</FHRSID><BusinessName>THE CROWN</BusinessName><BusinessType>Pub/bar/nightclub</BusinessType><BusinessTypeID>7843</BusinessTypeID><AddressLine1>The Crown</AddressLine1><AddressLine2>1 High Street</AddressLine2><AddressLine4>Birmingham</AddressLine4><PostCode>B1 1AA</PostCode><RatingValue>3</RatingValue><RatingKey>fhrs_3_en-GB</RatingKey><RatingDate>2026-03-12</RatingDate><LocalAuthorityCode>402</LocalAuthorityCode><LocalAuthorityName>Birmingham</LocalAuthorityName><LocalAuthorityWebSite>http://www.birmingham.gov.uk</LocalAuthorityWebSite><Scores><Hygiene>10</Hygiene><Structural>5</Structural><ConfidenceInManagement>10</ConfidenceInManagement></Scores><SchemeType>FHRS</SchemeType><NewRatingPending>True</NewRatingPending><RightToReply>&amp;lt;p&amp;gt;We have fixed it&amp;lt;/p&amp;gt;</RightToReply><Geocode><Longitude>-1.9</Longitude><Latitude>52.48</Latitude></Geocode></EstablishmentDetail><EstablishmentDetail><FHRSID>56</FHRSID><BusinessName>CAFFI CYMRU</BusinessName><BusinessType>Restaurant/Cafe/Canteen</BusinessType><BusinessTypeID>1</BusinessTypeID><RatingValue>Awaiting Inspection</RatingValue><RatingKey>fhrs_ratingawaited_cy-gb</RatingKey><RatingDate /><LocalAuthorityCode>500</LocalAuthorityCode><LocalAuthorityName>Gwynedd</LocalAuthorityName><LocalAuthorityWebSite /><Scores /><SchemeType>FHRS</SchemeType><NewRatingPending>False</NewRatingPending><Geocode><Longitude>-4.1</Longitude><Latitude>53.1</Latitude></Geocode></EstablishmentDetail></EstablishmentCollection></FHRSEstablishment>"#;

    #[test]
    fn the_authorities_list_names_each_file() {
        let a = decode_authorities(AUTHORITIES.as_bytes()).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].name, "Aberdeen City");
        assert_eq!(a[0].file_name.as_deref(), Some("https://ratings.food.gov.uk/OpenDataFiles/FHRS760en-GB.xml"));
        assert!(decode_authorities(br#"{"authorities":[]}"#).is_err());
    }

    #[test]
    fn a_file_yields_placed_establishments_with_one_rating_vocabulary_and_no_reply_html() {
        let d = decode_file(FILE.as_bytes(), &SourceId::new("fsa-hygiene"), Utc::now()).unwrap();
        assert_eq!(d.extract_date, "2026-09-08");
        assert_eq!(d.unplaced, 1, "an empty geocode is not placed");
        assert_eq!(d.observations.len(), 3);

        let market = &d.observations[0];
        assert_eq!(market.entity.key, "fhrs:1608170");
        assert_eq!(market.entity.kind, EntityKind::Feature);
        assert_eq!(market.label.as_deref(), Some("(CURATED) MOROCCAN MARKET"));
        assert_eq!(market.attrs["rating"], "improvement_required");
        assert_eq!(market.attrs["scheme"], "FHIS");
        assert_eq!(market.attrs["address"], "George Street, Aberdeen");
        assert!(market.attrs.get("hygiene_points").is_none(), "FHIS publishes no scores");
        assert!(market.attrs.get("new_rating_pending").is_none(), "false is left out");
        let p = market.position.unwrap();
        assert!((p.lon + 2.1008).abs() < 1e-3 && (p.lat - 57.1496).abs() < 1e-3);

        let crown = &d.observations[1];
        assert_eq!(crown.attrs["rating"], 3);
        assert_eq!(crown.attrs["rating_date"], "2026-03-12");
        assert_eq!(crown.attrs["hygiene_points"], 10);
        assert_eq!(crown.attrs["structural_points"], 5);
        assert_eq!(crown.attrs["management_points"], 10);
        assert_eq!(crown.attrs["new_rating_pending"], true);
        assert_eq!(crown.attrs["address"], "The Crown, 1 High Street, Birmingham");
        assert_eq!(crown.attrs["url"], "https://ratings.food.gov.uk/business/55");
        assert!(crown.attrs.get("right_to_reply").is_none() && !crown.attrs.to_string().contains("&lt;"), "{}", crown.attrs);

        let welsh = &d.observations[2];
        assert_eq!(welsh.attrs["rating"], "awaiting_inspection", "the Welsh key is the same state");
        assert!(welsh.attrs.get("rating_date").is_none());
        assert!(welsh.attrs.get("authority_url").is_none(), "an empty website is left out");
    }

    #[test]
    fn a_truncated_file_is_refused_by_its_own_header() {
        let truncated = FILE.replace("<ItemCount>4</ItemCount>", "<ItemCount>40</ItemCount>");
        let err = decode_file(truncated.as_bytes(), &SourceId::new("fsa-hygiene"), Utc::now()).unwrap_err();
        assert!(err.to_string().contains("header says 40"), "{err}");
    }

    #[test]
    fn every_rating_key_seen_in_the_register_maps_to_one_vocabulary() {
        // The 24 distinct keys across all 363 files when this was written.
        for (key, want) in [
            ("fhrs_5_en-GB", serde_json::json!(5)),
            ("fhrs_0_cy-gb", serde_json::json!(0)),
            ("fhrs_awaitinginspection_en-GB", serde_json::json!("awaiting_inspection")),
            ("fhis_awaiting_inspection_en-GB", serde_json::json!("awaiting_inspection")),
            ("fhrs_ratingawaited_cy-gb", serde_json::json!("awaiting_inspection")),
            ("fhrs_awaitingpublication_en-GB", serde_json::json!("awaiting_publication")),
            ("fhis_awaiting_publication_en-GB", serde_json::json!("awaiting_publication")),
            ("fhrs_exempt_cy-gb", serde_json::json!("exempt")),
            ("fhis_pass_en-GB", serde_json::json!("pass")),
            ("fhis_pass_and_eat_safe_en-GB", serde_json::json!("pass_and_eat_safe")),
            ("fhis_improvement_required_en-GB", serde_json::json!("improvement_required")),
        ] {
            assert_eq!(rating_from_key(key), Some(want), "{key}");
        }
        assert_eq!(rating_from_key(""), None);
        assert_eq!(rating_from_key("something_else"), None);
        assert_eq!(rating_from_key("fhrs_9_en-GB"), None, "off the scale");
    }
}
