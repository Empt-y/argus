//! The unified observation model.
//!
//! Every feed in Argus — a global ADS-B poll, a single seismograph, a submarine
//! cable route, a BGP withdrawal — normalises into [`Observation`]. Getting this
//! shape right is what keeps a new layer down to a ~200-line driver, so think
//! hard before widening it: a field that only one source can populate belongs in
//! [`Observation::attrs`], not here.

use chrono::{DateTime, Utc};
use geo_types::Geometry;
use serde::{Deserialize, Serialize};
use std::fmt;

/// What sort of thing an entity is. Determines default rendering and which
/// analytic passes apply to it, and is half of an entity's primary key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    /// Anything airborne with a transponder or a propagated position.
    Aircraft,
    /// Surface and subsurface vessels.
    Vessel,
    /// Orbiting objects propagated from element sets.
    Satellite,
    /// A thing that *happened* at a place and time: a quake, a fire detection,
    /// a launch, a BGP hijack, a conflict report. Events are immutable once
    /// observed; they are never updated, only superseded.
    Event,
    /// A fixed installation that reports readings: a camera, a river gauge, an
    /// air-quality monitor, a radio broadcaster, a bikeshare dock.
    Station,
    /// Static or slow-moving geography: cables, dams, transmission lines,
    /// boundaries. Stored in `features`, not the observation hypertable.
    Feature,
    /// A scalar or raster reading not tied to a discrete object: grid frequency,
    /// Kp index, a radar sweep.
    Measure,
}

impl EntityKind {
    /// Stable wire/database spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aircraft => "aircraft",
            Self::Vessel => "vessel",
            Self::Satellite => "satellite",
            Self::Event => "event",
            Self::Station => "station",
            Self::Feature => "feature",
            Self::Measure => "measure",
        }
    }

    /// Whether observations of this kind are worth writing to the time-series
    /// store. Features are versioned in their own table instead; re-recording a
    /// submarine cable every poll would be pure noise.
    pub const fn is_timeseries(self) -> bool {
        !matches!(self, Self::Feature)
    }

    /// Whether an observation of this kind describes the world *now*, so that
    /// the gap between `observed_at` and `ingested_at` measures feed staleness.
    ///
    /// For a tracked aircraft it does: a two-minute-old position means the feed
    /// is two minutes behind. For an event it does not — an earthquake's
    /// `observed_at` is when the ground moved, and a feed covering a rolling
    /// 24 hours will always contain day-old events without being stale in any
    /// sense. Measuring lag on those reports every quiet event feed as delayed
    /// forever.
    pub const fn reports_current_state(self) -> bool {
        !matches!(self, Self::Event)
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Identity of a tracked thing, stable across sources and across restarts.
///
/// The `key` must be a *natural* key from the domain — `icao24` for aircraft,
/// MMSI for vessels, NORAD ID for satellites, the USGS event id for quakes.
/// Never a database id and never something a source can renumber, because two
/// different feeds observing the same aircraft must collide on purpose: that is
/// how ADS-B from OpenSky and from a local dongle merge into one track.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityId {
    pub kind: EntityKind,
    pub key: String,
}

impl EntityId {
    pub fn new(kind: EntityKind, key: impl Into<String>) -> Self {
        Self {
            kind,
            key: key.into(),
        }
    }

    pub fn aircraft(icao24: impl AsRef<str>) -> Self {
        // ICAO 24-bit addresses arrive upper-, lower- and mixed-case depending on
        // the feed. Normalising here is what lets a local dongle and OpenSky
        // agree on identity.
        Self::new(EntityKind::Aircraft, icao24.as_ref().trim().to_lowercase())
    }

    pub fn vessel(mmsi: impl fmt::Display) -> Self {
        Self::new(EntityKind::Vessel, mmsi.to_string())
    }

    /// NORAD catalogue number. Takes `u64` rather than `u32` deliberately: the
    /// public catalogue has outgrown the historical 5-digit range, and
    /// truncating an id would silently merge two different objects onto one
    /// track.
    pub fn satellite(norad_id: u64) -> Self {
        Self::new(EntityKind::Satellite, norad_id.to_string())
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.key)
    }
}

/// What an altitude is measured *from*.
///
/// This is not pedantry. Upstream's single largest class of visual bug was
/// aircraft sinking into or floating above the terrain, and every instance of it
/// traced back to mixing these up. ADS-B reports pressure altitude by default;
/// rendering that against a geoid-referenced 3D mesh puts an airliner
/// underground on a high-pressure day. Carry the datum with the number and
/// convert explicitly, never by assumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AltitudeDatum {
    /// Height above the WGS-84 reference ellipsoid. What GNSS reports natively.
    Wgs84Ellipsoid,
    /// Height above mean sea level via an EGM geoid model.
    Geoid,
    /// Height above the local ground surface.
    AboveGround,
    /// Pressure altitude against the 1013.25 hPa standard datum. What a Mode-S
    /// transponder reports, and *not* a geometric height.
    Barometric,
}

