//! Cross-validate SGP4 propagation and the TEME->geodetic chain against an
//! independent tracker.
//!
//! The unit tests can only prove the ISS lands somewhere plausible — right
//! altitude band, right inclination, moving at orbital speed. They cannot catch
//! a sidereal angle that is subtly wrong, because that still produces a
//! perfectly plausible orbit, just in the wrong place. Only an independent
//! reference catches that.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run.

use argus_core::source::{SourceError, SourceId};
use chrono::{TimeZone, Utc};

/// Fetch a reference input, or skip the test if the upstream is simply down.
///
/// The distinction this draws is the whole point of the helper. This test
/// exists to catch *our* propagation being wrong, and a CelesTrak outage says
/// nothing about that — on 2026-08-29 it served 500s and 503s for hours and
/// turned a green suite red without a line of Argus having changed. A test that
/// cries wolf when a third party has a bad afternoon is a test people learn to
/// ignore, and this one is the only thing standing between a sign error in the
/// sidereal angle and a satellite layer that is confidently in the wrong place.
///
/// So availability failures skip. A `Decode` failure does *not*: a 200 with the
/// wrong shape means the upstream changed its schema, which breaks the real
/// driver too and is exactly the sort of thing that should turn something red.
/// The observed outage mode is 5xx, which `HttpClient` maps to `Transport`, so
/// skipping this class costs no coverage of the failure it was written for.
async fn fetch_or_skip<T: serde::de::DeserializeOwned>(
    http: &argus_ingest::HttpClient,
    what: &str,
    url: &str,
) -> Option<T> {
    match http.get_json::<T>(url).await {
        Ok(value) => Some(value),
        Err(err @ (SourceError::Transport(_)
        | SourceError::RateLimited { .. }
        | SourceError::Forbidden(_))) => {
            eprintln!("SKIPPING: {what} is unavailable ({err}); this says nothing about our propagation");
            None
        }
        Err(err) => panic!("{what}: {err}"),
    }
}

#[tokio::test]
async fn our_iss_position_agrees_with_an_independent_tracker() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("skipping: set ARGUS_NETWORK_TESTS=1 to run");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).unwrap();

    // The reference reports the instant it answered for; propagate to exactly
    // that, or the ISS's 7.6 km/s makes any comparison meaningless.
    let Some(reference) = fetch_or_skip::<serde_json::Value>(
        &http,
        "the reference tracker (wheretheiss.at)",
        "https://api.wheretheiss.at/v1/satellites/25544",
    )
    .await
    else {
        return;
    };
    let ref_lat = reference["latitude"].as_f64().unwrap();
    let ref_lon = reference["longitude"].as_f64().unwrap();
    let at = Utc
        .timestamp_opt(reference["timestamp"].as_i64().unwrap(), 0)
        .single()
        .unwrap();

    let Some(elements) = fetch_or_skip::<Vec<sgp4::Elements>>(
        &http,
        "the CelesTrak stations catalogue",
        "https://celestrak.org/NORAD/elements/gp.php?GROUP=stations&FORMAT=json",
    )
    .await
    else {
        return;
    };

    // CelesTrak answers 200 with an empty array while its own upstream is
    // refreshing. Nothing to propagate is not a propagation error.
    if elements.is_empty() {
        eprintln!("SKIPPING: CelesTrak returned an empty catalogue");
        return;
    }

    let obs = argus_ingest::sources::celestrak::propagate_all(
        &elements,
        at,
        &SourceId::new("celestrak"),
    );
    let iss = obs
        .iter()
        .find(|o| o.entity.key == "25544")
        .expect("ISS in the stations group");
    let p = iss.position.unwrap();

    let separation_km =
        argus_core::geo::haversine_m(p.lat, p.lon, ref_lat, ref_lon) / 1000.0;

    println!("ours:      {:.3}, {:.3}", p.lat, p.lon);
    println!("reference: {ref_lat:.3}, {ref_lon:.3}");
    println!("separation: {separation_km:.1} km");

    // Two independent propagators working from element sets fetched moments
    // apart will not agree exactly, but they must agree closely. 100 km is
    // ~13 seconds of ISS flight — loose enough for element-set age and
    // implementation differences, tight enough that a wrong rotation
    // direction (which puts it thousands of km away) fails loudly.
    assert!(
        separation_km < 100.0,
        "propagation disagrees with the reference by {separation_km:.1} km"
    );
}
