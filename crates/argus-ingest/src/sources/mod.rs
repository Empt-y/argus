//! Feed drivers, one module per source.
//!
//! Every driver is the same shape: a `SourceDescriptor` declaring what it is,
//! a `poll` that fetches, and a free `decode` function that turns the wire
//! format into observations. Keeping `decode` separate from `poll` is what lets
//! each driver be tested against a captured fixture with no network.

pub mod argo;
pub mod bods;
pub mod carbon;
pub mod eaflood;
pub mod elements;
pub mod emodnet;
pub mod spacetrack;
pub mod celestrak;
pub mod emsc;
pub mod gmn;
pub mod metar;
pub mod ndbc;
pub mod nws;
pub mod openmeteo;
pub mod opensky;
pub mod police;
pub mod sigmet;
pub mod sondehub;
pub mod stormoverflow;
pub mod tfl;
pub mod readsb;
pub mod raspberryshake;
pub mod satnogs;
pub mod usgs;

pub use argo::ArgoFloats;
pub use bods::Buses;
pub use carbon::CarbonIntensity;
pub use celestrak::CelestrakSatellites;
pub use eaflood::{EaFloodWarnings, EaRiverGauges};
pub use emodnet::Emodnet;
pub use emsc::EmscEarthquakes;
pub use gmn::GmnMeteors;
pub use metar::Metars;
pub use ndbc::NdbcBuoys;
pub use nws::NwsAlerts;
pub use openmeteo::OpenMeteo;
pub use opensky::OpenSky;
pub use police::StreetCrime;
pub use sigmet::Sigmets;
pub use sondehub::SondeHub;
pub use stormoverflow::StormOverflows;
pub use tfl::TflRoadDisruptions;
pub use readsb::ReadsbProvider;
pub use raspberryshake::RaspberryShake;
pub use satnogs::SatnogsStations;
pub use usgs::UsgsEarthquakes;
