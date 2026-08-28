//! Deterministic recording conflict analyzer.
//!
//! Conflict analysis is advisory. Runtime capacity handling remains
//! authoritative; schedules are not rejected solely because of an
//! advisory conflict.
//!
//! Classification:
//! - `NoKnownConflict`: demand never exceeds capacity.
//! - `PossibleCapacityWait`: demand exceeds capacity for some, but
//!   not all, of the candidate's padded interval.
//! - `LikelyMissedWindow`: no slot is predicted for the entire
//!   candidate interval.
//!
//! The analyzer builds piecewise demand segments across the union of
//! the candidate and the equal-or-higher-priority scheduled / active
//! recordings on the same provider/input. Each segment's demand is
//! compared against the effective capacity. The final classification
//! is the worst segment the candidate overlaps.
//!
//! Privacy contract: the analyzer never returns another private
//! recording's owner, title, channel display name, filename, rule
//! or task id. Logs and the response only carry the provider scope,
//! anonymized interval, and severity.

/// A demand point: a recording (the candidate or another scheduled /
/// active recording) plus the effective interval it occupies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemandPoint {
    /// Stable opaque task id. Used for log correlation, never surfaced
    /// in the preview response.
    pub task_id: String,
    /// `padded_start..padded_end` (Unix seconds, inclusive-exclusive).
    pub padded_start: i64,
    pub padded_end: i64,
    /// Higher `priority` means the demand dominates lower priorities.
    pub priority: i32,
}

/// Per-provider input capacity. `background_slots` is the
/// `max_background_per_provider` the worker actually uses. Reserved
/// interactive slots reduce the headroom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveCapacity {
    pub background_slots: u32,
    pub reserved_interactive_slots: u32,
}

impl EffectiveCapacity {
    /// `max(0, background_slots - reserved_interactive_slots)`.
    /// Saturates to zero so a misconfiguration never reports a
    /// negative headroom.
    pub fn headroom(self) -> u32 {
        self.background_slots.saturating_sub(self.reserved_interactive_slots)
    }
}

/// A single piecewise demand segment on the candidate's padded
/// interval. `peak_demand` is the maximum number of equal-or-higher
/// priority recordings active anywhere in the segment, *excluding*
/// the candidate itself (the candidate is implicitly always
/// present).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DemandSegment {
    pub start: i64,
    pub end: i64,
    pub peak_demand: u32,
}

/// The advisory classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictSeverity {
    NoKnownConflict,
    PossibleCapacityWait,
    LikelyMissedWindow,
}

impl ConflictSeverity {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::NoKnownConflict => "no_known_conflict",
            Self::PossibleCapacityWait => "possible_capacity_wait",
            Self::LikelyMissedWindow => "likely_missed_window",
        }
    }
}

/// The output of the analyzer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictPreview {
    pub severity: ConflictSeverity,
    /// Provider scope only — never a target id or another user's
    /// identifier. Optional: `None` when the analyzer has no
    /// provider context.
    pub provider_scope: Option<String>,
    /// Anonymized overlap intervals where the demand exceeded
    /// capacity. The candidate's own id is not stored here.
    pub overlap_segments: Vec<DemandSegment>,
}

