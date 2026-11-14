//! Member-health triggers of a failover-group HLS session.
//!
//! The HLS workers classify what they see (manifest refresh outcome, segment fetch outcome,
//! publication progress, download speed) and call into this module while they hold the session
//! lock. The functions here only decide whether a signal is serious enough to report; they return a
//! [`HlsMemberUnhealthyNotice`] that the caller sends after it dropped the lock.
//! [`HlsOriginAccountRuntime::notify_member_unhealthy`] then applies the app-owned budget, hysteresis
//! and member selection. Sessions whose runtime reports no [`HlsMemberHealthPolicy`] (single items,
//! failover disabled) never produce a notice.

use crate::{
    origin_account_runtime::{HlsMemberHealthPolicy, HlsMemberUnhealthyReason, HlsOriginAccountRuntime},
    HlsSession, HlsSessionHandle,
};
use std::{collections::VecDeque, sync::Arc};

/// Hard manifest failures in a row that report the member without waiting for the no-data timeout.
const HARD_MANIFEST_FAILURES_BEFORE_REPORT: u32 = 2;
/// Segment downloads needed inside the window before the rate is judged.
const MIN_SLOW_SAMPLES: usize = 3;
const MAX_SLOW_SAMPLES: usize = 64;

/// A member-unhealthy signal ready to be sent once the session lock is released.
pub(crate) struct HlsMemberUnhealthyNotice {
    runtime: Arc<dyn HlsOriginAccountRuntime>,
    pub(crate) reason: HlsMemberUnhealthyReason,
}

impl HlsMemberUnhealthyNotice {
    /// Non-blocking: the runtime only spawns the handoff.
    pub(crate) fn send(self, session: &HlsSessionHandle) { self.runtime.notify_member_unhealthy(session, self.reason); }
}

#[derive(Debug, Clone, Copy)]
#[expect(clippy::struct_field_names, reason = "Every field is a millisecond quantity.")]
struct DeliverySample {
    at_ms: u64,
    media_ms: u64,
    download_ms: u64,
}

/// Segment download rate over a sliding time window: media duration delivered per download time.
#[derive(Debug, Default)]
pub(crate) struct HlsSlowDeliveryMeter {
    samples: VecDeque<DeliverySample>,
}

impl HlsSlowDeliveryMeter {
    pub(crate) fn record(&mut self, at_ms: u64, media_ms: u64, download_ms: u64, window_ms: u64) {
        // Eviction is robust against wall-clock steps: samples timestamped in the future (after a
        // forward step) are dropped together with those older than the window, so a backward step
        // cannot pin a stale sample at the front by making the subtraction saturate to zero.
        while self
            .samples
            .front()
            .is_some_and(|sample| at_ms < sample.at_ms || at_ms.saturating_sub(sample.at_ms) > window_ms)
            || self.samples.len() >= MAX_SLOW_SAMPLES
        {
            self.samples.pop_front();
        }
        self.samples.push_back(DeliverySample { at_ms, media_ms, download_ms: download_ms.max(1) });
    }

    /// True when at least [`MIN_SLOW_SAMPLES`] downloads in the window delivered less than `pct`
    /// percent of real time.
    pub(crate) fn is_slow(&self, pct: u8) -> bool {
        if self.samples.len() < MIN_SLOW_SAMPLES {
            return false;
        }
        let (media_ms, download_ms) = self.samples.iter().fold((0_u128, 0_u128), |(media, download), sample| {
            (media + u128::from(sample.media_ms), download + u128::from(sample.download_ms))
        });
        media_ms * 100 < download_ms * u128::from(pct)
    }

    fn clear(&mut self) { self.samples.clear(); }
}

/// Per-session state behind the member-health triggers. Reset whenever the bound member changes.
#[derive(Debug, Default)]
pub(crate) struct HlsMemberHealth {
    /// When the current member was bound; media progress of an earlier member does not count.
    epoch_start_ms: u64,
    first_manifest_failure_ms: Option<u64>,
    manifest_failures: u32,
    hard_manifest_failures: u32,
    last_stall_report_ms: Option<u64>,
    slow: HlsSlowDeliveryMeter,
}

impl HlsMemberHealth {
    pub(crate) fn reset_for_handoff(&mut self, now_ms: u64) {
        *self = Self { epoch_start_ms: now_ms, ..Self::default() };
    }

    pub(crate) fn manifest_succeeded(&mut self) {
        self.first_manifest_failure_ms = None;
        self.manifest_failures = 0;
        self.hard_manifest_failures = 0;
    }

