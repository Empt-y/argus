//! Fetch EMODnet's platforms and wind farms for real.
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
async fn platforms_and_farms_arrive_in_european_waters_with_the_right_axis_order() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http =
        argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    for (source, floor, name) in [
        (
            argus_ingest::sources::Emodnet::platforms(http.clone()),
            800usize,
            "platforms",
        ),
        (
            argus_ingest::sources::Emodnet::wind_farms(http.clone()),
            300usize,
            "wind farms",
        ),
    ] {
        let observations = match source.poll(&argus_core::PollCtx::default()).await {
            Ok(o) => o,
            Err(err) => return skip_or_panic(name, err),
        };
        // 1,617 platforms and 600 farms when this was written.
        assert!(observations.len() >= floor, "{} {name}", observations.len());
        let mut keys = std::collections::HashSet::new();
        for o in &observations {
            assert_eq!(o.entity.kind, EntityKind::Feature);
            assert!(
                keys.insert(o.entity.key.clone()),
                "{} appeared twice",
                o.entity.key
            );
            let p = o.position.expect("a position");
            // Swapped axes would put the North Sea at 55°E 4°N, off Somalia.
            assert!(
                (-32.0..=45.0).contains(&p.lon) && (26.0..=82.0).contains(&p.lat),
                "{} at {:.2},{:.2}",
                o.entity.key,
                p.lon,
                p.lat
            );
            assert!(
                o.attrs.get("status").is_some(),
                "{} has no status",
                o.entity.key
            );
        }
        if name == "wind farms" {
            assert!(
                observations.iter().all(|o| o.geom.is_some()),
                "a wind farm without an outline"
            );
        }
        eprintln!("emodnet: {} {name}", observations.len());
    }
}
