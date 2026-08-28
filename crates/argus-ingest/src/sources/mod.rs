//! Feed drivers, one module per source.
//!
//! Every driver is the same shape: a `SourceDescriptor` declaring what it is,
//! a `poll` that fetches, and a free `decode` function that turns the wire
//! format into observations. Keeping `decode` separate from `poll` is what lets
//! each driver be tested against a captured fixture with no network.

pub mod celestrak;
pub mod emsc;
pub mod nws;
pub mod opensky;
pub mod readsb;
pub mod usgs;

pub use celestrak::CelestrakSatellites;
pub use emsc::EmscEarthquakes;
pub use nws::NwsAlerts;
pub use opensky::OpenSky;
pub use readsb::ReadsbProvider;
pub use usgs::UsgsEarthquakes;
