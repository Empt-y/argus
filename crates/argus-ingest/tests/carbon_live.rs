//! Poll the Carbon Intensity API and the NESO licence-area file for real.
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
async fn fourteen_regions_arrive_on_their_boundaries_inside_great_britain() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http =
        argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let source = argus_ingest::sources::CarbonIntensity::new(http);
    let observations = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("carbon-intensity", err),
    };
    assert_eq!(observations.len(), 14, "one reading per DNO region");
    let now = chrono::Utc::now();
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Measure);
        assert!(
            o.observed_at <= now && now - o.observed_at < chrono::Duration::hours(2),
            "{} is dated {}",
            o.entity.key,
            o.observed_at
        );
        // The boundary file is in British National Grid. Untransformed, a
        // centroid lands at (400000, 300000) and this is what catches it.
        let p = o.position.expect("a position");
        assert!(
            (-8.0..=2.0).contains(&p.lon) && (49.5..=61.0).contains(&p.lat),
            "{} centroid at {:.2},{:.2} is not in Great Britain",
            o.entity.key,
            p.lon,
            p.lat
        );
        assert!(
            o.geom.is_some(),
            "{} has no boundary; the licence-area file or its id table has moved",
            o.entity.key
        );
        let v = o.attrs["intensity_gco2_kwh"].as_f64().unwrap();
        assert!(
            (0.0..=1000.0).contains(&v),
            "{} reads {v} gCO2/kWh",
            o.entity.key
        );
        let mix = o.attrs["generation_mix_pct"]
            .as_object()
            .expect("a generation mix");
        let total: f64 = mix.values().filter_map(|v| v.as_f64()).sum();
        assert!(
            (90.0..=110.0).contains(&total),
            "{}'s mix sums to {total}%",
            o.entity.key
        );
    }
    eprintln!(
        "carbon: {} regions, e.g. {}",
        observations.len(),
        observations[0].label.as_deref().unwrap_or("")
    );
}
