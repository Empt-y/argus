//! Geofences: polygons with rules, and the alerts they raise.
//!
//! The shape of the feature is that a geofence is a *stored query over live
//! state*, not a subscription attached to a client. It fires whether or not
//! anyone is connected, the alert is recorded, and delivery to each device is
//! tracked separately — so a phone that was in a tunnel gets told what it
//! missed instead of losing it.

pub mod engine;
pub mod rule;

pub use engine::Engine;
pub use rule::{Candidate, Rule, Severity, Trigger};
