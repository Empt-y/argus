//! Feed ingestion: the driver contract's runtime half.
//!
//! `argus_core` defines what a [`Source`](argus_core::Source) *is*; this crate
//! decides when each one runs, enforces the guards they share, turns poll
//! outcomes into honest health states, and writes the results into the DVR.

pub mod chain;
pub mod geojson;
pub mod topojson;
pub mod zip;
pub mod http;
pub mod ripestat;
pub mod runtime;
pub mod scheduler;
pub mod sources;

pub use chain::{ChainStatus, ProviderChain};
pub use http::HttpClient;
pub use runtime::{CredentialResolver, Runtime};
pub use scheduler::{PollOutcome, SchedulerConfig, SourceState, next_delay, poll_once};