    /// Records one failed refresh. Reports on repeated hard failures, or on repeated failures that
    /// lasted `no_data_timeout_ms` without a successful refresh in between. A single failure never
    /// reports.
    pub(crate) fn manifest_failed(&mut self, hard: bool, now_ms: u64, no_data_timeout_ms: u64) -> bool {
        let first = *self.first_manifest_failure_ms.get_or_insert(now_ms);
        self.manifest_failures = self.manifest_failures.saturating_add(1);
        if hard {
            self.hard_manifest_failures = self.hard_manifest_failures.saturating_add(1);
        }
        self.hard_manifest_failures >= HARD_MANIFEST_FAILURES_BEFORE_REPORT
            || (no_data_timeout_ms > 0
                && self.manifest_failures >= 2
                && now_ms.saturating_sub(first) >= no_data_timeout_ms)
    }

    /// No new media for `max(no_data_timeout, 2 x target duration)`. Reports again after every further
    /// interval of that length while the stall lasts.
    pub(crate) fn stalled(
        &mut self,
        now_ms: u64,
        last_progress_ms: u64,
        target_duration_ms: u64,
        no_data_timeout_ms: u64,
    ) -> bool {
        let Some(due_at_ms) = self.next_stall_due_at_ms(last_progress_ms, target_duration_ms, no_data_timeout_ms)
        else {
            return false;
        };
        if now_ms < due_at_ms {
            return false;
        }
        self.last_stall_report_ms = Some(now_ms);
        true
    }

    /// Next wall-clock instant at which [`Self::stalled`] would report, if the member stays stalled.
    /// `None` when the no-data timeout is disabled. Mirrors [`Self::stalled`] so the periodic
    /// evaluation can schedule exactly one tick instead of polling.
    pub(crate) fn next_stall_due_at_ms(
        &self,
        last_progress_ms: u64,
        target_duration_ms: u64,
        no_data_timeout_ms: u64,
    ) -> Option<u64> {
        if no_data_timeout_ms == 0 {
            return None;
        }
        let threshold = no_data_timeout_ms.max(target_duration_ms.saturating_mul(2));
        let since = last_progress_ms.max(self.epoch_start_ms).max(self.last_stall_report_ms.unwrap_or(0));
        Some(since.saturating_add(threshold))
    }

    /// Records one segment download. Reports when the window is slow; the window restarts afterwards
    /// so the next report needs fresh evidence.
    pub(crate) fn segment_delivered(
        &mut self,
        now_ms: u64,
        media_ms: u64,
        download_ms: u64,
        policy: HlsMemberHealthPolicy,
    ) -> bool {
        if policy.slow_source_pct == 0 || policy.slow_source_window_ms == 0 || media_ms == 0 {
            return false;
        }
        self.slow.record(now_ms, media_ms, download_ms, policy.slow_source_window_ms);
        let slow = self.slow.is_slow(policy.slow_source_pct);
        if slow {
            self.slow.clear();
        }
        slow
    }
}

impl HlsSession {
    fn member_health_policy(&self) -> Option<(Arc<dyn HlsOriginAccountRuntime>, HlsMemberHealthPolicy)> {
        let runtime = self.origin_account_runtime.clone()?;
        let policy = runtime.member_health_policy(self)?;
        Some((runtime, policy))
    }

    /// A manifest refresh committed. Also the evaluation point for a stalled publication.
    pub(crate) fn member_unhealthy_after_manifest_success(&mut self, now_ms: u64) -> Option<HlsMemberUnhealthyNotice> {
        let (runtime, policy) = self.member_health_policy()?;
        self.member_health.manifest_succeeded();
        self.member_stall_notice(runtime, policy, now_ms)
    }

    /// A manifest refresh failed. `hard` is the failure class from the refresh classifier; the caller
    /// passes `None` for failures that say nothing about the member's origin.
    pub(crate) fn member_unhealthy_after_manifest_failure(
        &mut self,
        hard: Option<bool>,
        now_ms: u64,
    ) -> Option<HlsMemberUnhealthyNotice> {
        let hard = hard?;
        let (runtime, policy) = self.member_health_policy()?;
        if self.member_health.manifest_failed(hard, now_ms, policy.no_data_timeout_ms) {
            return Some(HlsMemberUnhealthyNotice { runtime, reason: HlsMemberUnhealthyReason::ManifestFailure });
        }
        self.member_stall_notice(runtime, policy, now_ms)
    }

