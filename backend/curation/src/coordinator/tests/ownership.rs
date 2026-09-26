use super::super::complete_evaluation;
use crate::kernel::{
    CurationEvaluation, CurationFailure, CurationIncompleteReason, CurationMediaKind, CurationMembership,
    CurationRunOutcome, CurationSelectorKey, CurationSelectorSummary, CurationUnavailableReason, SelectorOutcome,
};
use shared::utils::hash_string;

fn membership(key: usize, order: usize, title: &str, kind: CurationMediaKind, rank: Option<u32>) -> CurationMembership {
    let mut title_tiebreak = String::with_capacity(title.len() + 32);
    title_tiebreak.push_str(title);
    CurationMembership {
        selector_key: CurationSelectorKey(key),
        subject_uuid: hash_string(title),
        media_kind: kind,
        rank,
        title_tiebreak,
        candidate_order: order,
    }
}

fn completed_outcomes() -> Vec<SelectorOutcome> {
    vec![
        SelectorOutcome::Complete {
            key: CurationSelectorKey(7),
            reference_count: 4,
            memberships: vec![
                membership(7, 2, "Second", CurationMediaKind::Series, Some(9)),
                membership(7, 0, "First", CurationMediaKind::Movie, Some(3)),
            ],
        },
        SelectorOutcome::Complete { key: CurationSelectorKey(3), reference_count: 1, memberships: vec![] },
        SelectorOutcome::Complete {
            key: CurationSelectorKey(11),
            reference_count: 2,
            memberships: vec![
                membership(11, 8, "Other", CurationMediaKind::Movie, None),
                membership(11, 9, "Last", CurationMediaKind::Series, Some(1)),
            ],
        },
    ]
}

fn complete_memberships(outcomes: &[SelectorOutcome]) -> impl Iterator<Item = &CurationMembership> {
    outcomes.iter().flat_map(|outcome| match outcome {
        SelectorOutcome::Complete { memberships, .. } => memberships.as_slice(),
        SelectorOutcome::Incomplete { .. } | SelectorOutcome::Unavailable { .. } => &[],
    })
}

fn title_buffer(membership: &CurationMembership) -> (*const u8, usize, usize) {
    let title = &membership.title_tiebreak;
    assert!(!title.is_empty(), "empty strings cannot witness a moved heap allocation");
    (title.as_ptr(), title.len(), title.capacity())
}

#[test]
fn complete_admission_moves_title_buffers_and_preserves_selector_and_membership_order() {
    let outcomes = completed_outcomes();
    let buffers: Vec<_> = complete_memberships(&outcomes).map(title_buffer).collect();
    let expected_memberships: Vec<_> = complete_memberships(&outcomes).cloned().collect();

    let CurationRunOutcome::Complete(evaluation) = complete_evaluation(outcomes) else {
        panic!("all complete selectors must be admitted");
    };
    assert_eq!(
        evaluation.selectors,
        vec![
            CurationSelectorSummary { key: CurationSelectorKey(7), reference_count: 4, membership_count: 2 },
            CurationSelectorSummary { key: CurationSelectorKey(3), reference_count: 1, membership_count: 0 },
            CurationSelectorSummary { key: CurationSelectorKey(11), reference_count: 2, membership_count: 2 },
        ]
    );
    assert_eq!(evaluation.memberships, expected_memberships);
    assert_eq!(
        evaluation.memberships.iter().map(title_buffer).collect::<Vec<_>>(),
        buffers,
        "aggregate by moving existing Strings, not by cloning equal values"
    );
}

#[test]
fn failed_admission_retains_original_title_buffers_and_all_diagnostics_at_every_position() {
    for failure in [
        SelectorOutcome::Incomplete { key: CurationSelectorKey(19), reason: CurationIncompleteReason::Interrupted },
        SelectorOutcome::Unavailable { key: CurationSelectorKey(19), reason: CurationUnavailableReason::Configuration },
    ] {
        for position in 0..=3 {
            let mut outcomes = completed_outcomes();
            outcomes.insert(position, failure.clone());
            outcomes.push(SelectorOutcome::Unavailable {
                key: CurationSelectorKey(23),
                reason: CurationUnavailableReason::Source,
            });
            let buffers: Vec<_> = complete_memberships(&outcomes).map(title_buffer).collect();
            let expected = outcomes.clone();

            let CurationRunOutcome::Failed(diagnostic) = complete_evaluation(outcomes) else {
                panic!("any incomplete or unavailable selector must reject the whole run");
            };
            assert_eq!(diagnostic.selector_outcomes, expected);
            // This proves ownership of the retained diagnostics, not the absence
            // of temporary copies discarded before returning a failure.
            assert_eq!(
                complete_memberships(&diagnostic.selector_outcomes).map(title_buffer).collect::<Vec<_>>(),
                buffers
            );
        }
    }
}

#[test]
fn no_selectors_completed_empty_selectors_and_failed_only_runs_remain_distinct() {
    assert_eq!(complete_evaluation(vec![]), CurationRunOutcome::NotConfigured);
    let outcomes = vec![
        SelectorOutcome::Complete { key: CurationSelectorKey(0), reference_count: 0, memberships: vec![] },
        SelectorOutcome::Complete { key: CurationSelectorKey(1), reference_count: 2, memberships: vec![] },
    ];
    assert_eq!(
        complete_evaluation(outcomes),
        CurationRunOutcome::Complete(CurationEvaluation {
            selectors: vec![
                CurationSelectorSummary { key: CurationSelectorKey(0), reference_count: 0, membership_count: 0 },
                CurationSelectorSummary { key: CurationSelectorKey(1), reference_count: 2, membership_count: 0 },
            ],
            memberships: vec![],
        })
    );
    let failed = vec![SelectorOutcome::Unavailable {
        key: CurationSelectorKey(0),
        reason: CurationUnavailableReason::Configuration,
    }];
    assert_eq!(
        complete_evaluation(failed.clone()),
        CurationRunOutcome::Failed(CurationFailure { selector_outcomes: failed })
    );
}