/// A point on or above the Earth.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// Degrees east, in `-180.0..=180.0`.
    pub lon: f64,
    /// Degrees north, in `-90.0..=90.0`.
    pub lat: f64,
    /// Metres, interpreted per `datum`. `None` means the source gave a 2D fix.
    pub alt_m: Option<f64>,
    pub datum: AltitudeDatum,
}

impl Position {
    /// A surface position with no altitude information.
    pub fn surface(lon: f64, lat: f64) -> Self {
        Self {
            lon,
            lat,
            alt_m: None,
            datum: AltitudeDatum::Geoid,
        }
    }

    /// Reject the values that feeds genuinely emit when they mean "no fix":
    /// out-of-range coordinates and the null-island `0,0` that several ADS-B and
    /// AIS sources use as a placeholder. Dropping `0,0` costs us any real
    /// observation in the Gulf of Guinea, which is an accepted trade — the
    /// placeholder is orders of magnitude more common than the real position.
    pub fn is_plausible(&self) -> bool {
        self.lon.is_finite()
            && self.lat.is_finite()
            && (-180.0..=180.0).contains(&self.lon)
            && (-90.0..=90.0).contains(&self.lat)
            && !(self.lon == 0.0 && self.lat == 0.0)
    }
}

/// How a thing is moving. All angles are degrees clockwise from true north.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Kinematics {
    /// Direction of travel over the ground.
    pub course_deg: Option<f64>,
    /// Direction the body is pointing. Differs from `course_deg` under wind or
    /// current, which is exactly why both exist: icons must be drawn on
    /// `heading_deg` where it is known, but dead reckoning must integrate along
    /// `course_deg`.
    pub heading_deg: Option<f64>,
    pub ground_speed_mps: Option<f64>,
    /// Positive is climbing.
    pub vertical_rate_mps: Option<f64>,
}

/// How much this observation should be trusted, and why.
///
/// Argus never presents modelled data as live. This value propagates from the
/// driver all the way to the layer row in both clients, so a user can always see
/// whether they are looking at a real transponder return or an interpolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// Measured, and current as of `observed_at`.
    Live,
    /// Measured, but the source publishes on a lag we know about.
    Delayed,
    /// Computed from a physical model rather than measured — an SGP4-propagated
    /// satellite, a dead-reckoned aircraft between polls.
    Modeled,
    /// A prior or an approximation, not a measurement. Camera poses before
    /// calibration; keyless traffic simulation.
    Estimated,
    /// Was live, but the source has stopped answering and this is the last
    /// value we hold.
    Stale,
}

impl Quality {
    /// Whether this value came from an actual measurement of the world.
    pub const fn is_measured(self) -> bool {
        matches!(self, Self::Live | Self::Delayed | Self::Stale)
    }
}

/// One normalised reading from one source about one entity at one instant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    /// Which driver produced this. Used for provenance and for source-priority
    /// merging when two feeds describe the same entity.
    pub source_id: crate::source::SourceId,
    pub entity: EntityId,
    /// When the *world* was in this state, per the source. Never the local
    /// clock: interpolation, dead reckoning and the DVR all key off this, and
    /// substituting ingest time silently smears every track by the poll latency.
    pub observed_at: DateTime<Utc>,
    /// When Argus received it. `ingested_at - observed_at` is the source lag,
    /// which is what drives the `Delayed` quality state.
    pub ingested_at: DateTime<Utc>,
    pub position: Option<Position>,
    pub kinematics: Option<Kinematics>,
    /// Non-point geography: a fire perimeter, an alert polygon, a forecast cone,
    /// a cable route.
    pub geom: Option<Geometry<f64>>,
    /// Short human label, if the source supplies one — a callsign, a ship name,
    /// a station name.
    pub label: Option<String>,
    pub quality: Quality,
    /// Source-specific payload. Anything not universal lives here rather than
    /// widening this struct.
    pub attrs: serde_json::Value,
}

impl Observation {
    /// Start a well-formed observation; fill the optional fields with the
    /// builder methods.
    pub fn new(
        source_id: crate::source::SourceId,
        entity: EntityId,
        observed_at: DateTime<Utc>,
        quality: Quality,
    ) -> Self {
        Self {
            source_id,
            entity,
            observed_at,
            ingested_at: Utc::now(),
            position: None,
            kinematics: None,
            geom: None,
            label: None,
            quality,
            attrs: serde_json::Value::Null,
        }
    }

