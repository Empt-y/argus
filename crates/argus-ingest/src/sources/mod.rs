//! Feed drivers, one module per source.
//!
//! Every driver is the same shape: a `SourceDescriptor` declaring what it is,
//! a `poll` that fetches, and a free `decode` function that turns the wire
//! format into observations. Keeping `decode` separate from `poll` is what lets
//! each driver be tested against a captured fixture with no network.

pub mod eaflood;
pub mod elements;
pub mod spacetrack;
pub mod celestrak;
pub mod emsc;
pub mod ndbc;
pub mod nws;
pub mod opensky;
pub mod sigmet;
pub mod sondehub;
pub mod stormoverflow;
pub mod readsb;
pub mod usgs;

pub use celestrak::CelestrakSatellites;
pub use eaflood::{EaFloodWarnings, EaRiverGauges};
pub use emsc::EmscEarthquakes;
pub use ndbc::NdbcBuoys;
pub use nws::NwsAlerts;
pub use opensky::OpenSky;
pub use sigmet::Sigmets;
pub use sondehub::SondeHub;
pub use stormoverflow::StormOverflows;
pub use readsb::ReadsbProvider;
pub use usgs::UsgsEarthquakes;
