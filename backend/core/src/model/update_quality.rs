use shared::model::{ConfigInputUpdateQualityDto, XtreamCluster};

const MAX_UPDATE_QUALITY_THRESHOLD: u8 = 100;

/// Resolved per-cluster thresholds for the input update quality guard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConfigInputUpdateQuality {
    live: u8,
    vod: u8,
    series: u8,
}

impl ConfigInputUpdateQuality {
    /// Returns the configured threshold for an Xtream-compatible cluster.
    #[must_use]
    pub const fn threshold(&self, cluster: XtreamCluster) -> u8 {
        match cluster {
            XtreamCluster::Live => self.live,
            XtreamCluster::Video => self.vod,
            XtreamCluster::Series => self.series,
        }
    }
}

impl From<&ConfigInputUpdateQualityDto> for ConfigInputUpdateQuality {
    fn from(dto: &ConfigInputUpdateQualityDto) -> Self { Self { live: dto.live, vod: dto.vod, series: dto.series } }
}

/// Typed outcome of evaluating one candidate cluster against its active baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateQualityDecision {
    Disabled,
    BootstrapAccepted { candidate: usize, threshold: u8 },
    Accepted { current: usize, candidate: usize, threshold: u8, quality: u8 },
    Rejected { current: usize, candidate: usize, threshold: u8, quality: u8 },
    RejectedWithoutBaseline { candidate: usize, threshold: u8 },
}

impl UpdateQualityDecision {
    /// Preserves an already evaluated accepted Quality result for downstream reporting.
    #[must_use]
    pub const fn acceptance(self, cluster: XtreamCluster) -> Option<ClusterUpdateAcceptance> {
        match self {
            Self::BootstrapAccepted { candidate, threshold } => Some(ClusterUpdateAcceptance {
                cluster,
                current_count: None,
                candidate_count: candidate,
                threshold,
                quality: None,
            }),
            Self::Accepted { current, candidate, threshold, quality } => Some(ClusterUpdateAcceptance {
                cluster,
                current_count: Some(current),
                candidate_count: candidate,
                threshold,
                quality: Some(quality),
            }),
            Self::Disabled | Self::Rejected { .. } | Self::RejectedWithoutBaseline { .. } => None,
        }
    }

    /// Preserves an already evaluated rejected Quality result for downstream reporting.
    #[must_use]
    pub const fn rejection(self, cluster: XtreamCluster) -> Option<ClusterUpdateRejection> {
        match self {
            Self::Rejected { current, candidate, threshold, quality } => Some(ClusterUpdateRejection {
                cluster,
                current_count: current,
                candidate_count: candidate,
                threshold,
                quality,
            }),
            Self::RejectedWithoutBaseline { candidate, threshold } => Some(ClusterUpdateRejection {
                cluster,
                current_count: 0,
                candidate_count: candidate,
                threshold,
                quality: 0,
            }),
            Self::Disabled | Self::BootstrapAccepted { .. } | Self::Accepted { .. } => None,
        }
    }
}

/// Completed normal Quality evaluation whose candidate was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterUpdateAcceptance {
    pub cluster: XtreamCluster,
    pub current_count: Option<usize>,
    pub candidate_count: usize,
    pub threshold: u8,
    pub quality: Option<u8>,
}

/// Nonfatal report for one cluster whose candidate update was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterUpdateRejection {
    pub cluster: XtreamCluster,
    pub current_count: usize,
    pub candidate_count: usize,
    pub threshold: u8,
    pub quality: u8,
}

/// Audit report for one cluster accepted through a request-local quality bypass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterForceUpdate {
    pub cluster: XtreamCluster,
    pub candidate_count: usize,
    pub configured_threshold: u8,
}

