//! Playlist curation capability.
//!
//! This crate owns the trusted, source-neutral matching and virtual-category
//! projection kernel, plus the concrete edge adapter that translates foreign
//! source data before invoking that kernel. Trakt is currently the only adapter.
//!
//! Serialized configuration remains in `shared`, resolved configuration remains
//! in `tuliprox-core`, and target-stage orchestration and persistence remain in
//! their existing processing and repository crates.

mod kernel;
mod trakt;

pub use trakt::curate_trakt_categories;
