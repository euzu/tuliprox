//! Byte counters shared between a stream and the event manager that reports it.
//!
//! `StreamMeterHandle` is the counter a metering stream writes into and the
//! event manager reads; `MeterReading` is one sample of it. Neither touches a
//! stream type, which is what lets the event manager use them without pulling in
//! the streaming layer.

use crate::api::model::EventManager;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering}, Weak,
    },
    time::Instant,
};

/// Shared per-source meter state. The stream hot path only performs relaxed byte counting.
#[derive(Debug)]
pub struct StreamMeterHandle {
    meter_uid: u32,
    bytes_total: AtomicU64,
    bytes_window: AtomicU64,
    attached: AtomicBool,
    event_manager: Weak<EventManager>,
    created_at: Instant,
    /// Elapsed nanos from `created_at` when the first byte was received.
    /// 0 means no bytes received yet.
    first_byte_elapsed_nanos: AtomicU64,
}

impl StreamMeterHandle {
    pub fn new(meter_uid: u32, event_manager: Weak<EventManager>) -> Self {
        Self {
            meter_uid,
            bytes_total: AtomicU64::new(0),
            bytes_window: AtomicU64::new(0),
            attached: AtomicBool::new(false),
            event_manager,
            created_at: Instant::now(),
            first_byte_elapsed_nanos: AtomicU64::new(0),
        }
    }

    pub fn meter_uid(&self) -> u32 { self.meter_uid }
    pub fn bytes_total(&self) -> u64 { self.bytes_total.load(Ordering::Relaxed) }

    /// Returns the first-byte latency in milliseconds, or `None` if no bytes were received.
    pub fn first_byte_latency_ms(&self) -> Option<u64> {
        let nanos = self.first_byte_elapsed_nanos.load(Ordering::Relaxed);
        if nanos == 0 { None } else { Some(nanos / 1_000_000) }
    }

    pub fn mark_attached(&self) { self.attached.store(true, Ordering::Release); }

    pub fn record_bytes(&self, len: u64) {
        self.bytes_total.fetch_add(len, Ordering::Relaxed);
        self.bytes_window.fetch_add(len, Ordering::Relaxed);
        // Record first-byte timestamp once. After the first CAS succeeds, the load
        // short-circuits — costs one relaxed load (~1ns) per subsequent chunk.
        if self.first_byte_elapsed_nanos.load(Ordering::Relaxed) == 0 {
            let elapsed = u64::try_from(self.created_at.elapsed().as_nanos()).unwrap_or(0);
            let _ = self.first_byte_elapsed_nanos.compare_exchange(0, elapsed.max(1), Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> MeterReading {
        MeterReading {
            meter_uid: self.meter_uid,
            bytes_total: self.bytes_total.load(Ordering::Relaxed),
            bytes_window: self.bytes_window.swap(0, Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterReading {
    pub meter_uid: u32,
    pub bytes_total: u64,
    pub bytes_window: u64,
}

impl Drop for StreamMeterHandle {
    fn drop(&mut self) {
        if self.attached.load(Ordering::Acquire) || self.meter_uid == 0 {
            return;
        }

        let Some(event_manager) = self.event_manager.upgrade() else {
            return;
        };

        let meter_uid = self.meter_uid;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                event_manager.flush_and_unregister_meter(meter_uid).await;
            });
        }
    }
}