    fn member_stall_notice(
        &mut self,
        runtime: Arc<dyn HlsOriginAccountRuntime>,
        policy: HlsMemberHealthPolicy,
        now_ms: u64,
    ) -> Option<HlsMemberUnhealthyNotice> {
        let last_progress_ms = self.origin_control.last_media_progress_at_ms?;
        let target_duration_ms = self.origin_control.target_duration_snapshot_ms.unwrap_or_default();
        self.member_health
            .stalled(now_ms, last_progress_ms, target_duration_ms, policy.no_data_timeout_ms)
            .then_some(HlsMemberUnhealthyNotice { runtime, reason: HlsMemberUnhealthyReason::Stall })
    }

    /// A segment or init map failed after its own retries.
    pub(crate) fn member_unhealthy_after_media_failure(
        &self,
        error: &crate::HlsOriginResourceFetchError,
    ) -> Option<HlsMemberUnhealthyNotice> {
        if !error.is_origin_delivery_failure() {
            return None;
        }
        let (runtime, _) = self.member_health_policy()?;
        Some(HlsMemberUnhealthyNotice { runtime, reason: HlsMemberUnhealthyReason::SegmentFailure })
    }

    /// Periodic stall evaluation: reports a stalled member without any manifest refresh having run.
    /// The lifecycle manager fires this on its own per-session timer while the session is leased.
    pub(crate) fn member_unhealthy_on_stall_timer(&mut self, now_ms: u64) -> Option<HlsMemberUnhealthyNotice> {
        let (runtime, policy) = self.member_health_policy()?;
        self.member_stall_notice(runtime, policy, now_ms)
    }

    /// Next instant at which the periodic stall evaluation must run, if one is due at all.
    pub(crate) fn member_health_stall_due_at_ms(&self) -> Option<u64> {
        let policy = self.origin_account_runtime.as_ref()?.member_health_policy(self)?;
        let last_progress_ms = self.origin_control.last_media_progress_at_ms?;
        let target_duration_ms = self.origin_control.target_duration_snapshot_ms.unwrap_or_default();
        self.member_health.next_stall_due_at_ms(last_progress_ms, target_duration_ms, policy.no_data_timeout_ms)
    }

    /// A segment became ready after `download_ms` of fetching. Tells the runtime the member plays and
    /// feeds the slow-delivery window.
    pub(crate) fn member_unhealthy_after_segment_ready(
        &mut self,
        now_ms: u64,
        media_ms: u64,
        download_ms: u64,
    ) -> Option<HlsMemberUnhealthyNotice> {
        let (runtime, policy) = self.member_health_policy()?;
        runtime.notify_member_progress(self);
        self.member_health
            .segment_delivered(now_ms, media_ms, download_ms, policy)
            .then_some(HlsMemberUnhealthyNotice { runtime, reason: HlsMemberUnhealthyReason::SlowDelivery })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::{
        origin_account_runtime::{HlsOriginAccountWork, HlsPreparedOriginAccount},
        HlsBoundAccountAcquireErrorKind, HlsOriginWorkClass,
    };
    use std::{
        fmt,
        future::Future,
        pin::Pin,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };

    /// Group-like runtime that records every notification.
    pub(crate) struct RecordingMemberRuntime {
        policy: Option<HlsMemberHealthPolicy>,
        reasons: Mutex<Vec<HlsMemberUnhealthyReason>>,
        progress: AtomicUsize,
    }

    impl fmt::Debug for RecordingMemberRuntime {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("RecordingMemberRuntime") }
    }

    impl RecordingMemberRuntime {
        pub(crate) fn new(policy: Option<HlsMemberHealthPolicy>) -> Arc<Self> {
            Arc::new(Self { policy, reasons: Mutex::new(Vec::new()), progress: AtomicUsize::new(0) })
        }

        pub(crate) fn default_policy() -> HlsMemberHealthPolicy {
            HlsMemberHealthPolicy { no_data_timeout_ms: 8_000, slow_source_pct: 90, slow_source_window_ms: 15_000 }
        }

        pub(crate) fn reasons(&self) -> Vec<HlsMemberUnhealthyReason> {
            self.reasons.lock().map(|reasons| reasons.clone()).unwrap_or_default()
        }

        pub(crate) fn assert_quiet(&self, message: &str) {
            assert_eq!(self.reasons(), Vec::new(), "{message}");
        }

        pub(crate) fn progress(&self) -> usize { self.progress.load(Ordering::Acquire) }
    }

