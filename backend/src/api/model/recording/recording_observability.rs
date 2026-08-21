//! Recording observability counters.
//!
//! - Add counters for recording
//!   create / start / complete / fail / delete, persistence
//!   failure, unsafe path rejection, retention cleanup, and
//!   notification attempt / failure.
//! - Add queue revision and opaque task id to diagnostic logs
//!   where useful.
//! - Do not label metrics with user IDs, titles, channels,
//!   filenames, or rule IDs.
//! - Use structured failure categories rather than raw private
//!   metadata.
//! - Add tests or review assertions for user-visible logs and
//!   conflict responses.
//!
//! This module owns the counter shape and the
//! increment / snapshot helpers. The metric sink is the
//! caller's responsibility; the counters themselves are pure
//! atomic integers.


use std::sync::atomic::{AtomicU64, Ordering};

/// The recording counter set. Each variant maps to a single atomic
/// counter. The enum's wire name is the metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counter {
    Create,
    Start,
    Complete,
    Fail,
    Delete,
    PersistenceFailure,
    UnsafePathRejection,
    RetentionCleanup,
    NotificationAttempt,
    NotificationFailure,
}

impl Counter {
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Create => "recording_create_total",
            Self::Start => "recording_start_total",
            Self::Complete => "recording_complete_total",
            Self::Fail => "recording_fail_total",
            Self::Delete => "recording_delete_total",
            Self::PersistenceFailure => "recording_persistence_failure_total",
            Self::UnsafePathRejection => "recording_unsafe_path_rejection_total",
            Self::RetentionCleanup => "recording_retention_cleanup_total",
            Self::NotificationAttempt => "recording_notification_attempt_total",
            Self::NotificationFailure => "recording_notification_failure_total",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Create => 0,
            Self::Start => 1,
            Self::Complete => 2,
            Self::Fail => 3,
            Self::Delete => 4,
            Self::PersistenceFailure => 5,
            Self::UnsafePathRejection => 6,
            Self::RetentionCleanup => 7,
            Self::NotificationAttempt => 8,
            Self::NotificationFailure => 9,
        }
    }
}

/// A snapshot of the counter set. The metric sink serializes
/// this as a flat list of `(name, value)` pairs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CounterSnapshot {
    pub create: u64,
    pub start: u64,
    pub complete: u64,
    pub fail: u64,
    pub delete: u64,
    pub persistence_failure: u64,
    pub unsafe_path_rejection: u64,
    pub retention_cleanup: u64,
    pub notification_attempt: u64,
    pub notification_failure: u64,
}

impl CounterSnapshot {
    pub fn as_pairs(&self) -> Vec<(&'static str, u64)> {
        vec![
            ("recording_create_total", self.create),
            ("recording_start_total", self.start),
            ("recording_complete_total", self.complete),
            ("recording_fail_total", self.fail),
            ("recording_delete_total", self.delete),
            ("recording_persistence_failure_total", self.persistence_failure),
            ("recording_unsafe_path_rejection_total", self.unsafe_path_rejection),
            ("recording_retention_cleanup_total", self.retention_cleanup),
            ("recording_notification_attempt_total", self.notification_attempt),
            ("recording_notification_failure_total", self.notification_failure),
        ]
    }
}

/// The atomic counter store. The metric sink increments
/// counters via `inc`; the rest of the codebase reads via
/// `snapshot`.
pub struct Counters {
    inner: [AtomicU64; 10],
}

impl Counters {
    pub const fn new() -> Self {
        Self {
            inner: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }

    pub fn inc(&self, counter: Counter) {
        self.inner[counter.index()].fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            create: self.inner[0].load(Ordering::Relaxed),
            start: self.inner[1].load(Ordering::Relaxed),
            complete: self.inner[2].load(Ordering::Relaxed),
            fail: self.inner[3].load(Ordering::Relaxed),
            delete: self.inner[4].load(Ordering::Relaxed),
            persistence_failure: self.inner[5].load(Ordering::Relaxed),
            unsafe_path_rejection: self.inner[6].load(Ordering::Relaxed),
            retention_cleanup: self.inner[7].load(Ordering::Relaxed),
            notification_attempt: self.inner[8].load(Ordering::Relaxed),
            notification_failure: self.inner[9].load(Ordering::Relaxed),
        }
    }
}

