//! Satellites, propagated from CelesTrak element sets.
//!
//! Structurally different from every other driver so far, and the difference is
//! worth understanding before copying this shape: the *fetch* and the
//! *observation* are on completely separate clocks. Element sets change slowly
//! and CelesTrak explicitly asks not to be polled hard for them, so they are
//! fetched every few hours and cached. Positions, meanwhile, are produced on
//! every poll by propagating those cached elements forward.
//!
//! Which means these observations are `Quality::Modeled`, always. Nothing here
//! is measured. A satellite's position is the output of a physics model fed by
//! an element set that was itself fitted to observations hours or days ago, and
//! the client must be able to say so.
//!
//! **The element set is kept on disk, and that is an availability feature, not
//! an optimisation.** Elements stay usable for about a week, so an upstream
//! outage of a few hours should be invisible: there is nothing to fetch that we
//! do not already have. Holding them only in memory broke that promise in the
//! least obvious way — the layer survived CelesTrak being down and then died
//! the moment the daemon restarted, because a restart threw away a catalogue
//! that was still perfectly good. Observed on 2026-08-30, with celestrak.org
//! unreachable and the layer going empty on restart rather than coasting.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::orbital;
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Duration, Utc};
use tokio::sync::RwLock;

/// Curated core catalogue, ~1,100 objects across every orbital regime worth
/// looking at: crewed stations, the naked-eye visible set, all four GNSS
/// constellations, weather and science platforms, and the geostationary belt.
///
/// Deliberately NOT `GROUP=active`. That is 16,469 objects and 6.7 MB, which at
/// a 60-second propagation cadence is ~23 million rows a day — the DVR budget
/// gone in under a week, on data that is perfectly reconstructable from the
/// element sets themselves. The Starlink shell is available the same way and is
/// opt-in for the same reason.
pub const CORE_GROUPS: &[&str] = &[
    "stations", "visual", "gnss", "weather", "science", "geo",
];

fn group_url(group: &str) -> String {
    format!("https://celestrak.org/NORAD/elements/gp.php?GROUP={group}&FORMAT=json")
}

/// How often to re-fetch element sets. CelesTrak's guidance is to treat these
/// as slow-moving and cache them; refetching per position poll would be abuse
/// of a free service for no accuracy gain.
const ELEMENTS_TTL: Duration = Duration::hours(6);

/// How often to emit propagated positions. Fast enough that an orbit draws
/// smoothly, slow enough that ~1,100 objects do not dominate the write path.
const PROPAGATE_CADENCE_SECS: u64 = 60;

/// Beyond this, a propagated position is too stale to be worth publishing.
/// SGP4 error grows quickly, and a week-old element set is fiction rather than
/// an estimate.
const MAX_ELEMENT_AGE: Duration = Duration::days(7);

pub struct CelestrakSatellites {
    descriptor: SourceDescriptor,
    http: HttpClient,
    groups: Vec<String>,
    cache: RwLock<Option<ElementCache>>,
    /// Where the last good element set is kept between runs. `None` disables
    /// persistence, which is what the tests use.
    store_path: Option<std::path::PathBuf>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ElementCache {
    fetched_at: DateTime<Utc>,
    elements: Vec<sgp4::Elements>,
}

impl CelestrakSatellites {
    pub fn new(http: HttpClient) -> Self {
        Self::with_groups(http, CORE_GROUPS.iter().map(|s| (*s).to_string()).collect())
    }

    /// Track a specific set of CelesTrak groups. Adding `starlink` here is what
    /// turns on the full shell; be aware of what that does to the row budget.
    pub fn with_groups(http: HttpClient, groups: Vec<String>) -> Self {
        Self {
            groups,
            descriptor: SourceDescriptor {
                id: SourceId::new("celestrak"),
                layer_id: LayerId::new("satellites"),
                display_name: "Satellites (CelesTrak, core catalogue)".into(),
                kind: EntityKind::Satellite,
                cadence: Cadence::every(PROPAGATE_CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "CelesTrak (Dr. T.S. Kelso)".into(),
                    url: "https://celestrak.org/".into(),
                    license: "Free for non-commercial use; see CelesTrak terms".into(),
                    notice: Some("Orbital data courtesy of CelesTrak".into()),
                },
                // Never measured. Always the output of a model.
                base_quality: Quality::Modeled,
                // No published numeric cap; CelesTrak instead asks that
                // element sets be cached rather than re-fetched, which
                // ELEMENTS_TTL honours. It enforces that with 403s.
                quota: None,
            },
            http,
            cache: RwLock::new(None),
            store_path: None,
        }
    }

    /// Keep the element set in `dir`, so an upstream outage that spans a
    /// restart costs nothing.
    #[must_use]
    pub fn persisting_in(mut self, dir: &std::path::Path) -> Self {
        self.store_path = Some(dir.join("celestrak-elements.json"));
        self
    }

