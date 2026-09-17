//! Poll AuroraWatch UK for real.
//!
//! Asserts on the shape of the network — a placed alerting site with a full
//! day of hourly readings and a level the scheme knows — never on status
//! codes. Network-gated: set ARGUS_NETWORK_TESTS=1 to run.

use argus_core::entity::EntityKind;
use argus_core::source::{Source, SourceError};

#[tokio::test]
async fn the_alerting_magnetometer_arrives_with_a_day_of_readings_and_the_alert_level() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let obs = match argus_ingest::sources::AuroraWatch::new(http).poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err @ (SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_))) => {
            eprintln!("SKIPPING aurorawatch: unavailable ({err})");
            return;
        }
        Err(err) => panic!("aurorawatch failed: {err}"),
    };
    // Five sites published an activity document when written; one was current.
    assert!(!obs.is_empty());
    let alerting: Vec<_> = obs.iter().filter(|o| o.attrs["alerting"] == true).collect();
    assert_eq!(alerting.len(), 1, "exactly one site sets the alert: {:?}", obs.iter().map(|o| &o.entity.key).collect::<Vec<_>>());
    let site = alerting[0];
    assert_eq!(site.entity.kind, EntityKind::Station);
    let p = site.position.expect("placed");
    assert!((49.0..61.5).contains(&p.lat) && (-9.0..2.5).contains(&p.lon), "{p:?}");
    assert!(["green", "yellow", "amber", "red"].contains(&site.attrs["alert_level"].as_str().unwrap()), "{}", site.attrs["alert_level"]);
    assert_eq!(site.attrs["hours"].as_array().unwrap().len(), 24, "a day of hourly readings");
    let age = chrono::Utc::now() - site.observed_at;
    assert!(age < chrono::Duration::hours(3), "the alerting site is current, not {age}");
    assert!(site.attrs["activity_nt"].as_f64().unwrap() >= 0.0);
}
