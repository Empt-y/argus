//! Poll the Bus Open Data Service for real and assert on what comes back.
//!
//! The unit tests hold the decoder to a captured envelope and four captured
//! records. What they cannot see: the service changing its namespace or
//! nesting (which decodes to zero vehicles, not an error), the bounding box
//! starting to thin the way AWC's does, or the key being revoked.
//!
//! Network-gated and keyed: set ARGUS_NETWORK_TESTS=1 and BODS_API_KEY.

use argus_core::BoundingBox;
use argus_core::entity::EntityKind;
use argus_core::source::{Source, SourceError};

/// 28,088 vehicles were reporting across England at 10:35 on a Tuesday,
/// 20,819 of them within ten minutes. At 03:00 on a Sunday the honest number
/// is a few hundred, so the floor is that rather than half the weekday
/// figure; the point is to catch zero, which is what a schema change looks
/// like, and a box that returns a fraction of its halves.
const VEHICLES_FLOOR: usize = 200;

fn skip_or_panic(what: &str, err: SourceError) {
    match err {
        SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_) => {
            eprintln!("SKIPPING {what}: unavailable ({err})");
        }
        other => panic!("{what} failed: {other}"),
    }
}

async fn poll(
    source: &argus_ingest::sources::Buses,
    bbox: BoundingBox,
) -> Option<Vec<argus_core::Observation>> {
    let ctx = argus_core::PollCtx {
        bbox: Some(bbox),
        ..Default::default()
    };
    match source.poll(&ctx).await {
        Ok(o) => Some(o),
        Err(err) => {
            skip_or_panic("buses", err);
            None
        }
    }
}

#[tokio::test]
async fn the_fleet_arrives_whole_and_current() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let Ok(key) = std::env::var("BODS_API_KEY") else {
        eprintln!("SKIPPING: set BODS_API_KEY to poll the live feed");
        return;
    };
    // The daemon gives this source a 60-second, 256 MiB client; the same
    // here, so the test exercises the configuration the driver runs under.
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(60))
        .expect("an http client")
        .with_max_bytes(256 << 20);
    let source = argus_ingest::sources::Buses::new(http).with_api_key(Some(key));

    let england = BoundingBox::new(-6.5, 49.8, 2.0, 55.9);
    let Some(observations) = poll(&source, england).await else {
        return;
    };
    assert!(
        observations.len() >= VEHICLES_FLOOR,
        "{} buses reporting, expected at least {VEHICLES_FLOOR} — zero is what a schema \
         change looks like",
        observations.len()
    );

    let now = chrono::Utc::now();
    let mut with_line = 0usize;
    let mut with_bearing = 0usize;
    let mut operators = std::collections::HashSet::new();
    let mut keys = std::collections::HashSet::new();
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Vehicle);
        assert!(
            keys.insert(o.entity.key.clone()),
            "{} appeared twice",
            o.entity.key
        );
        let p = o.position.expect("a bus without a position was emitted");
        assert!(
            (-8.0..=2.5).contains(&p.lon) && (49.0..=61.0).contains(&p.lat),
            "{} is at {:.3},{:.3}, not in Great Britain",
            o.entity.key,
            p.lon,
            p.lat
        );
        // The driver's own horizon: nothing older than ten minutes, and the
        // stamp is UTC — read as local on a BST machine, every bus would be
        // an hour in the future.
        let age = now - o.observed_at;
        assert!(
            age <= chrono::Duration::minutes(11) && age >= chrono::Duration::minutes(-2),
            "{} is {} seconds old",
            o.entity.key,
            age.num_seconds()
        );
        let attrs = o.attrs.as_object().expect("attrs are an object");
        for key in ["origin", "destination", "line"] {
            if let Some(v) = attrs.get(key).and_then(|v| v.as_str()) {
                assert!(
                    !v.contains('_'),
                    "{} has an underscored {key}: {v}",
                    o.entity.key
                );
                assert!(!v.is_empty(), "{} has an empty {key}", o.entity.key);
            }
        }
        if attrs.contains_key("line") {
            with_line += 1;
        }
        if o.kinematics.is_some_and(|k| k.heading_deg.is_some()) {
            with_bearing += 1;
        }
        operators.insert(attrs["operator"].as_str().unwrap().to_string());
    }
    // 28,087 of 28,093 had a line and 23,202 a bearing. Both collapsing to
    // zero is what a renamed element looks like.
    assert!(
        with_line * 10 >= observations.len() * 9,
        "only {with_line} of {} have a line",
        observations.len()
    );
    assert!(
        with_bearing * 2 >= observations.len(),
        "only {with_bearing} of {} have a bearing",
        observations.len()
    );
    assert!(
        operators.len() >= 50,
        "only {} operators; 375 were reporting",
        operators.len()
    );

    // The bounding box must not thin. The two halves of England summed to
    // within two buses of the whole when this was written.
    let south = BoundingBox::new(-6.5, 49.8, 2.0, 52.8);
    let north = BoundingBox::new(-6.5, 52.8, 2.0, 55.9);
    let (Some(s), Some(n)) = (poll(&source, south).await, poll(&source, north).await) else {
        return;
    };
    let halves = s.len() + n.len();
    assert!(
        halves * 10 <= observations.len() * 11 && observations.len() * 10 <= halves * 11,
        "the whole box returned {} buses and its halves {halves}; a bbox that thins by area \
         is the METAR trap again",
        observations.len()
    );

    eprintln!(
        "bods: {} buses within 10 min, {} operators, {with_line} with a line, {with_bearing} with a bearing; halves {halves}",
        observations.len(),
        operators.len()
    );
}
