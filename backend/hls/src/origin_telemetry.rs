//! Origin-transfer telemetry: one record per origin fetch, folded into per-input rolling windows.
//!
//! Records carry no URLs and no credentials; the segment id is a keyed hash that is only
//! stable within one process.

mod aggregate;
mod model;
mod recorder;

#[cfg(test)]
mod tests;

pub use self::{
    aggregate::{OriginAvailability, OriginInputSnapshot, OriginTelemetrySnapshot, OriginTransferQuality},
    model::{OriginFetchOutcome, OriginFetchPhaseError, OriginFetchRecord, OriginResourceKind},
    recorder::{FetchProbe, OriginTelemetryRecorder},
};
