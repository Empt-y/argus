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

use argus_core::source::SourceId;
use chrono::{TimeZone, Utc};

#[tokio::test]
async fn our_iss_position_agrees_with_an_independent_tracker() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("skipping: set ARGUS_NETWORK_TESTS=1 to run");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).unwrap();

    // The reference reports the instant it answered for; propagate to exactly
    // that, or the ISS's 7.6 km/s makes any comparison meaningless.
    let reference: serde_json::Value = http
        .get_json("https://api.wheretheiss.at/v1/satellites/25544")
        .await
        .expect("reference tracker");
    let ref_lat = reference["latitude"].as_f64().unwrap();
    let ref_lon = reference["longitude"].as_f64().unwrap();
    let at = Utc
        .timestamp_opt(reference["timestamp"].as_i64().unwrap(), 0)
        .single()
        .unwrap();

    let elements: Vec<sgp4::Elements> = http
        .get_json("https://celestrak.org/NORAD/elements/gp.php?GROUP=stations&FORMAT=json")
        .await
        .expect("celestrak stations");

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
