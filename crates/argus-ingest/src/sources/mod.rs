//! Feed drivers, one module per source.
//!
//! Every driver is the same shape: a `SourceDescriptor` declaring what it is,
//! a `poll` that fetches, and a free `decode` function that turns the wire
//! format into observations. Keeping `decode` separate from `poll` is what lets
//! each driver be tested against a captured fixture with no network.

pub mod argo;
pub mod aurorawatch;
pub mod bods;
pub mod cables;
pub mod carbon;
pub mod eaflood;
pub mod elements;
pub mod emodnet;
pub mod spacetrack;
pub mod celestrak;
pub mod cneos;
pub mod emsc;
pub mod firms;
pub mod fsa;
pub mod glofas;
pub mod gmn;
pub mod grip;
pub mod ioda;
pub mod metar;
pub mod ndbc;
pub mod nws;
pub mod openmeteo;
pub mod osmpower;
pub mod ourairports;
pub mod opensky;
pub mod police;
pub mod sigmet;
pub mod sondehub;
pub mod stormoverflow;
pub mod tfl;
pub mod readsb;
pub mod rislive;
pub mod raspberryshake;
pub mod satnogs;
pub mod usgs;
pub mod wspr;

pub use argo::ArgoFloats;
pub use aurorawatch::AuroraWatch;
pub use bods::Buses;
pub use cables::SubmarineCables;
pub use carbon::CarbonIntensity;
pub use celestrak::CelestrakSatellites;
pub use cneos::Fireballs;
pub use eaflood::{EaFloodWarnings, EaRiverGauges};
pub use emodnet::Emodnet;
pub use emsc::EmscEarthquakes;
pub use firms::Fires;
pub use fsa::FoodHygiene;
pub use glofas::RiverDischarge;
pub use gmn::GmnMeteors;
pub use grip::Grip;
pub use ioda::Ioda;
pub use metar::Metars;
pub use ndbc::NdbcBuoys;
pub use nws::NwsAlerts;
pub use openmeteo::OpenMeteo;
pub use osmpower::PowerGrid;
pub use ourairports::Airports;
pub use opensky::OpenSky;
pub use police::StreetCrime;
pub use sigmet::Sigmets;
pub use sondehub::SondeHub;
pub use stormoverflow::StormOverflows;
pub use tfl::TflRoadDisruptions;
pub use readsb::ReadsbProvider;
pub use rislive::RisLive;
pub use raspberryshake::RaspberryShake;
pub use satnogs::SatnogsStations;
pub use usgs::UsgsEarthquakes;
pub use wspr::WsprPaths;
