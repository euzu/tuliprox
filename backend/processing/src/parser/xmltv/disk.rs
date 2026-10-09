use super::{
    merge::{normalize_channel_programmes, EPG_DISK_BATCH_SIZE},
    EpgMergeAccumulator, TempFileGuard,
};
use serde::{Deserialize, Serialize};
use shared::{model::EpgChannel, utils::Internable};
use std::{collections::HashMap, io, path::PathBuf, sync::Arc};
use tuliprox_core::utils::{arc_str_serde, with_folded_epg_id};
use tuliprox_repository::{BPlusTree, BPlusTreeUpdate, FlushPolicy};

impl EpgMergeAccumulator {
    /// Drain the accumulator directly into a temp `BPlusTree` on disk. Peak RAM
    /// here is `EPG_DISK_BATCH_SIZE × max_channel_size` — channels never sit
    /// in a `Vec`. The returned `DiskEpgSource` removes the file in its
    /// `Drop`.
    ///
    /// `source_priority` and `source_order` are the per-source values used
    /// for `set_attributes_if_preferred` at merge time. The accumulator
    /// itself does not aggregate them, so the caller — which knows the
    /// original Epg's `priority` and `source_order` — must hand them in.
    ///
    /// # Panics
    ///
    /// Panics if `source_order` exceeds `u32`, which would mean four billion
    /// sources in one import.
    pub fn finish_into_disk(self, path: PathBuf, source_priority: i16, source_order: u32) -> io::Result<DiskEpgSource> {
        // Fresh tree at the temp path. `store` creates the file; the
        // subsequent updater opens it for batched writes.
        BPlusTree::<EpgDiskChannelKey, EpgChannel>::new()
            .store(&path)
            .map_err(|e| io::Error::other(format!("create temp EPG tree: {e}")))?;
        // The temp file exists from here until `DiskEpgSource::new` takes
        // ownership at the end. Any `?` in between would otherwise leak the
        // file because no `Drop` is wired up yet. The local guard covers the
        // fallible window and is disarmed just before we hand the path off.
        let mut temp_guard = TempFileGuard(Some(path.clone()));

        let mut updater = BPlusTreeUpdate::<EpgDiskChannelKey, EpgChannel>::try_new_with_backoff(&path)
            .map_err(|e| io::Error::other(format!("open temp EPG tree: {e}")))?;
        updater.set_flush_policy(FlushPolicy::Batch);

        let EpgMergeAccumulator { attributes, channels, dummy_policies: _ } = self;
        let total = channels.len();
        let mut batch: Vec<(EpgDiskChannelKey, EpgChannel)> = Vec::with_capacity(EPG_DISK_BATCH_SIZE);
        let mut written = 0usize;

        for (_, mut acc) in channels {
            normalize_channel_programmes(&mut acc);
            let folded = with_folded_epg_id(&acc.channel.id, |folded| folded.intern());
            batch.push((
                EpgDiskChannelKey {
                    folded_id: folded,
                    priority: acc.priority,
                    // `usize -> u32` overflow requires > 4 billion sources, which
                    // is not representable in the file format anyway; treat it
                    // as a programmer error rather than a recoverable I/O
                    // failure.
                    source_order: u32::try_from(acc.source_order)
                        .expect("source_order exceeds u32 (4 billion sources in one import)"),
                },
                acc.channel,
            ));
            if batch.len() >= EPG_DISK_BATCH_SIZE {
                flush_batch(&mut updater, &mut batch, &mut written)?;
            }
        }
        flush_batch(&mut updater, &mut batch, &mut written)?;
        updater.commit()?;
        // Reclaim space wasted by priority-override entries (multiple keys per
        // channel). Without compact, the file is ~2× its eventual size.
        updater.compact()?;

        log::debug!(
            "Drained {written} channels ({total} total before normalize) into temp EPG tree at {}",
            path.display()
        );

        let source = DiskEpgSource::new(path, attributes.map(|a| a.attributes), source_priority, source_order);
        // DiskEpgSource now owns the path; disarm the guard so its Drop does
        // not double-remove the file. `mem::forget` would also work, but
        // `take()` keeps the guard in scope and is auditable.
        temp_guard.0.take();
        Ok(source)
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            if let Err(err) = std::fs::remove_file(&path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("Failed to remove temp EPG tree at {}: {err}", path.display());
                }
            }
        }
    }
}

