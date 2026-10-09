use crate::sync_ext::MutexExt;
use bytes::Bytes;
use shared::model::Bytes as ConfigBytes;
use std::{
    io,
    sync::{Arc, Mutex},
};
use tuliprox_core::model::HlsStartupConfig;

/// Fixed budget charge for one claimed progressive slot.
pub(crate) const PROGRESSIVE_SLOT_FIXED_BYTES: usize = 512;
/// Allocator bookkeeping charged on top of every replay chunk.
const PROGRESSIVE_CHUNK_OVERHEAD_BYTES: usize = std::mem::size_of::<BudgetedChunk>() + 64;

fn exceeds(bytes: usize, limit: ConfigBytes) -> bool { !u64::try_from(bytes).is_ok_and(|bytes| bytes <= limit.get()) }

#[derive(Debug)]
struct Usage {
    slots: usize,
    bytes: usize,
    repairs: usize,
    limits: HlsStartupConfig,
}

#[derive(Debug)]
pub struct ProgressiveBudgetManager {
    usage: Mutex<Usage>,
    changed: tokio::sync::Notify,
}

impl ProgressiveBudgetManager {
    pub fn new(limits: HlsStartupConfig) -> Arc<Self> {
        Arc::new(Self {
            usage: Mutex::new(Usage { slots: 0, bytes: 0, repairs: 0, limits }),
            changed: tokio::sync::Notify::new(),
        })
    }

    pub fn update_limits(&self, limits: HlsStartupConfig) {
        self.usage.lock_unpoisoned().limits = limits;
        self.changed.notify_waiters();
    }

    pub fn try_claim(self: &Arc<Self>) -> Option<ProgressiveSlot> {
        let mut usage = self.usage.lock_unpoisoned();
        if usage.slots >= usage.limits.max_progressive_segments {
            return None;
        }
        let fixed = PROGRESSIVE_SLOT_FIXED_BYTES;
        if exceeds(usage.bytes.saturating_add(fixed), usage.limits.max_progressive_bytes_total) {
            return None;
        }
        usage.slots += 1;
        usage.bytes += fixed;
        Some(ProgressiveSlot { manager: Arc::clone(self), bytes: fixed, fixed, metadata_bytes: 0 })
    }

    pub async fn wait_for_repair(self: &Arc<Self>, deadline: tokio::time::Instant) -> io::Result<DeferredRepairPermit> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let available = {
                let mut usage = self.usage.lock_unpoisoned();
                if usage.repairs < usage.limits.max_deferred_repairs {
                    usage.repairs += 1;
                    true
                } else {
                    false
                }
            };
            if available {
                return Ok(DeferredRepairPermit { manager: Arc::clone(self) });
            }
            tokio::time::timeout_at(deadline, changed)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "deferred HLS repair capacity timed out"))?;
        }
    }

    pub fn usage_snapshot(&self) -> (usize, usize, usize) {
        let usage = self.usage.lock_unpoisoned();
        (usage.slots, usage.bytes, usage.repairs)
    }

    pub fn usage(&self) -> (usize, usize) {
        let usage = self.usage.lock_unpoisoned();
        (usage.slots, usage.bytes)
    }
}

pub struct ProgressiveSlot {
    fixed: usize,
    manager: Arc<ProgressiveBudgetManager>,
    bytes: usize,
    metadata_bytes: usize,
}

impl ProgressiveSlot {
    pub fn copy_chunk(&mut self, data: &[u8]) -> io::Result<Bytes> {
        let charge = data
            .len()
            .checked_add(PROGRESSIVE_CHUNK_OVERHEAD_BYTES)
            .ok_or_else(|| io::Error::other("progressive chunk size overflow"))?;
        self.reserve_bytes(charge)?;
        let mut data_copy = Vec::new();
        if let Err(error) = data_copy.try_reserve_exact(data.len()) {
            self.release_failed_reservation(charge);
            return Err(io::Error::other(error));
        }
        let excess = data_copy.capacity().saturating_sub(data.len());
        if excess > 0 {
            if let Err(error) = self.reserve_bytes(excess) {
                self.release_failed_reservation(charge);
                return Err(error);
            }
        }
        data_copy.extend_from_slice(data);
        Ok(Bytes::from_owner(BudgetedChunk {
            data: data_copy,
            charge: charge + excess,
            manager: Arc::clone(&self.manager),
        }))
    }

