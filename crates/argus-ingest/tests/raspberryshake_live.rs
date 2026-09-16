//! Poll the Raspberry Shake FDSN station service for real.
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
async fn thousands_of_shakes_arrive_with_a_model_each_and_hundreds_in_britain() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let source = argus_ingest::sources::RaspberryShake::new(http);
    let observations = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("raspberry-shake", err),
    };
    // 6,186 open station epochs when this was written. Half that is a
    // lost page or a changed filter, not a smaller network.
    assert!(observations.len() > 3_000, "{} stations", observations.len());
    let uk = observations
        .iter()
        .filter(|o| o.position.is_some_and(|p| (-11.0..=2.0).contains(&p.lon) && (49.5..=61.0).contains(&p.lat)))
        .count();
    assert!(uk > 150, "{uk} stations in the British Isles; there were 364");
    let mut models = std::collections::BTreeMap::new();
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Station);
        assert!(o.entity.key.starts_with("AM."));
        *models.entry(o.attrs["model"].as_str().unwrap().to_string()).or_insert(0usize) += 1;
    }
    eprintln!("models: {models:?}");
    assert!(models.get("Raspberry Shake 1D").copied().unwrap_or(0) > 1_000, "{models:?}");
    assert!(models.get("Raspberry Shake 4D").copied().unwrap_or(0) > 100, "{models:?}");
    assert!(models.get("Raspberry Boom").copied().unwrap_or(0) > 10, "{models:?}");
}