/// Evaluates a candidate count without I/O or floating-point arithmetic.
///
/// A missing count is treated as having no baseline. Config
/// validation guarantees thresholds in `0..=100`; directly supplied values
/// above 100 are defensively normalized to 100.
#[must_use]
pub fn evaluate_update_quality(current: Option<usize>, candidate: usize, threshold: u8) -> UpdateQualityDecision {
    let threshold = threshold.min(MAX_UPDATE_QUALITY_THRESHOLD);
    if threshold == 0 {
        return UpdateQualityDecision::Disabled;
    }

    let Some(current) = current.filter(|value| *value > 0) else {
        return if candidate > 0 {
            UpdateQualityDecision::BootstrapAccepted { candidate, threshold }
        } else {
            UpdateQualityDecision::RejectedWithoutBaseline { candidate, threshold }
        };
    };

    if current == 0 {
        return if candidate == 0 {
            UpdateQualityDecision::Accepted { current: 0, candidate: 0, threshold, quality: 100 }
        } else {
            UpdateQualityDecision::Rejected { current: 0, candidate, threshold, quality: 0 }
        };
    }

    let difference = current.abs_diff(candidate) as u128;
    let current_wide = current as u128;
    let scaled_difference = difference * 100;
    let allowed_difference = current_wide * u128::from(100_u8.saturating_sub(threshold));
    let accepted = scaled_difference <= allowed_difference;

    let rounded_deviation =
        scaled_difference / current_wide + u128::from(!scaled_difference.is_multiple_of(current_wide));
    let Ok(quality) = u8::try_from(100_u128.saturating_sub(rounded_deviation)) else {
        return UpdateQualityDecision::Rejected { current, candidate, threshold, quality: 0 };
    };

    if accepted {
        UpdateQualityDecision::Accepted { current, candidate, threshold, quality }
    } else {
        UpdateQualityDecision::Rejected { current, candidate, threshold, quality }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        evaluate_update_quality, ClusterUpdateAcceptance, ClusterUpdateRejection, ConfigInputUpdateQuality,
        UpdateQualityDecision,
    };
    use shared::model::{ConfigInputUpdateQualityDto, XtreamCluster};

    struct Case {
        name: &'static str,
        current: Option<usize>,
        candidate: usize,
        threshold: u8,
        expected: UpdateQualityDecision,
    }

    fn assert_cases(cases: &[Case]) {
        for case in cases {
            let decision = evaluate_update_quality(case.current, case.candidate, case.threshold);
            if let UpdateQualityDecision::Accepted { threshold, quality, .. } = decision {
                assert!(threshold <= 100, "accepted threshold exceeds 100 in case: {}", case.name);
                assert!(quality >= threshold, "accepted quality is below threshold in case: {}", case.name);
            }
            assert_eq!(decision, case.expected, "case: {}", case.name);
        }
    }

    #[test]
    fn runtime_policy_maps_thresholds_to_clusters() {
        let policy = ConfigInputUpdateQuality::from(&ConfigInputUpdateQualityDto { live: 95, vod: 90, series: 85 });

        assert_eq!(policy.threshold(XtreamCluster::Live), 95);
        assert_eq!(policy.threshold(XtreamCluster::Video), 90);
        assert_eq!(policy.threshold(XtreamCluster::Series), 85);
    }

    #[test]
    fn evaluates_disabled_and_bootstrap_cases() {
        assert_cases(&[
            Case {
                name: "disabled check preserves existing behavior",
                current: Some(1_000),
                candidate: 0,
                threshold: 0,
                expected: UpdateQualityDecision::Disabled,
            },
            Case {
                name: "missing baseline accepts a non-empty bootstrap",
                current: None,
                candidate: 42,
                threshold: 90,
                expected: UpdateQualityDecision::BootstrapAccepted { candidate: 42, threshold: 90 },
            },
            Case {
                name: "zero baseline accepts a non-empty bootstrap",
                current: Some(0),
                candidate: 42,
                threshold: 90,
                expected: UpdateQualityDecision::BootstrapAccepted { candidate: 42, threshold: 90 },
            },
            Case {
                name: "missing baseline rejects an empty candidate",
                current: None,
                candidate: 0,
                threshold: 90,
                expected: UpdateQualityDecision::RejectedWithoutBaseline { candidate: 0, threshold: 90 },
            },
        ]);
    }

    #[test]
    fn pipeline_transparency_quality_reports_preserve_evaluated_acceptance_and_rejection() {
        let accepted = evaluate_update_quality(Some(1_000), 950, 90);
        assert_eq!(
            accepted.acceptance(XtreamCluster::Live),
            Some(ClusterUpdateAcceptance {
                cluster: XtreamCluster::Live,
                current_count: Some(1_000),
                candidate_count: 950,
                threshold: 90,
                quality: Some(95),
            })
        );
        assert_eq!(accepted.rejection(XtreamCluster::Live), None);

        let bootstrap = evaluate_update_quality(None, 42, 90);
        assert_eq!(
            bootstrap.acceptance(XtreamCluster::Video),
            Some(ClusterUpdateAcceptance {
                cluster: XtreamCluster::Video,
                current_count: None,
                candidate_count: 42,
                threshold: 90,
                quality: None,
            })
        );

        let rejected = evaluate_update_quality(None, 0, 90);
        assert_eq!(
            rejected.rejection(XtreamCluster::Series),
            Some(ClusterUpdateRejection {
                cluster: XtreamCluster::Series,
                current_count: 0,
                candidate_count: 0,
                threshold: 90,
                quality: 0,
            })
        );
        assert_eq!(evaluate_update_quality(Some(100), 50, 0).acceptance(XtreamCluster::Live), None);
    }

    #[test]
    fn evaluates_90_percent_boundaries() {
        assert_cases(&[
            Case {
                name: "lower 90 percent boundary is accepted",
                current: Some(1_000),
                candidate: 900,
                threshold: 90,
                expected: UpdateQualityDecision::Accepted {
                    current: 1_000,
                    candidate: 900,
                    threshold: 90,
                    quality: 90,
                },
            },
            Case {
                name: "upper 90 percent boundary is accepted",
                current: Some(1_000),
                candidate: 1_100,
                threshold: 90,
                expected: UpdateQualityDecision::Accepted {
                    current: 1_000,
                    candidate: 1_100,
                    threshold: 90,
                    quality: 90,
                },
            },
            Case {
                name: "one below lower 90 percent boundary is rejected",
                current: Some(1_000),
                candidate: 899,
                threshold: 90,
                expected: UpdateQualityDecision::Rejected {
                    current: 1_000,
                    candidate: 899,
                    threshold: 90,
                    quality: 89,
                },
            },
            Case {
                name: "one above upper 90 percent boundary is rejected",
                current: Some(1_000),
                candidate: 1_101,
                threshold: 90,
                expected: UpdateQualityDecision::Rejected {
                    current: 1_000,
                    candidate: 1_101,
                    threshold: 90,
                    quality: 89,
                },
            },
        ]);
    }

    #[test]
    fn evaluates_exact_empty_and_large_counter_cases() {
        assert_cases(&[
            Case {
                name: "threshold 100 accepts an exact count",
                current: Some(1_000),
                candidate: 1_000,
                threshold: 100,
                expected: UpdateQualityDecision::Accepted {
                    current: 1_000,
                    candidate: 1_000,
                    threshold: 100,
                    quality: 100,
                },
            },
            Case {
                name: "threshold 100 rejects any difference",
                current: Some(1_000),
                candidate: 999,
                threshold: 100,
                expected: UpdateQualityDecision::Rejected {
                    current: 1_000,
                    candidate: 999,
                    threshold: 100,
                    quality: 99,
                },
            },
            Case {
                name: "existing baseline rejects an empty candidate",
                current: Some(1_000),
                candidate: 0,
                threshold: 90,
                expected: UpdateQualityDecision::Rejected { current: 1_000, candidate: 0, threshold: 90, quality: 0 },
            },
            Case {
                name: "large counters do not overflow",
                current: Some(usize::MAX),
                candidate: usize::MAX - 1,
                threshold: 99,
                expected: UpdateQualityDecision::Accepted {
                    current: usize::MAX,
                    candidate: usize::MAX - 1,
                    threshold: 99,
                    quality: 99,
                },
            },
            Case {
                name: "large difference products do not overflow",
                current: Some(usize::MAX),
                candidate: 0,
                threshold: 1,
                expected: UpdateQualityDecision::Rejected {
                    current: usize::MAX,
                    candidate: 0,
                    threshold: 1,
                    quality: 0,
                },
            },
        ]);
    }

    #[test]
    fn normalizes_out_of_range_thresholds_before_deciding() {
        assert_cases(&[
            Case {
                name: "threshold 101 accepts an identical count as normalized 100",
                current: Some(1_000),
                candidate: 1_000,
                threshold: 101,
                expected: UpdateQualityDecision::Accepted {
                    current: 1_000,
                    candidate: 1_000,
                    threshold: 100,
                    quality: 100,
                },
            },
            Case {
                name: "threshold 101 rejects a differing count as normalized 100",
                current: Some(1_000),
                candidate: 999,
                threshold: 101,
                expected: UpdateQualityDecision::Rejected {
                    current: 1_000,
                    candidate: 999,
                    threshold: 100,
                    quality: 99,
                },
            },
            Case {
                name: "maximum u8 threshold accepts an identical count as normalized 100",
                current: Some(1_000),
                candidate: 1_000,
                threshold: u8::MAX,
                expected: UpdateQualityDecision::Accepted {
                    current: 1_000,
                    candidate: 1_000,
                    threshold: 100,
                    quality: 100,
                },
            },
            Case {
                name: "maximum u8 threshold rejects a differing count as normalized 100",
                current: Some(1_000),
                candidate: 1_001,
                threshold: u8::MAX,
                expected: UpdateQualityDecision::Rejected {
                    current: 1_000,
                    candidate: 1_001,
                    threshold: 100,
                    quality: 99,
                },
            },
        ]);
    }
}
