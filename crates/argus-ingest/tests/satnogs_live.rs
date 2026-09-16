//! Poll SatNOGS for real: the station list and the schedule around now.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run.

use argus_core::entity::{EntityKind, Quality};
use argus_core::source::{Source, SourceError};

/// 4,470 stations, 4,263 of them placed, 305 online when this was written.
const PLACED_SEEN: usize = 4_263;

fn skip_or_panic(what: &str, err: SourceError) {
    match err {
        SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_) => {
            eprintln!("SKIPPING {what}: unavailable ({err})");
        }
        other => panic!("{what} failed: {other}"),
    }
}

#[tokio::test]
async fn the_network_arrives_with_its_dark_stations_marked_and_its_passes_attached() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http =
        argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let source = argus_ingest::sources::SatnogsStations::new(http);
    let observations = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("satnogs", err),
    };
    assert!(
        observations.len() >= PLACED_SEEN / 2,
        "{} stations",
        observations.len()
    );

    let mut live = 0usize;
    let mut listening = 0usize;
    let mut next = 0usize;
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Station);
        let p = o.position.expect("a station without a position");
        assert!(
            !(p.lat == 0.0 && p.lon == 0.0),
            "{} is on Null Island",
            o.entity.key
        );
        let status = o.attrs["status"].as_str().unwrap();
        // Quality follows status: a station the network calls online is
        // live and one it calls offline is stale, never the other way.
        match status {
            "Online" => {
                assert_eq!(o.quality, Quality::Live);
                live += 1;
            }
            "Offline" => assert_eq!(
                o.quality,
                Quality::Stale,
                "{} is offline but live",
                o.entity.key
            ),
            _ => {}
        }
        if let Some(l) = o.attrs.get("listening_to") {
            listening += 1;
            assert!(
                l["norad_id"].is_number(),
                "{} is listening to nothing in particular",
                o.entity.key
            );
        }
        if o.attrs.get("next").is_some() {
            next += 1;
        }
    }
    // 305 online; a schedule that decoded attaches passes to some of them.
    assert!(live >= 100, "only {live} stations online");
    assert!(
        listening + next >= 10,
        "only {listening} listening and {next} with a next pass; the schedule join has broken"
    );

    eprintln!(
        "satnogs: {} stations, {live} online, {listening} listening now, {next} with a next pass",
        observations.len()
    );
}