    impl HlsOriginAccountRuntime for RecordingMemberRuntime {
        fn prepare<'a>(
            &'a self,
            _session: &'a HlsSessionHandle,
            _work: HlsOriginAccountWork,
            _class: HlsOriginWorkClass,
        ) -> Pin<Box<dyn Future<Output = Result<HlsPreparedOriginAccount, HlsBoundAccountAcquireErrorKind>> + Send + 'a>>
        {
            Box::pin(async { Err(HlsBoundAccountAcquireErrorKind::Missing) })
        }

        fn notify_member_unhealthy(&self, _session: &HlsSessionHandle, reason: HlsMemberUnhealthyReason) {
            if let Ok(mut reasons) = self.reasons.lock() {
                reasons.push(reason);
            }
        }

        fn member_health_policy(&self, _session: &HlsSession) -> Option<HlsMemberHealthPolicy> { self.policy }

        fn notify_member_progress(&self, _session: &HlsSession) { self.progress.fetch_add(1, Ordering::AcqRel); }
    }

    /// Delivers a notice to the recording runtime the way the workers do after dropping the lock.
    pub(crate) fn deliver(notice: Option<HlsMemberUnhealthyNotice>) {
        if let Some(notice) = notice {
            let handle: HlsSessionHandle = Arc::new(tokio::sync::RwLock::new(HlsSession::new(
                crate::HlsSessionKey::new(1, "member-health-notice"),
                b"secret",
                0,
            )));
            notice.send(&handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::RecordingMemberRuntime, *};
    use crate::{HlsBoundAccountAcquireErrorKind, HlsSessionKey};

    fn session(runtime: &Arc<RecordingMemberRuntime>) -> HlsSession {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "member-health"), b"secret", 0);
        session.origin_account_runtime = Some(runtime.clone());
        session
    }

    fn send(notice: Option<HlsMemberUnhealthyNotice>) { test_support::deliver(notice); }

    #[test]
    fn meter_needs_three_samples_in_the_window() {
        let mut meter = HlsSlowDeliveryMeter::default();
        meter.record(1_000, 4_000, 8_000, 15_000);
        meter.record(2_000, 4_000, 8_000, 15_000);
        assert!(!meter.is_slow(90), "two samples are no decision");
        meter.record(3_000, 4_000, 8_000, 15_000);
        assert!(meter.is_slow(90), "half real time is below 90 percent");
    }

    #[test]
    fn meter_rate_is_aggregated_over_the_window() {
        let mut meter = HlsSlowDeliveryMeter::default();
        // 12 s of media in 12 s of download is exactly real time: 100 percent, not below 90.
        for at in [1_000, 2_000, 3_000] {
            meter.record(at, 4_000, 4_000, 15_000);
        }
        assert!(!meter.is_slow(90));
        // One slow segment among fast ones does not pull the aggregate below the threshold.
        let mut meter = HlsSlowDeliveryMeter::default();
        meter.record(1_000, 4_000, 500, 15_000);
        meter.record(2_000, 4_000, 500, 15_000);
        meter.record(3_000, 4_000, 6_000, 15_000);
        assert!(!meter.is_slow(90));
    }

    #[test]
    fn meter_drops_samples_older_than_the_window() {
        let mut meter = HlsSlowDeliveryMeter::default();
        meter.record(1_000, 4_000, 8_000, 15_000);
        meter.record(2_000, 4_000, 8_000, 15_000);
        meter.record(20_000, 4_000, 8_000, 15_000);
        assert!(!meter.is_slow(90), "only one sample remains inside the window");
    }

    #[test]
    fn meter_disabled_by_zero_percent() {
        let mut meter = HlsSlowDeliveryMeter::default();
        for at in [1_000, 2_000, 3_000] {
            meter.record(at, 4_000, 40_000, 15_000);
        }
        assert!(!meter.is_slow(0));
    }

    #[test]
    fn one_retryable_failure_never_reports_but_two_hard_ones_do() {
        let mut health = HlsMemberHealth::default();
        assert!(!health.manifest_failed(false, 1_000, 8_000));
        assert!(!health.manifest_failed(true, 1_500, 8_000));
        assert!(health.manifest_failed(true, 2_000, 8_000));
        health.manifest_succeeded();
        assert!(!health.manifest_failed(true, 3_000, 8_000), "a successful refresh restarts the count");
    }

    #[test]
    fn retryable_failures_report_after_the_no_data_timeout() {
        let mut health = HlsMemberHealth::default();
        assert!(!health.manifest_failed(false, 1_000, 8_000));
        assert!(!health.manifest_failed(false, 5_000, 8_000));
        assert!(health.manifest_failed(false, 9_000, 8_000));
        let mut disabled = HlsMemberHealth::default();
        assert!(!disabled.manifest_failed(false, 1_000, 0));
        assert!(!disabled.manifest_failed(false, 90_000, 0));
    }

    #[test]
    fn two_quick_retryable_failures_do_not_switch() {
        let mut health = HlsMemberHealth::default();
        assert!(!health.manifest_failed(false, 1_000, 8_000));
        assert!(!health.manifest_failed(false, 1_500, 8_000), "two quick soft failures stay below the no-data timeout");
    }

    #[test]
    fn stall_timer_reports_without_any_manifest_refresh() {
        let runtime = RecordingMemberRuntime::new(Some(RecordingMemberRuntime::default_policy()));
        let mut with_policy = session(&runtime);
        with_policy.origin_control.record_media_progress(10_000, 4_000);
        // No manifest success/failure is evaluated: the timer path alone must report the stall.
        send(with_policy.member_unhealthy_on_stall_timer(18_000));
        assert_eq!(runtime.reasons(), vec![HlsMemberUnhealthyReason::Stall]);
        // A session without a policy stays quiet on the timer path too.
        let plain_runtime = RecordingMemberRuntime::new(None);
        let mut without_policy = session(&plain_runtime);
        without_policy.origin_control.record_media_progress(10_000, 4_000);
        send(without_policy.member_unhealthy_on_stall_timer(90_000));
        plain_runtime.assert_quiet("no policy disables the stall timer");
    }

    #[test]
    fn stall_timer_due_at_ms_matches_the_stall_threshold() {
        let runtime = RecordingMemberRuntime::new(Some(RecordingMemberRuntime::default_policy()));
        let mut with_policy = session(&runtime);
        assert_eq!(with_policy.member_health_stall_due_at_ms(), None, "no media progress means no stall timer");
        with_policy.origin_control.record_media_progress(10_000, 4_000);
        // no_data_timeout_ms = 8_000 and 2 x 4_000 ms target duration both equal 8_000.
        assert_eq!(with_policy.member_health_stall_due_at_ms(), Some(18_000));

        let plain_runtime = RecordingMemberRuntime::new(None);
        let mut without_policy = session(&plain_runtime);
        without_policy.origin_control.record_media_progress(10_000, 4_000);
        assert_eq!(without_policy.member_health_stall_due_at_ms(), None, "no policy disables the stall timer");
    }

    #[test]
    fn meter_drops_samples_timestamped_in_the_future() {
        let mut meter = HlsSlowDeliveryMeter::default();
        // A forward clock step records a sample in the future relative to the later samples; without
        // dropping future-dated samples the stale one would make three samples and report slow.
        meter.record(20_000, 4_000, 8_000, 15_000);
        meter.record(1_000, 4_000, 8_000, 15_000);
        meter.record(2_000, 4_000, 8_000, 15_000);
        assert!(!meter.is_slow(90), "the stale future-dated sample must not survive the backward step");
    }

    #[test]
    fn stall_uses_the_larger_of_timeout_and_two_target_durations() {
        let mut health = HlsMemberHealth::default();
        assert!(!health.stalled(15_000, 10_000, 4_000, 8_000), "5 s is below max(8 s, 8 s)");
        assert!(health.stalled(18_000, 10_000, 4_000, 8_000));
        assert!(!health.stalled(19_000, 10_000, 4_000, 8_000), "reports again only after another interval");
        assert!(health.stalled(26_000, 10_000, 4_000, 8_000));
        let mut long_segments = HlsMemberHealth::default();
        assert!(!long_segments.stalled(25_000, 10_000, 10_000, 8_000), "2 x 10 s target duration wins");
        assert!(long_segments.stalled(30_000, 10_000, 10_000, 8_000));
    }

    #[test]
    fn handoff_restarts_every_clock() {
        let mut health = HlsMemberHealth::default();
        assert!(!health.manifest_failed(true, 1_000, 8_000));
        health.reset_for_handoff(50_000);
        assert!(!health.stalled(52_000, 10_000, 4_000, 8_000), "progress of the old member does not count");
        assert!(health.stalled(58_000, 10_000, 4_000, 8_000));
        assert!(!health.manifest_failed(true, 58_500, 8_000));
    }

    #[test]
    fn sessions_without_a_policy_never_report() {
        let runtime = RecordingMemberRuntime::new(None);
        let mut session = session(&runtime);
        session.origin_control.record_media_progress(1_000, 4_000);
        for now in [10_000, 20_000, 30_000] {
            send(session.member_unhealthy_after_manifest_failure(Some(true), now));
            send(session.member_unhealthy_after_manifest_success(now));
            send(session.member_unhealthy_after_segment_ready(now, 4_000, 90_000));
        }
        runtime.assert_quiet("");
        assert_eq!(runtime.progress(), 0);
        let plain = HlsSession::new(HlsSessionKey::new(1, "plain"), b"secret", 0);
        assert!(plain.member_unhealthy_after_media_failure(&crate::HlsOriginResourceFetchError::Timeout).is_none());
    }

    #[test]
    fn manifest_failures_notify_with_the_classified_reason() {
        let runtime = RecordingMemberRuntime::new(Some(RecordingMemberRuntime::default_policy()));
        let mut session = session(&runtime);
        send(session.member_unhealthy_after_manifest_failure(Some(false), 1_000));
        runtime.assert_quiet("one retryable failure must not switch");
        send(session.member_unhealthy_after_manifest_failure(None, 1_100));
        runtime.assert_quiet("failures unrelated to the origin are ignored");
        send(session.member_unhealthy_after_manifest_failure(Some(true), 1_200));
        runtime.assert_quiet("");
        send(session.member_unhealthy_after_manifest_failure(Some(true), 1_300));
        assert_eq!(runtime.reasons(), vec![HlsMemberUnhealthyReason::ManifestFailure]);
    }

    #[test]
    fn stalled_publication_notifies_stall() {
        let runtime = RecordingMemberRuntime::new(Some(RecordingMemberRuntime::default_policy()));
        let mut session = session(&runtime);
        session.origin_control.record_media_progress(10_000, 4_000);
        send(session.member_unhealthy_after_manifest_success(15_000));
        runtime.assert_quiet("");
        send(session.member_unhealthy_after_manifest_success(18_000));
        assert_eq!(runtime.reasons(), vec![HlsMemberUnhealthyReason::Stall]);
    }

    #[test]
    fn media_failures_notify_only_for_origin_delivery_errors() {
        let runtime = RecordingMemberRuntime::new(Some(RecordingMemberRuntime::default_policy()));
        let session = session(&runtime);
        for error in [
            crate::HlsOriginResourceFetchError::PermanentStatus(axum::http::StatusCode::NOT_FOUND),
            crate::HlsOriginResourceFetchError::RetryableStatus(axum::http::StatusCode::BAD_GATEWAY),
            crate::HlsOriginResourceFetchError::Transport("reset".to_string()),
        ] {
            send(session.member_unhealthy_after_media_failure(&error));
        }
        assert_eq!(runtime.reasons(), vec![HlsMemberUnhealthyReason::SegmentFailure; 3]);
        for error in [
            crate::HlsOriginResourceFetchError::Superseded,
            crate::HlsOriginResourceFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::Expired),
        ] {
            assert!(session.member_unhealthy_after_media_failure(&error).is_none());
        }
    }

    #[test]
    fn ready_segments_report_progress_and_slow_delivery() {
        let runtime = RecordingMemberRuntime::new(Some(RecordingMemberRuntime::default_policy()));
        let mut session = session(&runtime);
        for at in [1_000, 2_000] {
            send(session.member_unhealthy_after_segment_ready(at, 4_000, 8_000));
        }
        runtime.assert_quiet("fewer than three segments are no decision");
        send(session.member_unhealthy_after_segment_ready(3_000, 4_000, 8_000));
        assert_eq!(runtime.reasons(), vec![HlsMemberUnhealthyReason::SlowDelivery]);
        assert_eq!(runtime.progress(), 3);
        send(session.member_unhealthy_after_segment_ready(4_000, 4_000, 8_000));
        assert_eq!(runtime.reasons().len(), 1, "the window restarts after a report");
    }

    #[test]
    fn fast_segments_never_report_slow_delivery() {
        let runtime = RecordingMemberRuntime::new(Some(RecordingMemberRuntime::default_policy()));
        let mut session = session(&runtime);
        for at in 1..=10 {
            send(session.member_unhealthy_after_segment_ready(at * 1_000, 4_000, 400));
        }
        runtime.assert_quiet("");
        assert_eq!(runtime.progress(), 10);
    }
}
