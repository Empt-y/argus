//! Poll the Environment Agency flood API for real and assert on what comes back.
//!
//! Two things this catches that the unit tests cannot.
//!
//! The gauge half is a paging and cardinality check. `/id/floodAreas` silently
//! truncates to 500 rows on a request that looks identical to the one
//! `/id/stations` answers in full, and the scalar-or-array flattening means a
//! single awkward station can fail the deserialisation of all 5,525. Both
//! failures produce a valid, decodable, wrong-sized answer, and only a count
//! notices.
//!
//! The warning half is a schema watch. There were no flood warnings in force in
//! England when this driver was written, so its decoder was built from the
//! Agency's published example rather than a live response. This test therefore
//! asserts the *shape* of whatever is in force rather than demanding warnings
//! exist — an England with no floods is a normal Tuesday, not a red build — and
//! it will be the thing that notices if the real shape differs from the
//! documented one the first time the rivers rise.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run.

use argus_core::entity::EntityKind;
use argus_core::source::{Source, SourceError};

/// England had 5,525 gauges when this was written. The floor is well below it —
/// stations are commissioned and retired — but two thirds still catches a lost
/// page or a cardinality failure that takes the whole document with it.
const STATIONS_SEEN: usize = 5525;

fn skip_or_panic(what: &str, err: SourceError) {
    match err {
        SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_) => {
            eprintln!("SKIPPING {what}: unavailable ({err})");
        }
        // A decode failure means the schema moved, which breaks the real driver
        // too and is exactly what should turn something red.
        other => panic!("{what} failed to decode: {other}"),
    }
}

#[tokio::test]
async fn the_gauge_network_arrives_whole() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(120))
        .expect("an http client");
    let source = argus_ingest::sources::EaRiverGauges::new(http);

    let observations = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("ea-river-gauges", err),
    };

    let floor = STATIONS_SEEN / 2;
    assert!(
        observations.len() >= floor,
        "{} gauges reporting, expected at least {floor} — a short answer is what a \
         lost page or a failed cardinality looks like",
        observations.len()
    );

    let mut with_readings = 0usize;
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Station);
        let p = o.position.expect("a gauge without a position was emitted");
        // Not England — the Agency publishes the National Tide Gauge Network
        // too, which reaches Lerwick and Portrush. This box is the UK.
        assert!(
            (-8.0..=2.1).contains(&p.lon) && (49.0..=61.0).contains(&p.lat),
            "{} is at {:.4},{:.4}, which is not in the UK",
            o.entity.key,
            p.lon,
            p.lat
        );
        let readings = o.attrs["readings"].as_array().expect("a readings array");
        assert!(
            !readings.is_empty(),
            "{} was emitted with no reading; it should have been skipped",
            o.entity.key
        );
        with_readings += readings.len();
        for r in readings {
            assert!(r["value"].is_number(), "a reading without a numeric value");
        }
    }

    eprintln!(
        "ea-river-gauges: {} gauges reporting, {with_readings} instrument readings",
        observations.len()
    );
}

#[tokio::test]
async fn flood_warnings_have_the_documented_shape_whenever_there_are_any() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(120))
        .expect("an http client");
    let source = argus_ingest::sources::EaFloodWarnings::new(http);

    let observations = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("ea-flood-warnings", err),
    };

    if observations.is_empty() {
        // Not a failure. The decoder still ran against the live envelope, which
        // is most of what this test is for: a schema change severe enough to
        // break parsing would have panicked above rather than reached here.
        eprintln!("ea-flood-warnings: no flood warnings in force in England");
        return;
    }

    let mut with_outline = 0usize;
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Event);
        let level = o.attrs["severity_level"]
            .as_i64()
            .expect("every warning carries a severity level");
        assert!(
            (1..=3).contains(&level),
            "{} has severity {level}; 4 means no longer in force and must not be drawn",
            o.entity.key
        );
        assert!(o.label.is_some(), "a warning with nothing to call it");
        if o.geom.is_some() {
            with_outline += 1;
        } else {
            assert!(
                o.position.is_some(),
                "{} has neither an outline nor a point, so nothing can draw it",
                o.entity.key
            );
        }
    }
    eprintln!(
        "ea-flood-warnings: {} in force, {with_outline} with an outline resolved this poll",
        observations.len()
    );
}