    /// The last element set written to disk, if it is still worth propagating.
    async fn load_stored(&self) -> Option<ElementCache> {
        let path = self.store_path.as_ref()?;
        let text = tokio::fs::read_to_string(path).await.ok()?;
        let cache: ElementCache = serde_json::from_str(&text).ok()?;
        if cache.elements.is_empty() {
            return None;
        }
        // Age is judged per element when propagating, but a wholesale refusal
        // here keeps a truly ancient file from looking like a live source.
        if Utc::now() - cache.fetched_at > MAX_ELEMENT_AGE {
            tracing::warn!(
                "celestrak: stored elements are older than {} days, ignoring",
                MAX_ELEMENT_AGE.num_days()
            );
            return None;
        }
        Some(cache)
    }

    async fn to_disk(&self, cache: &ElementCache) {
        let Some(path) = self.store_path.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        // Write beside and rename, so a daemon killed mid-write leaves the
        // previous good catalogue rather than a truncated one.
        let tmp = path.with_extension("json.tmp");
        match serde_json::to_vec(cache) {
            Ok(bytes) => {
                if tokio::fs::write(&tmp, &bytes).await.is_ok() {
                    let _ = tokio::fs::rename(&tmp, path).await;
                }
            }
            Err(err) => tracing::warn!("celestrak: could not serialise elements: {err}"),
        }
    }

    /// Return cached elements, refetching only when the cache has aged out.
    async fn elements(&self) -> Result<Vec<sgp4::Elements>, SourceError> {
        if let Some(cache) = self.cache.read().await.as_ref()
            && Utc::now() - cache.fetched_at < ELEMENTS_TTL
        {
            return Ok(cache.elements.clone());
        }

        // Nothing in memory yet: a fresh start, or a restart. Anything on disk
        // is worth adopting before reaching for the network, and is what makes
        // a restart during an outage a non-event.
        if self.cache.read().await.is_none()
            && let Some(stored) = self.load_stored().await
        {
            let age = Utc::now() - stored.fetched_at;
            tracing::info!(
                objects = stored.elements.len(),
                age_hours = age.num_minutes() as f64 / 60.0,
                "celestrak: adopted stored element set"
            );
            let elements = stored.elements.clone();
            *self.cache.write().await = Some(stored);
            if age < ELEMENTS_TTL {
                return Ok(elements);
            }
            // Older than the refresh interval: try upstream, but we now have
            // something to fall back to if it will not answer.
        }

        // Groups overlap — a station is also visible, GNSS birds sit in the GEO
        // belt — so dedupe by NORAD id and keep the first sighting.
        let mut seen = std::collections::HashSet::new();
        let mut fetched: Vec<sgp4::Elements> = Vec::new();
        let mut failures = Vec::new();

        for group in &self.groups {
            match self.http.get_json::<Vec<sgp4::Elements>>(&group_url(group)).await {
                Ok(batch) => {
                    for e in batch {
                        if seen.insert(e.norad_id) {
                            fetched.push(e);
                        }
                    }
                }
                // One group failing must not cost the rest. A partial
                // catalogue is far better than none, and the health state
                // says so rather than claiming everything is fine.
                Err(err) => failures.push(format!("{group}: {err}")),
            }
        }

        if fetched.is_empty() {
            // An empty catalogue is a bad response, not a world with no
            // satellites in it. Elements outlive an outage comfortably, so
            // coast on what we hold and say so, rather than blanking a layer
            // over a few hours of someone else's downtime.
            if let Some(cache) = self.cache.read().await.as_ref() {
                let age = Utc::now() - cache.fetched_at;
                if age < MAX_ELEMENT_AGE {
                    tracing::warn!(
                        objects = cache.elements.len(),
                        age_hours = age.num_minutes() as f64 / 60.0,
                        "celestrak unreachable; propagating from held elements"
                    );
                    return Ok(cache.elements.clone());
                }
            }
            return Err(failures.into_iter().next().map_or_else(
                || SourceError::Decode("CelesTrak returned an empty element set".into()),
                SourceError::Transport,
            ));
        }
        if !failures.is_empty() {
            tracing::warn!("celestrak: {} of {} groups failed: {}",
                failures.len(), self.groups.len(), failures.join("; "));
        }

        let fresh = ElementCache {
            fetched_at: Utc::now(),
            elements: fetched.clone(),
        };
        self.to_disk(&fresh).await;
        *self.cache.write().await = Some(fresh);
        Ok(fetched)
    }
}

#[async_trait::async_trait]
impl Source for CelestrakSatellites {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let elements = self.elements().await?;
        Ok(propagate_all(&elements, Utc::now(), &self.descriptor.id))
    }
}