    fn reserve_bytes(&mut self, charge: usize) -> io::Result<()> {
        let mut usage = self.manager.usage.lock_unpoisoned();
        let next_segment =
            self.bytes.checked_add(charge).ok_or_else(|| io::Error::other("progressive segment budget overflow"))?;
        let next_total =
            usage.bytes.checked_add(charge).ok_or_else(|| io::Error::other("progressive byte budget overflow"))?;
        if exceeds(next_segment, usage.limits.max_progressive_bytes_per_segment)
            || exceeds(next_total, usage.limits.max_progressive_bytes_total)
        {
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, "progressive byte budget exhausted"));
        }
        usage.bytes = next_total;
        self.bytes = next_segment;
        Ok(())
    }

    pub fn reserve_metadata(&mut self, bytes: usize) -> io::Result<()> {
        self.reserve_bytes(bytes)?;
        self.metadata_bytes += bytes;
        Ok(())
    }

    pub fn release_metadata(&mut self, bytes: usize) {
        self.release_failed_reservation(bytes);
        self.metadata_bytes = self.metadata_bytes.saturating_sub(bytes);
    }

    fn release_failed_reservation(&mut self, charge: usize) {
        let mut usage = self.manager.usage.lock_unpoisoned();
        usage.bytes = usage.bytes.saturating_sub(charge);
        self.bytes = self.bytes.saturating_sub(charge);
    }
}

impl Drop for ProgressiveSlot {
    fn drop(&mut self) {
        let mut usage = self.manager.usage.lock_unpoisoned();
        usage.slots = usage.slots.saturating_sub(1);
        usage.bytes = usage.bytes.saturating_sub(self.fixed.saturating_add(self.metadata_bytes));
    }
}

struct BudgetedChunk {
    data: Vec<u8>,
    charge: usize,
    manager: Arc<ProgressiveBudgetManager>,
}
impl AsRef<[u8]> for BudgetedChunk {
    fn as_ref(&self) -> &[u8] { &self.data }
}
impl Drop for BudgetedChunk {
    fn drop(&mut self) {
        drop(std::mem::take(&mut self.data));
        let mut usage = self.manager.usage.lock_unpoisoned();
        usage.bytes = usage.bytes.saturating_sub(self.charge);
    }
}

pub struct DeferredRepairPermit {
    manager: Arc<ProgressiveBudgetManager>,
}
impl Drop for DeferredRepairPermit {
    fn drop(&mut self) {
        let mut usage = self.manager.usage.lock_unpoisoned();
        usage.repairs = usage.repairs.saturating_sub(1);
        drop(usage);
        self.manager.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_and_slices_keep_the_allocation_charged_after_slot_release() -> io::Result<()> {
        let manager = ProgressiveBudgetManager::new(HlsStartupConfig::default());
        let mut slot = manager.try_claim().ok_or_else(|| io::Error::other("slot"))?;
        let chunk = slot.copy_chunk(&[1; 188])?;
        let slice = chunk.slice(1..3);
        let charged = manager.usage().1;
        drop(chunk);
        drop(slot);
        assert_eq!(manager.usage(), (0, charged.saturating_sub(PROGRESSIVE_SLOT_FIXED_BYTES)));
        drop(slice);
        assert_eq!(manager.usage(), (0, 0));
        Ok(())
    }

    #[tokio::test]
    async fn deferred_repair_waiters_wake_on_release_and_reload() -> io::Result<()> {
        let mut limits = HlsStartupConfig { max_deferred_repairs: 1, ..Default::default() };
        let manager = ProgressiveBudgetManager::new(limits.clone());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let first = manager.wait_for_repair(deadline).await?;
        let waiting_manager = Arc::clone(&manager);
        let waiting = tokio::spawn(async move { waiting_manager.wait_for_repair(deadline).await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(first);
        let second = waiting.await.map_err(io::Error::other)??;
        let waiting_manager = Arc::clone(&manager);
        let waiting = tokio::spawn(async move { waiting_manager.wait_for_repair(deadline).await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        limits.max_deferred_repairs = 2;
        manager.update_limits(limits);
        let third = waiting.await.map_err(io::Error::other)??;
        drop(second);
        drop(third);
        assert_eq!(manager.usage.lock().map_err(|_| io::Error::other("budget lock"))?.repairs, 0);
        Ok(())
    }

    #[test]
    fn total_limit_and_reload_apply_to_all_slots() -> io::Result<()> {
        let mut limits = HlsStartupConfig {
            max_progressive_bytes_total: ConfigBytes::new(4096),
            max_progressive_bytes_per_segment: ConfigBytes::new(4096),
            ..Default::default()
        };
        let manager = ProgressiveBudgetManager::new(limits.clone());
        let mut first = manager.try_claim().ok_or_else(|| io::Error::other("slot"))?;
        let mut second = manager.try_claim().ok_or_else(|| io::Error::other("slot"))?;
        let retained = first.copy_chunk(&[0; 2000])?;
        assert!(second.copy_chunk(&[0; 2000]).is_err());
        limits.max_progressive_bytes_total = ConfigBytes::new(1);
        limits.max_progressive_segments = 1;
        manager.update_limits(limits);
        assert!(manager.try_claim().is_none());
        assert!(first.copy_chunk(&[0]).is_err());
        drop(retained);
        drop(first);
        drop(second);
        assert_eq!(manager.usage(), (0, 0));
        Ok(())
    }
}