/// Flush a non-empty batch and bump the written counter. No-op for empty
/// batches, so the caller does not need a guard around the trailing flush.
fn flush_batch(
    updater: &mut BPlusTreeUpdate<EpgDiskChannelKey, EpgChannel>,
    batch: &mut Vec<(EpgDiskChannelKey, EpgChannel)>,
    written: &mut usize,
) -> io::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let items: Vec<(&EpgDiskChannelKey, &EpgChannel)> = batch.iter().map(|(k, v)| (k, v)).collect();
    let prepared = BPlusTreeUpdate::<EpgDiskChannelKey, EpgChannel>::prepare_upsert_batch(&items)?;
    updater.upsert_batch_encoded(prepared)?;
    *written += batch.len();
    batch.clear();
    Ok(())
}

/// Sort key for the per-source temp `BPlusTree`. Order: folded channel id ascending,
/// then priority ascending, then source order ascending. This matches the existing
/// rule in `EpgMergeAccumulator::upsert_channel` (`(priority, source_order) < (acc.priority, acc.source_order)`)
/// — lower numbers win. Sorted iteration over the tree therefore yields channels in
/// the right order for the multi-way merge downstream.
///
/// The `Ord` implementation below is hand-written to make the value-based,
/// deterministic ordering explicit (`folded_id` → `priority` → `source_order`).
/// `Arc<str>` already compares by value when the inner `str` is `Ord`, so a
/// derived `Ord` would produce the same total order — the custom impl exists
/// to lock the contract in source rather than rely on a derived behaviour.
/// Same key type serialises through `rmp_serde` as a record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EpgDiskChannelKey {
    #[serde(with = "arc_str_serde")]
    pub folded_id: Arc<str>,
    pub priority: i16,
    pub source_order: u32,
}

// Explicit value-based ordering. `#[derive(Ord)]` would derive the same total
// order via `Arc<str>`'s value-based `Ord` impl, but writing it out documents
// the contract: `folded_id` ascending, then `priority` ascending, then
// `source_order` ascending. Sorted iteration in `merge_epg_trees` depends on
// this exact order for deterministic multi-way merge results.
impl Ord for EpgDiskChannelKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.folded_id
            .as_ref()
            .cmp(other.folded_id.as_ref())
            .then(self.priority.cmp(&other.priority))
            .then(self.source_order.cmp(&other.source_order))
    }
}

impl PartialOrd for EpgDiskChannelKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}

/// Handle for a temp `BPlusTree` that holds the channels of one EPG source.
/// `Drop` removes the file — even on panic — so the temp dir never accumulates
/// stale trees. The caller is expected to hand ownership to `merge_epg_trees`,
/// which opens a query handle and drains the file before letting the guard drop.
///
/// `source_priority` and `source_order` are the per-source values used at
/// merge time by `set_attributes_if_preferred`. They live on the source
/// rather than being reconstructed from the disk tree, because the disk
/// tree only stores per-channel priorities (which can differ across
/// channels if sources overlap), not a single source-level value.
pub struct DiskEpgSource {
    pub(in crate::parser) path: PathBuf,
    pub(in crate::parser) attributes: Option<HashMap<Arc<str>, Arc<str>>>,
    pub(in crate::parser) source_priority: i16,
    pub(in crate::parser) source_order: u32,
}

impl DiskEpgSource {
    pub fn new(
        path: PathBuf,
        attributes: Option<HashMap<Arc<str>, Arc<str>>>,
        source_priority: i16,
        source_order: u32,
    ) -> Self {
        Self { path, attributes, source_priority, source_order }
    }
}

impl Drop for DiskEpgSource {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path) {
            // Missing-file is fine (already cleaned up). Anything else is worth
            // a warning — it leaks the temp file until the OS clears /tmp.
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!("Failed to remove temp EPG tree at {}: {err}", self.path.display());
            }
        }
    }
}
