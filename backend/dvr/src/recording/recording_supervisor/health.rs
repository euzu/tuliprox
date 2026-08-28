//! Supervisor health tracking.
//!
//! Last-tick timestamps and counters, so an operator can tell a healthy
//! supervisor from one that died without reading the log.
//!
//! The health struct lives in `OnceLock` so the health endpoint does not
//! need a handle threaded through `AppState` (which is rebuilt on every
//! config reload, whereas the supervisors outlive one).

use std::sync::{
    atomic::{AtomicI64, Ordering},
    OnceLock,
};

/// Last-tick timestamps, so an operator can tell a healthy supervisor
/// from one that died. Read by the health endpoint; written by the
/// supervisors themselves.
///
/// The atomic fields are `pub(super)` rather than private because the
/// supervisor submodules are the only legitimate writers and they stamp
/// them via [`SupervisorHealth::stamp`]; hiding them behind setters
/// would force every caller to invent its own method, which is the
/// opposite of "every caller stamps the same way".
#[derive(Debug, Default)]
pub struct SupervisorHealth {
    pub(super) reconciliation_last_run: AtomicI64,
    pub(super) retention_last_tick: AtomicI64,
    pub(super) notification_last_drain: AtomicI64,
    pub(super) notification_outbox_depth: AtomicI64,
    pub(super) notification_dead_lettered: AtomicI64,
}

impl SupervisorHealth {
    pub fn stamp(field: &AtomicI64, now: i64) {
        field.store(now, Ordering::Relaxed);
    }

    pub fn reconciliation_last_run(&self) -> Option<i64> {
        non_zero(self.reconciliation_last_run.load(Ordering::Relaxed))
    }
    pub fn retention_last_tick(&self) -> Option<i64> {
        non_zero(self.retention_last_tick.load(Ordering::Relaxed))
    }
    pub fn notification_last_drain(&self) -> Option<i64> {
        non_zero(self.notification_last_drain.load(Ordering::Relaxed))
    }
    pub fn notification_outbox_depth(&self) -> i64 {
        self.notification_outbox_depth.load(Ordering::Relaxed)
    }
    pub fn notification_dead_lettered(&self) -> i64 {
        self.notification_dead_lettered.load(Ordering::Relaxed)
    }
}

fn non_zero(value: i64) -> Option<i64> {
    (value != 0).then_some(value)
}

/// Process-wide health, so the health endpoint does not need a handle
/// threaded through `AppState` (which is rebuilt on every config
/// reload, whereas the supervisors outlive one).
pub fn supervisor_health() -> &'static SupervisorHealth {
    static HEALTH: OnceLock<SupervisorHealth> = OnceLock::new();
    HEALTH.get_or_init(SupervisorHealth::default)
}
