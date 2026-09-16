//! Poll TfL's road disruption feed for real.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run.

use argus_core::entity::EntityKind;
use argus_core::source::{Source, SourceError};

fn skip_or_panic(what: &str, err: SourceError) {
    match err {
        SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_) => {
            eprintln!("SKIPPING {what}: unavailable ({err})");
        }
        other => panic!("{what} failed: {other}"),
    }
}

#[tokio::test]
async fn londons_disruptions_arrive_current_and_inside_london() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http =
        argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let source = argus_ingest::sources::TflRoadDisruptions::new(http);
    let observations = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("tfl-road-disruptions", err),
    };
    // 131 in the pull that built this; a red-route network never has none.
    assert!(
        observations.len() >= 20,
        "{} disruptions",
        observations.len()
    );
    let now = chrono::Utc::now();
    let mut with_area = 0usize;
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Event);
        let p = o.position.expect("a disruption without a point");
        assert!(
            (-0.6..=0.4).contains(&p.lon) && (51.2..=51.8).contains(&p.lat),
            "{} is outside London at {:.3},{:.3}",
            o.entity.key,
            p.lon,
            p.lat
        );
        assert!(
            o.observed_at <= now + chrono::Duration::minutes(1),
            "{} is dated in the future",
            o.entity.key
        );
        let end = o.attrs["end"]
            .as_str()
            .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok())
            .expect("an end");
        assert!(
            end >= now,
            "{} ended {} and is still emitted",
            o.entity.key,
            end
        );
        if o.geom.is_some() {
            with_area += 1;
        }
    }
    assert!(
        with_area > 0,
        "no disruption carried an area; the geometry field has moved"
    );
    eprintln!(
        "tfl: {} disruptions, {with_area} with an area",
        observations.len()
    );
}
