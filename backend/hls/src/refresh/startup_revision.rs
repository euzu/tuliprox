//! Filling an optimistic-startup revision from a segment object.
//!
//! The origin-restart path and the alternative-switch path both create a
//! startup revision and publish its bytes from one source path. They differ
//! only in which path that is and in how they report failure, so the shared
//! step lives here and each caller maps the error to its own type.

use crate::{sync_ext::MutexExt, HlsSegmentCache, SegmentRevisionGuard, SegmentRevisionKind};
use shared::model::HlsStartupMode;
use std::{io, path::Path};
use tokio::time::Instant;

/// Maps the startup mode to the revision kind its fast-start store serves.
pub(super) fn startup_revision_kind(mode: HlsStartupMode) -> SegmentRevisionKind {
    if mode.streams_raw_revisions() {
        SegmentRevisionKind::Raw
    } else {
        SegmentRevisionKind::Processed
    }
}

/// Publishes `source` as the revision body, pins the published file for the
/// revision's lifetime and marks the revision complete.
pub(super) async fn complete_startup_revision(
    cache: &HlsSegmentCache,
    guard: &SegmentRevisionGuard,
    source: &Path,
    deadline: Instant,
) -> io::Result<()> {
    let metadata = cache.publish_processed_revision(&guard.revision().key, source, deadline).await?;
    let pin = cache.pin_revision_file(&metadata.path)?;
    *guard.revision().file_pin.lock_unpoisoned() = Some(pin);
    guard.revision().complete(metadata);
    Ok(())
}