impl Default for Counters {
    fn default() -> Self { Self::new() }
}

/// The structured failure category the log redaction uses
/// instead of raw private metadata. Wire-shape stable; never
/// carries user ids, titles, channels, filenames, or rule ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCategory {
    InvalidSource,
    PathTraversal,
    SymlinkSwap,
    StaleClaim,
    ForgedVisibility,
    PersistenceFailed,
    QuotaExceeded,
    InsufficientDisk,
    SourceTampered,
    ForeignPrivateAccess,
    EventLeakage,
}

impl FailureCategory {
    pub fn wire(self) -> &'static str {
        match self {
            Self::InvalidSource => "recording_failure_invalid_source",
            Self::PathTraversal => "recording_failure_path_traversal",
            Self::SymlinkSwap => "recording_failure_symlink_swap",
            Self::StaleClaim => "recording_failure_stale_claim",
            Self::ForgedVisibility => "recording_failure_forged_visibility",
            Self::PersistenceFailed => "recording_failure_persistence",
            Self::QuotaExceeded => "recording_failure_quota",
            Self::InsufficientDisk => "recording_failure_disk",
            Self::SourceTampered => "recording_failure_source_tampered",
            Self::ForeignPrivateAccess => "recording_failure_foreign_private",
            Self::EventLeakage => "recording_failure_event_leakage",
        }
    }
}

/// A redacted log entry. Carries the queue revision and opaque
/// task id, never the private fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedactedLog {
    pub queue_revision: Option<u64>,
    pub opaque_task_id: Option<String>,
    pub category: Option<FailureCategory>,
    pub note: Option<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_wire_names_are_stable() {
        assert_eq!(Counter::Create.wire_name(), "recording_create_total");
        assert_eq!(Counter::Start.wire_name(), "recording_start_total");
        assert_eq!(Counter::Complete.wire_name(), "recording_complete_total");
        assert_eq!(Counter::Fail.wire_name(), "recording_fail_total");
        assert_eq!(Counter::Delete.wire_name(), "recording_delete_total");
        assert_eq!(Counter::PersistenceFailure.wire_name(), "recording_persistence_failure_total");
        assert_eq!(
            Counter::UnsafePathRejection.wire_name(),
            "recording_unsafe_path_rejection_total"
        );
        assert_eq!(Counter::RetentionCleanup.wire_name(), "recording_retention_cleanup_total");
        assert_eq!(
            Counter::NotificationAttempt.wire_name(),
            "recording_notification_attempt_total"
        );
        assert_eq!(
            Counter::NotificationFailure.wire_name(),
            "recording_notification_failure_total"
        );
    }

    #[test]
    fn counters_increment() {
        let c = Counters::new();
        c.inc(Counter::Create);
        c.inc(Counter::Create);
        c.inc(Counter::Start);
        let s = c.snapshot();
        assert_eq!(s.create, 2);
        assert_eq!(s.start, 1);
        assert_eq!(s.complete, 0);
    }

    #[test]
    fn snapshot_pairs_include_all_counters() {
        let c = Counters::new();
        let s = c.snapshot();
        let pairs = s.as_pairs();
        assert_eq!(pairs.len(), 10);
    }

    #[test]
    fn failure_category_wire_names_are_stable() {
        assert_eq!(FailureCategory::PathTraversal.wire(), "recording_failure_path_traversal");
        assert_eq!(FailureCategory::EventLeakage.wire(), "recording_failure_event_leakage");
    }

    #[test]
    fn redacted_log_default_is_empty() {
        let l = RedactedLog::default();
        assert!(l.queue_revision.is_none());
        assert!(l.opaque_task_id.is_none());
        assert!(l.category.is_none());
    }
}