    #[must_use]
    pub fn with_position(mut self, position: Position) -> Self {
        self.position = Some(position);
        self
    }

    #[must_use]
    pub fn with_kinematics(mut self, kinematics: Kinematics) -> Self {
        self.kinematics = Some(kinematics);
        self
    }

    #[must_use]
    pub fn with_geom(mut self, geom: Geometry<f64>) -> Self {
        self.geom = Some(geom);
        self
    }

    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    #[must_use]
    pub fn with_attrs(mut self, attrs: serde_json::Value) -> Self {
        self.attrs = attrs;
        self
    }

    /// How far behind real time this observation was when we got it.
    /// Negative durations are clamped to zero: several feeds publish timestamps
    /// a second or two into the future from clock skew, and a negative lag would
    /// otherwise poison the source-health average.
    pub fn lag(&self) -> chrono::Duration {
        (self.ingested_at - self.observed_at).max(chrono::Duration::zero())
    }

    /// Whether this observation carries anything worth storing. A reading with
    /// no position, no geometry and no attributes is a source bug, not data.
    pub fn is_meaningful(&self) -> bool {
        self.position.is_some_and(|p| p.is_plausible())
            || self.geom.is_some()
            || !self.attrs.is_null()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aircraft_keys_normalise_case_so_feeds_collide() {
        // A local dongle reporting "A1B2C3" and OpenSky reporting "a1b2c3" must
        // land on the same track, or every aircraft appears twice.
        assert_eq!(EntityId::aircraft("A1B2C3"), EntityId::aircraft("a1b2c3"));
        assert_eq!(EntityId::aircraft(" a1b2c3 ").key, "a1b2c3");
    }

    #[test]
    fn null_island_is_rejected_as_a_placeholder() {
        assert!(!Position::surface(0.0, 0.0).is_plausible());
        assert!(Position::surface(-97.74, 30.27).is_plausible());
    }

    #[test]
    fn out_of_range_and_nan_positions_are_rejected() {
        assert!(!Position::surface(181.0, 0.0).is_plausible());
        assert!(!Position::surface(0.0, 91.0).is_plausible());
        assert!(!Position::surface(f64::NAN, 30.0).is_plausible());
        assert!(!Position::surface(1.0, f64::INFINITY).is_plausible());
    }

    #[test]
    fn event_feeds_do_not_measure_staleness_by_observation_age() {
        // Regression: the USGS feed covers a rolling 24 hours, so the oldest
        // quake in it is always ~24h old. Treating that as feed lag reported a
        // perfectly healthy source as permanently delayed.
        assert!(!EntityKind::Event.reports_current_state());
        assert!(EntityKind::Aircraft.reports_current_state());
        assert!(EntityKind::Vessel.reports_current_state());
        assert!(EntityKind::Satellite.reports_current_state());
    }

    #[test]
    fn features_are_excluded_from_the_timeseries() {
        assert!(!EntityKind::Feature.is_timeseries());
        assert!(EntityKind::Aircraft.is_timeseries());
        assert!(EntityKind::Measure.is_timeseries());
    }

    #[test]
    fn modelled_data_never_counts_as_measured() {
        assert!(!Quality::Modeled.is_measured());
        assert!(!Quality::Estimated.is_measured());
        assert!(Quality::Live.is_measured());
        // Stale is a real past measurement, just an old one.
        assert!(Quality::Stale.is_measured());
    }

    #[test]
    fn clock_skew_cannot_produce_negative_lag() {
        let observed = Utc::now() + chrono::Duration::seconds(5);
        let mut obs = Observation::new(
            crate::source::SourceId::new("test"),
            EntityId::aircraft("abc123"),
            observed,
            Quality::Live,
        );
        obs.ingested_at = Utc::now();
        assert_eq!(obs.lag(), chrono::Duration::zero());
    }

    #[test]
    fn empty_observations_are_not_meaningful() {
        let obs = Observation::new(
            crate::source::SourceId::new("test"),
            EntityId::aircraft("abc123"),
            Utc::now(),
            Quality::Live,
        );
        assert!(!obs.is_meaningful());
        assert!(
            obs.clone()
                .with_position(Position::surface(-97.74, 30.27))
                .is_meaningful()
        );
        // A position that fails plausibility does not rescue it.
        assert!(
            !obs.clone()
                .with_position(Position::surface(0.0, 0.0))
                .is_meaningful()
        );
    }
}
