//! Shared vocabulary for Argus.
//!
//! Everything downstream — ingest drivers, the store, the tiler, the API — speaks
//! in the types defined here. Keeping this crate free of I/O and of any database
//! or HTTP dependency is deliberate: it is the one place where a change ripples
//! everywhere, so it should be cheap to read and cheap to test.

pub mod cache;
pub mod entity;
pub mod geo;
pub mod layer;
pub mod orbital;
pub mod seismic;
pub mod source;

pub use cache::{GeometryCache, MemoryGeometryCache, TrackedCatalogue};
pub use entity::{
    AltitudeDatum, EntityId, EntityKind, Kinematics, Observation, Position, Quality,
};
pub use geo::{BoundingBox, TileCoord};
pub use layer::{GeometryClass, LayerStyle};
pub use source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceHealth, SourceId, StreamSource,
};
