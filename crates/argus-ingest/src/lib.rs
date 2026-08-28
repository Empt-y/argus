//! Feed ingestion: the driver contract's runtime half.
//!
//! `argus_core` defines what a [`Source`](argus_core::Source) *is*; this crate
//! decides when each one runs, enforces the guards they share, and turns poll
//! outcomes into the honest health states clients display.

pub mod http;
pub mod scheduler;

pub use http::HttpClient;
pub use scheduler::{PollOutcome, SchedulerConfig, SourceState, next_delay, poll_once};