/// Propagate a whole catalogue to one instant.
///
/// Objects that fail to propagate are dropped rather than aborting the batch:
/// a public catalogue always contains a few decayed or malformed entries, and
/// one bad element set must not cost the other eight hundred.
pub fn propagate_all(
    elements: &[sgp4::Elements],
    at: DateTime<Utc>,
    source_id: &SourceId,
) -> Vec<Observation> {
    elements
        .iter()
        .filter_map(|e| propagate_one(e, at, source_id))
        .collect()
}

fn propagate_one(
    elements: &sgp4::Elements,
    at: DateTime<Utc>,
    source_id: &SourceId,
) -> Option<Observation> {
    let norad_id = elements.norad_id;
    let epoch = elements.datetime.and_utc();
    let age = at - epoch;

    if age.abs() > MAX_ELEMENT_AGE {
        return None;
    }

    let constants = sgp4::Constants::from_elements(elements).ok()?;
    let minutes_since_epoch = (at - epoch).num_milliseconds() as f64 / 60_000.0;
    let prediction = constants
        .propagate(sgp4::MinutesSinceEpoch(minutes_since_epoch))
        .ok()?;

    let [x, y, z] = prediction.position;
    let (lat, lon, alt_m) = orbital::teme_to_geodetic(x, y, z, at);
    if !lat.is_finite() || !lon.is_finite() || !alt_m.is_finite() {
        return None;
    }

    // A decayed object propagates to an absurd or sub-surface altitude rather
    // than failing outright. Publishing those puts satellites underground.
    if !(80_000.0..2_000_000_000.0).contains(&alt_m) {
        return None;
    }

    let position = Position {
        lon,
        lat,
        alt_m: Some(alt_m),
        // SGP4 works against the geometric shape of the Earth, so this is
        // ellipsoidal height, not a height above mean sea level.
        datum: AltitudeDatum::Wgs84Ellipsoid,
    };
    if !position.is_plausible() {
        return None;
    }

    let [vx, vy, vz] = prediction.velocity;
    let speed_mps = (vx * vx + vy * vy + vz * vz).sqrt() * 1000.0;

    let attrs = serde_json::json!({
        "norad_id": norad_id,
        "object_name": elements.object_name,
        "international_designator": elements.international_designator,
        "epoch": epoch,
        // Surfaced deliberately: propagation error grows with this, and it is
        // the honest measure of how much to trust the position.
        "element_age_hours": (age.num_minutes() as f64 / 60.0 * 100.0).round() / 100.0,
        "mean_motion_rev_per_day": elements.mean_motion,
        "eccentricity": elements.eccentricity,
        "inclination_deg": elements.inclination,
        "period_minutes": if elements.mean_motion > 0.0 {
            Some((1440.0 / elements.mean_motion * 100.0).round() / 100.0)
        } else {
            None
        },
    });

    Some(
        Observation::new(
            source_id.clone(),
            EntityId::satellite(norad_id),
            at,
            Quality::Modeled,
        )
        .with_position(position)
        .with_kinematics(argus_core::Kinematics {
            ground_speed_mps: Some(speed_mps),
            ..Default::default()
        })
        .with_label(
            elements
                .object_name
                .clone()
                .unwrap_or_else(|| format!("NORAD {norad_id}")),
        )
        .with_attrs(attrs),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../fixtures/celestrak_active.json");

    fn elements() -> Vec<sgp4::Elements> {
        serde_json::from_str(FIXTURE).expect("fixture parses as the live wire format")
    }

    /// A scratch directory that cleans up after itself.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "argus-celestrak-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_held_element_set_survives_a_restart() {
        // The failure this prevents: CelesTrak goes down, the daemon restarts
        // for an unrelated reason, and a catalogue that was still good for
        // another six days is gone. Observed for real on 2026-08-30.
        let dir = scratch("restart");
        let http = HttpClient::new(std::time::Duration::from_secs(5)).unwrap();

        let first = CelestrakSatellites::new(http.clone()).persisting_in(&dir);
        first
            .to_disk(&ElementCache {
                fetched_at: Utc::now() - Duration::hours(2),
                elements: elements(),
            })
            .await;

        // A brand new source — as after a restart — with no network behind it.
        let second = CelestrakSatellites::new(http).persisting_in(&dir);
        let adopted = second.load_stored().await.expect("stored elements adopted");
        assert_eq!(adopted.elements.len(), elements().len());

        let obs = propagate_all(&adopted.elements, Utc::now(), &SourceId::new("celestrak"));
        assert!(!obs.is_empty(), "held elements must still propagate");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn elements_past_their_useful_life_are_not_adopted() {
        // Coasting is defensible for days and fiction after that. A week-old
        // catalogue must read as a dead source, not a working one.
        let dir = scratch("stale");
        let source = CelestrakSatellites::new(HttpClient::new(std::time::Duration::from_secs(5)).unwrap()).persisting_in(&dir);
        source
            .to_disk(&ElementCache {
                fetched_at: Utc::now() - MAX_ELEMENT_AGE - Duration::hours(1),
                elements: elements(),
            })
            .await;
        assert!(source.load_stored().await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn persistence_is_optional() {
        // Without a path configured nothing is written and nothing is read,
        // which is what keeps the tests and any embedded use hermetic.
        let source = CelestrakSatellites::new(HttpClient::new(std::time::Duration::from_secs(5)).unwrap());
        source
            .to_disk(&ElementCache {
                fetched_at: Utc::now(),
                elements: elements(),
            })
            .await;
        assert!(source.load_stored().await.is_none());
    }

    #[test]
    fn the_fixture_propagates_to_plausible_orbits() {
        let obs = propagate_all(&elements(), Utc::now(), &SourceId::new("celestrak"));
        assert!(!obs.is_empty(), "nothing propagated");
        for o in &obs {
            let p = o.position.expect("position");
            assert!(p.is_plausible(), "implausible position for {}", o.entity.key);
            let alt = p.alt_m.expect("altitude");
            // Nothing in orbit is below the Kármán line or beyond lunar
            // distance; anything outside that is a decayed or corrupt entry.
            assert!(
                (80_000.0..2_000_000_000.0).contains(&alt),
                "{} at {alt} m",
                o.entity.key
            );
            assert_eq!(p.datum, AltitudeDatum::Wgs84Ellipsoid);
        }
    }

    #[test]
    fn satellite_positions_are_always_modelled_never_live() {
        // Nothing here is measured. If this ever reads Live, the client will
        // present a physics estimate as a sensor reading.
        let obs = propagate_all(&elements(), Utc::now(), &SourceId::new("celestrak"));
        assert!(obs.iter().all(|o| o.quality == Quality::Modeled));
        assert!(obs.iter().all(|o| !o.quality.is_measured()));
    }

    #[test]
    fn the_iss_is_where_the_iss_should_be() {
        // NORAD 25544 in low Earth orbit: ~400-420 km, ~7.6 km/s, 51.6°
        // inclination so it never strays beyond those latitudes.
        let obs = propagate_all(&elements(), Utc::now(), &SourceId::new("celestrak"));
        let Some(iss) = obs.iter().find(|o| o.entity.key == "25544") else {
            eprintln!("skipping: ISS not present in fixture");
            return;
        };
        let p = iss.position.unwrap();
        let alt_km = p.alt_m.unwrap() / 1000.0;
        assert!(
            (300.0..600.0).contains(&alt_km),
            "ISS altitude {alt_km} km is outside the plausible band"
        );
        assert!(
            p.lat.abs() <= 52.5,
            "ISS at {}° exceeds its 51.6° inclination",
            p.lat
        );
        let speed = iss.kinematics.unwrap().ground_speed_mps.unwrap();
        assert!(
            (7_000.0..8_200.0).contains(&speed),
            "ISS speed {speed} m/s is not orbital"
        );
    }

    #[test]
    fn a_satellite_moves_a_sensible_distance_in_a_minute() {
        // At ~7.6 km/s an object covers ~450 km per minute. This catches a
        // propagation that silently returns the same point every time, which
        // would otherwise look like a perfectly stable satellite.
        let els = elements();
        let t0 = Utc::now();
        let a = propagate_all(&els, t0, &SourceId::new("celestrak"));
        let b = propagate_all(&els, t0 + Duration::minutes(1), &SourceId::new("celestrak"));

        let Some(first) = a.first() else { return };
        let matched = b.iter().find(|o| o.entity.key == first.entity.key).unwrap();
        let (p1, p2) = (first.position.unwrap(), matched.position.unwrap());
        let moved_km =
            argus_core::geo::haversine_m(p1.lat, p1.lon, p2.lat, p2.lon) / 1000.0;
        assert!(
            moved_km > 50.0,
            "{} moved only {moved_km} km in a minute",
            first.entity.key
        );
    }

    #[test]
    fn stale_element_sets_are_dropped_rather_than_extrapolated() {
        // SGP4 error grows fast. A month-old element set is fiction, and
        // publishing it as a position would be worse than publishing nothing.
        let els = elements();
        let far_future = Utc::now() + Duration::days(60);
        let obs = propagate_all(&els, far_future, &SourceId::new("celestrak"));
        assert!(
            obs.is_empty(),
            "propagated {} objects from month-old elements",
            obs.len()
        );
    }

    #[test]
    fn element_age_is_reported_so_trust_can_be_judged() {
        let obs = propagate_all(&elements(), Utc::now(), &SourceId::new("celestrak"));
        let o = obs.first().expect("at least one");
        let age = o.attrs["element_age_hours"].as_f64().expect("age reported");
        assert!(age.is_finite());
        assert!(o.attrs["norad_id"].is_number());
    }
}
