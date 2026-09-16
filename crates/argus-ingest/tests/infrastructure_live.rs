//! Poll TeleGeography, Overpass and FIRMS for real.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run. The cable test makes
//! 700-odd requests at one a second, so a quarter of an hour; the FIRMS
//! test needs ARGUS_FIRMS_MAP_KEY and skips without it.

use argus_core::entity::{EntityKind, Quality};
use argus_core::source::{Source, SourceError};
use argus_core::BoundingBox;
use geo_types::Geometry;

fn skip_or_panic(what: &str, err: SourceError) {
    match err {
        SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_) => {
            eprintln!("SKIPPING {what}: unavailable ({err})");
        }
        other => panic!("{what} failed: {other}"),
    }
}

fn gated() -> bool {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return false;
    }
    true
}

#[tokio::test]
async fn hundreds_of_cables_arrive_as_lines_with_owners_and_landings_ashore() {
    if !gated() {
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let cables = match argus_ingest::sources::SubmarineCables::cables(http.clone()).poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("submarine-cables", err),
    };
    // 728 when written; half that is a lost file, not a smaller internet.
    assert!(cables.len() > 400, "{} cables", cables.len());
    let with_owners = cables.iter().filter(|c| c.attrs.get("owners").is_some()).count();
    assert!(with_owners * 10 > cables.len() * 9, "{with_owners} of {} cables have their record", cables.len());
    for c in &cables {
        assert_eq!(c.entity.kind, EntityKind::Feature);
        assert!(matches!(c.geom, Some(Geometry::LineString(_)) | Some(Geometry::MultiLineString(_))), "{} has no line", c.entity.key);
    }
    let landings = match argus_ingest::sources::SubmarineCables::landing_points(http).poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("cable-landings", err),
    };
    assert!(landings.len() > 1_000, "{} landing points", landings.len());
    let bude = landings.iter().find(|l| l.entity.key == "landing:bude-united-kingdom").expect("Bude");
    let at_bude = bude.attrs["cables"].as_array().expect("cables at Bude");
    assert!(at_bude.len() >= 5, "Bude lands {} cables; it had at least five", at_bude.len());
}

#[tokio::test]
async fn the_home_area_grid_has_its_400_kv_lines_and_no_street_kiosks() {
    if !gated() {
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(120)).expect("an http client");
    let source = argus_ingest::sources::PowerGrid::new(http);
    // One tile: Berkshire to the Chilterns, Bramley to Didcot.
    let ctx = argus_core::PollCtx {
        bbox: Some(BoundingBox::new(-2.0, 51.0, 0.0, 52.0)),
        ..Default::default()
    };
    let obs = match source.poll(&ctx).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("osm-power", err),
    };
    let lines = obs.iter().filter(|o| o.attrs["kind"] == "line").count();
    let subs = obs.iter().filter(|o| o.attrs["kind"] == "substation").count();
    let plants = obs.iter().filter(|o| o.attrs["kind"] == "plant").count();
    eprintln!("lines {lines} substations {subs} plants {plants}");
    assert!(lines > 500, "{lines} lines");
    assert!(obs.iter().any(|o| o.attrs["kind"] == "line" && o.attrs["voltage_kv"] == 400.0), "no 400 kV line in a tile with the Bramley ring");
    assert!(subs > 100 && subs < 5_000, "{subs} substations: under a hundred lost the typed ones, over five thousand kept the kiosks");
    assert!(plants > 50, "{plants} plants");
    for o in &obs {
        assert_eq!(o.entity.kind, EntityKind::Feature);
        assert!(o.entity.key.starts_with("osm:"));
    }
}

#[tokio::test]
async fn a_day_of_fires_is_tens_of_thousands_and_the_second_read_is_nothing_new() {
    if !gated() {
        return;
    }
    let Ok(key) = std::env::var("ARGUS_FIRMS_MAP_KEY") else {
        eprintln!("SKIPPING: set ARGUS_FIRMS_MAP_KEY to poll FIRMS");
        return;
    };
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let source = argus_ingest::sources::Fires::new(http).with_map_key(Some(key));
    let first = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("firms", err),
    };
    assert!(first.len() > 20_000, "{} detections in a day worldwide", first.len());
    let now = chrono::Utc::now();
    let mut sats = std::collections::BTreeSet::new();
    for o in &first {
        assert_eq!(o.entity.kind, EntityKind::Event);
        assert_eq!(o.quality, Quality::Live);
        assert!(o.observed_at <= now && now - o.observed_at < chrono::Duration::hours(36), "{} dated {}", o.entity.key, o.observed_at);
        sats.insert(o.attrs["satellite"].as_str().unwrap().to_string());
    }
    assert_eq!(sats.len(), 3, "three satellites: {sats:?}");
    let keys: std::collections::HashSet<_> = first.iter().map(|o| &o.entity.key).collect();
    assert_eq!(keys.len(), first.len(), "keys are unique");
    let second = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("firms", err),
    };
    assert!(second.len() < first.len() / 20, "{} new on an immediate re-read of {}", second.len(), first.len());
}