/// Pure: build the piecewise demand segments across the union of
/// the candidate's padded interval and every equal-or-higher-priority
/// `DemandPoint`. `EqualOrHigherPriority` is satisfied by filtering
/// to `priority >= candidate_priority` before the segment build.
pub fn build_demand_segments(candidate: &DemandPoint, higher_priority: &[DemandPoint]) -> Vec<DemandSegment> {
    // Collect the boundary points: candidate start/end plus every
    // boundary of the higher-priority demand.
    let mut boundaries: Vec<i64> = Vec::with_capacity((higher_priority.len() + 1) * 2 + 2);
    boundaries.push(candidate.padded_start);
    boundaries.push(candidate.padded_end);
    for hp in higher_priority {
        boundaries.push(hp.padded_start);
        boundaries.push(hp.padded_end);
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut segments: Vec<DemandSegment> = Vec::new();
    for pair in boundaries.windows(2) {
        let start = pair[0];
        let end = pair[1];
        if end <= start {
            continue;
        }
        // Skip segments outside the candidate's interval — the
        // preview is the candidate's view, not the global view.
        if end <= candidate.padded_start || start >= candidate.padded_end {
            continue;
        }
        // Peak demand at any midpoint in [start, end) is the count
        // of higher-priority points whose padded interval contains
        // that midpoint. The midpoint is (start + end) / 2; saturate
        // on overflow.
        let mid = start.saturating_add(end).saturating_div(2);
        let mut peak: u32 = 0;
        for hp in higher_priority {
            if hp.padded_start <= mid && mid < hp.padded_end {
                peak = peak.saturating_add(1);
            }
        }
        segments.push(DemandSegment { start, end, peak_demand: peak });
    }
    segments
}

/// Pure: classify the candidate against the piecewise demand and the
/// effective capacity. The candidate itself is treated as an
/// additional unit of demand (so the candidate's own segment is
/// `peak + 1`).
pub fn classify(candidate: &DemandPoint, segments: &[DemandSegment], capacity: EffectiveCapacity) -> ConflictSeverity {
    let headroom = capacity.headroom();
    // The candidate's effective load on every overlapping segment is
    // `peak + 1`. `headroom` is the number of additional recordings
    // the runtime will accept on the same provider/input.
    //
    // - always_ok: every segment is `peak + 1 <= headroom` (the
    //   candidate fits at every moment it overlaps).
    // - always_over: every overlapping segment is `peak + 1 >
    //   headroom` (no slot is predicted for any moment).
    // - mixed: the candidate fits for some but not all of its
    //   interval. This is the `PossibleCapacityWait` case.
    let mut any_overlap = false;
    let mut always_ok = true;
    let mut always_over = true;
    for segment in segments {
        if segment.end <= candidate.padded_start || segment.start >= candidate.padded_end {
            continue;
        }
        any_overlap = true;
        let load = segment.peak_demand.saturating_add(1);
        if load <= headroom {
            always_over = false;
        } else {
            always_ok = false;
        }
    }
    if !any_overlap {
        return ConflictSeverity::NoKnownConflict;
    }
    if always_ok {
        ConflictSeverity::NoKnownConflict
    } else if always_over {
        ConflictSeverity::LikelyMissedWindow
    } else {
        ConflictSeverity::PossibleCapacityWait
    }
}

/// Pure: build a conflict preview from the candidate, the
/// equal-or-higher-priority demand, and the effective capacity. The
/// `provider_scope` is optional metadata; it is never derived from a
/// private recording.
pub fn preview_conflict(
    candidate: &DemandPoint,
    others: &[DemandPoint],
    capacity: EffectiveCapacity,
    provider_scope: Option<String>,
) -> ConflictPreview {
    let higher: Vec<DemandPoint> = others.iter().filter(|d| d.priority >= candidate.priority).cloned().collect();
    let segments = build_demand_segments(candidate, &higher);
    let severity = classify(candidate, &segments, capacity);
    // Anonymize: the returned segments carry peak_demand and the
    // boundaries of the overlap window only. Each segment's boundaries
    // are derived from the higher-priority recordings' padded intervals
    // (clipped to the candidate window) so they are sufficient for a
    // capacity preview without leaking the other recordings' titles,
    // channels, or absolute intervals.
    let overlap: Vec<DemandSegment> = segments.into_iter().filter(|s| s.peak_demand > 0).collect();
    ConflictPreview { severity, provider_scope, overlap_segments: overlap }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(start: i64, end: i64, priority: i32) -> DemandPoint {
        DemandPoint { task_id: "cand".into(), padded_start: start, padded_end: end, priority }
    }

    fn other(task_id: &str, start: i64, end: i64, priority: i32) -> DemandPoint {
        DemandPoint { task_id: task_id.into(), padded_start: start, padded_end: end, priority }
    }

    #[test]
    fn empty_demand_means_no_known_conflict() {
        let candidate = cand(100, 200, 0);
        let segments = build_demand_segments(&candidate, &[]);
        let sev =
            classify(&candidate, &segments, EffectiveCapacity { background_slots: 1, reserved_interactive_slots: 0 });
        assert_eq!(sev, ConflictSeverity::NoKnownConflict);
    }

    #[test]
    fn lower_priority_other_is_ignored() {
        let candidate = cand(100, 200, 5);
        // `other` has priority 1 — well below the candidate's 5. The
        // analyzer must ignore it.
        let others = vec![other("o1", 100, 200, 1)];
        let preview = preview_conflict(
            &candidate,
            &others,
            EffectiveCapacity { background_slots: 1, reserved_interactive_slots: 0 },
            None,
        );
        assert_eq!(preview.severity, ConflictSeverity::NoKnownConflict);
    }

    #[test]
    fn equal_priority_overlap_triggers_likely_missed_window_when_headroom_zero() {
        // 1 background slot, no reserved → headroom = 1. With 1
        // equal-priority demand + the candidate itself, every
        // moment has load = 2, which is > headroom = 1. That makes
        // the whole window over → LikelyMissedWindow.
        let candidate = cand(100, 200, 0);
        let others = vec![other("o1", 100, 200, 0)];
        let preview = preview_conflict(
            &candidate,
            &others,
            EffectiveCapacity { background_slots: 1, reserved_interactive_slots: 0 },
            None,
        );
        assert_eq!(preview.severity, ConflictSeverity::LikelyMissedWindow);
    }

    #[test]
    fn equal_priority_partial_overlap_is_possible_capacity_wait() {
        // Candidate spans 100..200; other covers 100..150 only. The
        // overlap 100..150 is over-capacity (load 2, headroom 1);
        // 150..200 is under-capacity. Mixed → PossibleCapacityWait.
        let candidate = cand(100, 200, 0);
        let others = vec![other("o1", 100, 150, 0)];
        let preview = preview_conflict(
            &candidate,
            &others,
            EffectiveCapacity { background_slots: 1, reserved_interactive_slots: 0 },
            None,
        );
        assert_eq!(preview.severity, ConflictSeverity::PossibleCapacityWait);
    }

    #[test]
    fn whole_window_over_capacity_is_likely_missed_window() {
        // 3 simultaneous demands on a 1-slot headroom.
        let candidate = cand(100, 200, 0);
        let others = vec![other("o1", 100, 200, 0), other("o2", 100, 200, 0), other("o3", 100, 200, 0)];
        let preview = preview_conflict(
            &candidate,
            &others,
            EffectiveCapacity { background_slots: 1, reserved_interactive_slots: 0 },
            None,
        );
        assert_eq!(preview.severity, ConflictSeverity::LikelyMissedWindow);
    }

    #[test]
    fn reserved_interactive_slots_reduce_headroom() {
        // 1 background slot, 1 reserved interactive → headroom 0.
        let candidate = cand(100, 200, 0);
        let others = vec![other("o1", 100, 200, 0)];
        let preview = preview_conflict(
            &candidate,
            &others,
            EffectiveCapacity { background_slots: 1, reserved_interactive_slots: 1 },
            None,
        );
        assert_eq!(preview.severity, ConflictSeverity::LikelyMissedWindow);
    }

    #[test]
    fn partial_window_over_is_possible_capacity_wait() {
        // Candidate spans 100..200; other covers 150..250. The
        // overlap segment 150..200 is over-capacity; 100..150 is
        // under-capacity → mixed.
        let candidate = cand(100, 200, 0);
        let others = vec![other("o1", 150, 250, 0)];
        let preview = preview_conflict(
            &candidate,
            &others,
            EffectiveCapacity { background_slots: 1, reserved_interactive_slots: 0 },
            None,
        );
        assert_eq!(preview.severity, ConflictSeverity::PossibleCapacityWait);
    }

    #[test]
    fn no_overlap_means_no_known_conflict() {
        let candidate = cand(100, 200, 0);
        let others = vec![other("o1", 300, 400, 0)];
        let preview = preview_conflict(
            &candidate,
            &others,
            EffectiveCapacity { background_slots: 1, reserved_interactive_slots: 0 },
            None,
        );
        assert_eq!(preview.severity, ConflictSeverity::NoKnownConflict);
    }

    #[test]
    fn segments_outside_candidate_are_dropped() {
        // `other` covers 0..50 — entirely before the candidate.
        // The segment 0..50 should be dropped from the preview.
        let candidate = cand(100, 200, 0);
        let others = vec![other("o1", 0, 50, 0)];
        let preview = preview_conflict(
            &candidate,
            &others,
            EffectiveCapacity { background_slots: 1, reserved_interactive_slots: 0 },
            None,
        );
        assert_eq!(preview.severity, ConflictSeverity::NoKnownConflict);
        assert!(preview.overlap_segments.is_empty());
    }

    #[test]
    fn severity_wire_strings_are_stable() {
        assert_eq!(ConflictSeverity::NoKnownConflict.as_wire(), "no_known_conflict");
        assert_eq!(ConflictSeverity::PossibleCapacityWait.as_wire(), "possible_capacity_wait");
        assert_eq!(ConflictSeverity::LikelyMissedWindow.as_wire(), "likely_missed_window");
    }

    #[test]
    fn headroom_saturates_when_reserved_exceeds_background() {
        let c = EffectiveCapacity { background_slots: 1, reserved_interactive_slots: 5 };
        assert_eq!(c.headroom(), 0);
    }

    #[test]
    fn preview_never_leaks_other_task_ids() {
        let candidate = cand(100, 200, 0);
        let others = vec![other("private-task", 100, 200, 0)];
        let preview = preview_conflict(
            &candidate,
            &others,
            EffectiveCapacity { background_slots: 1, reserved_interactive_slots: 0 },
            None,
        );
        // The serialized form must not contain the other task id.
        let serialized = format!("{preview:?}");
        assert!(!serialized.contains("private-task"));
    }
}
