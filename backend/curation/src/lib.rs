//! Playlist curation capability.
//!
//! This crate owns the trusted, source-neutral matching and virtual-category
//! projection kernel, plus the concrete edge adapter that translates foreign
//! source data before invoking that kernel. Trakt and TMDB Trending are concrete adapters.
//!
//! Serialized configuration remains in `shared`, resolved configuration remains
//! in `tuliprox-core`, and target-stage orchestration and persistence remain in
//! their existing processing and repository crates.

mod coordinator;
mod kernel;
#[cfg(test)]
mod test_support;
mod tmdb;
mod trakt;

pub use coordinator::{evaluate_curation, project_curation_categories};
pub use kernel::{
    CurationEvaluation, CurationFailure, CurationIncompleteReason, CurationMediaKind, CurationMembership,
    CurationRunOutcome, CurationSelectorKey, CurationSelectorSummary, CurationUnavailableReason, SelectorOutcome,
};
pub use trakt::{evaluate_trakt_curation, project_trakt_categories};
