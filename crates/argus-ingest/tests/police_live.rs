//! Poll data.police.uk for real: a box in central London dense enough
//! that the API refuses it whole, so the tile split is exercised.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run. Takes a minute or
//! two: a dense tile answers in ten seconds and splits in four.

use argus_core::entity::{EntityKind, Quality};
use argus_core::source::{Source, SourceError};
use argus_core::BoundingBox;

fn skip_or_panic(what: &str, err: SourceError) {
    match err {
        SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_) => {
            eprintln!("SKIPPING {what}: unavailable ({err})");
        }
        other => panic!("{what} failed: {other}"),
    }
}

#[tokio::test]
async fn central_london_is_over_the_limit_and_arrives_anyway_by_splitting() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(120)).expect("an http client");
    let source = argus_ingest::sources::StreetCrime::new(http);
    // Islington to the City: 0.1° by 0.07°, some fifteen thousand records
    // a month, over the API's ten-thousand refusal.
    let bbox = BoundingBox::new(-0.15, 51.48, -0.05, 51.55);
    let ctx = argus_core::PollCtx {
        bbox: Some(bbox),
        ..Default::default()
    };
    let observations = match source.poll(&ctx).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("police-uk", err),
    };
    assert!(
        observations.len() > 10_000,
        "{} crimes: a whole answer for this box is over ten thousand, so fewer means a split was lost",
        observations.len()
    );
    let now = chrono::Utc::now();
    let mut months = std::collections::BTreeSet::new();
    let mut outside = 0;
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Event);
        assert_eq!(o.quality, Quality::Delayed);
        assert!(now - o.observed_at < chrono::Duration::minutes(30), "stamped at poll time");
        months.insert(o.attrs["month"].as_str().unwrap().to_string());
        let p = o.position.expect("a position");
        // The API answers for the polygon, so every point is inside it,
        // give or take the snapping.
        if !bbox.expanded(0.01).contains(p.lon, p.lat) {
            outside += 1;
        }
    }
    assert_eq!(months.len(), 1, "one month at a time: {months:?}");
    assert!(outside < observations.len() / 100, "{outside} points outside the box asked for");
    let keys: std::collections::HashSet<_> = observations.iter().map(|o| &o.entity.key).collect();
    assert_eq!(keys.len(), observations.len(), "the quarters of a split tile do not overlap");
}
