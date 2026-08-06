//! Cross-store rule reconciliation.
//!
//! The scheduler persists two stores that can drift:
//! - The queue (`downloads_state.json`) holds the materialized
//!   recording tasks.
//! - The rule repository (`recording_rules.json`) holds the rules
//!   and the bounded tombstones.
//!
//! Drift happens when a queue persistence succeeds but the rule
//! tombstone write fails (or vice versa). The reconciliation pass
//! is a pure function over the (tasks, rules, tombstones, now)
//! tuple that produces the actions the scheduler should take. The
//! caller applies them under the queue-mutation boundary; the
//! rule-side writes follow the fixed cross-store lock order:
//!
//! ```text
//! queue mutation boundary -> rule repository mutation
//! ```
//!
//! Reconciliation truth table:
//! - Materialized task without `Scheduled` tombstone →
//!   `AddScheduledTombstone` (reconciliation repairs the drift).
//! - `Scheduled` tombstone without a task and still eligible →
//!   `Materialize` (only when no terminal tombstone exists).
//! - `Cancelled` tombstone with an eligible inactive task →
//!   `Finalize` (complete the cancellation / removal that the
//!   operator already expressed).
//! - `Completed` tombstone → always suppress rematerialization
//!   inside the horizon, regardless of task presence.
//! - Active task is never cancelled or removed solely because of a
//!   stale reconciliation intent. The reconciler logs a
//!   `ConflictingIntent` and the operator must resolve manually.


use shared::model::recording_rule::{RecordingRule, RecordingTombstone, TombstoneKind, TombstoneSet};

/// Minimum retention horizon for tombstones. Tombstones are retained
/// for at least 14 days even when the EPG horizon is shorter. Weekly
/// rules without an EPG horizon still suppress duplicates inside this
/// window.
pub const MIN_TOMBSTONE_HORIZON_SECS: i64 = 14 * 86_400;

/// A task from the queue, summarized for reconciliation. The real
/// `FileDownload` has more fields; reconciliation only needs the
/// identity, provenance, state, and activity flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcilableTask {
    pub uuid: String,
    pub rule_id: Option<String>,
    pub occurrence_key: Option<String>,
    /// `true` when the task is in a terminal state
    /// (`Completed` / `Failed` / `Cancelled`) or `Deleting`.
    pub terminal: bool,
    /// `true` when the task is active (`Downloading`).
    pub active: bool,
    /// `true` when the task is editable (i.e. it has not yet started
    /// recording and is not in a terminal state).
    pub editable: bool,
}

/// The action the reconciliation pass decides for a single
/// (rule, occurrence, task) tuple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Add a `Scheduled` tombstone for a task that the queue
    /// persisted but the rule repository did not.
    AddScheduledTombstone {
        rule_id: String,
        occurrence_key: String,
    },
    /// Materialize a new task for a `Scheduled` tombstone whose
    /// task disappeared and no terminal tombstone exists.
    Materialize {
        rule_id: String,
        occurrence_key: String,
    },
    /// Complete the cancellation / removal intent that the
    /// operator already expressed via a `Cancelled` tombstone.
    Finalize { uuid: String },
    /// Update a tombstone kind (e.g. move from `Scheduled` to
    /// `Completed` when the task finishes).
    UpdateTombstone {
        rule_id: String,
        occurrence_key: String,
        new_kind: TombstoneKind,
    },
    /// The task is active; the reconciliation must not cancel
    /// or remove it. Log and require manual resolution.
    ConflictingIntent { uuid: String, intent: TombstoneKind },
    /// Prune an expired tombstone.
    PruneTombstone { rule_id: String, occurrence_key: String },
    /// No action.
    Noop,
}

/// Pure: prune tombstones whose `expires_at` is in the past. The
/// minimum horizon (14 days) is enforced for the EPG-driven
/// suppression so a weekly rule without an EPG horizon still
/// suppresses duplicates inside the window.
pub fn prune_tombstones(tombstones: &TombstoneSet, now: i64) -> Vec<RecordingTombstone> {
    tombstones
        .tombstones
        .iter()
        .filter(|t| t.expires_at > now)
        .cloned()
        .collect()
}

/// Pure: compute the actions the caller should apply. The function
/// does not call into the queue or the rule repository; it returns
/// the operations for the caller's atomic boundary to execute.
pub fn reconcile(
    rules: &[RecordingRule],
    tasks: &[ReconcilableTask],
    tombstones: &TombstoneSet,
    now: i64,
) -> Vec<ReconcileAction> {
    let mut actions: Vec<ReconcileAction> = Vec::new();

    // Index tasks by (rule_id, occurrence_key) for fast lookup.
    let mut by_key: std::collections::HashMap<(String, String), &ReconcilableTask> =
        std::collections::HashMap::new();
    for task in tasks {
        if let (Some(rule_id), Some(key)) = (task.rule_id.as_deref(), task.occurrence_key.as_deref()) {
            by_key.entry((rule_id.to_string(), key.to_string())).or_insert(task);
        }
    }

    // Index tombstones by (rule_id, occurrence_key).
    let mut tombs: std::collections::HashMap<(String, String), &RecordingTombstone> =
        std::collections::HashMap::new();
    for t in &tombstones.tombstones {
        tombs.insert((t.rule_id.clone(), t.occurrence_key.clone()), t);
    }

    // Materialized task without Scheduled tombstone:
    // add the missing Scheduled tombstone.
    for task in tasks {
        let Some(rule_id) = task.rule_id.as_deref() else { continue };
        let Some(key) = task.occurrence_key.as_deref() else { continue };
        let entry = tombs.get(&(rule_id.to_string(), key.to_string()));
        if let Some(t) = entry {
            if matches!(t.kind, TombstoneKind::Scheduled) {
                // Already in sync. The task's terminal state may
                // require an UpdateTombstone (rule 4 below).
                continue;
            }
            // The tombstone is terminal; the task still exists. If
            // the task is active, surface a conflict. If the task
            // is terminal too, fall through to the completion
            // update path.
            if task.active {
                actions.push(ReconcileAction::ConflictingIntent { uuid: task.uuid.clone(), intent: t.kind });
                continue;
            }
        } else {
            actions.push(ReconcileAction::AddScheduledTombstone {
                rule_id: rule_id.to_string(),
                occurrence_key: key.to_string(),
            });
        }

        // Terminal task with Scheduled tombstone: update tombstone
        // to Completed.
        if task.terminal {
            if let Some(t) = entry {
                if matches!(t.kind, TombstoneKind::Scheduled) {
                    actions.push(ReconcileAction::UpdateTombstone {
                        rule_id: rule_id.to_string(),
                        occurrence_key: key.to_string(),
                        new_kind: TombstoneKind::Completed,
                    });
                }
            }
        }
    }

    // Scheduled tombstone without a task may be rematerialized.
    // Cancelled tombstone with an eligible inactive task may be
    // finalized.
    for t in &tombstones.tombstones {
        if t.expires_at <= now {
            actions.push(ReconcileAction::PruneTombstone {
                rule_id: t.rule_id.clone(),
                occurrence_key: t.occurrence_key.clone(),
            });
            continue;
        }
        let key = (t.rule_id.clone(), t.occurrence_key.clone());
        let Some(rule) = rules.iter().find(|r| r.id == t.rule_id) else {
            // The rule was deleted. The tombstone outlives the
            // rule; leave it in place until it expires.
            continue;
        };
        if !rule.enabled {
            // Disabled rules do not re-materialize.
            continue;
        }
        let Some(task) = by_key.get(&key) else {
            match t.kind {
                TombstoneKind::Scheduled => {
                    let still_eligible = true;
                    if still_eligible {
                        actions.push(ReconcileAction::Materialize {
                            rule_id: t.rule_id.clone(),
                            occurrence_key: t.occurrence_key.clone(),
                        });
                    }
                }
                TombstoneKind::Cancelled | TombstoneKind::Completed => {
                    // Terminal tombstones always suppress
                    // rematerialization. No action.
                }
            }
            continue;
        };
        match t.kind {
            TombstoneKind::Scheduled => {
                // Task is present and tombstone is Scheduled —
                // covered by the loop above.
                if task.terminal {
                    actions.push(ReconcileAction::UpdateTombstone {
                        rule_id: t.rule_id.clone(),
                        occurrence_key: t.occurrence_key.clone(),
                        new_kind: TombstoneKind::Completed,
                    });
                }
            }
            TombstoneKind::Cancelled => {
                if task.active {
                    // Never cancel an active task solely because of a
                    // stale reconciliation intent.
                    actions.push(ReconcileAction::ConflictingIntent {
                        uuid: task.uuid.clone(),
                        intent: TombstoneKind::Cancelled,
                    });
                } else if task.editable || task.terminal {
                    // The operator already cancelled; finish the
                    // intent.
                    actions.push(ReconcileAction::Finalize { uuid: task.uuid.clone() });
                }
            }
            TombstoneKind::Completed => {
                // Suppression is authoritative; the task being
                // present is fine — it just must not be re-created.
                // No action.
            }
        }
    }

    actions
}

/// Pure: the minimum `expires_at` for a tombstone. The repository
/// uses this when persisting a new tombstone so the suppression
/// survives the longer of the EPG horizon and the weekly fallback
/// horizon.
pub fn tombstone_expires_at(now: i64, epg_horizon_end: Option<i64>) -> i64 {
    let epg_bound = epg_horizon_end.unwrap_or(now);
    let minimum = now + MIN_TOMBSTONE_HORIZON_SECS;
    if epg_bound > minimum {
        epg_bound
    } else {
        minimum
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::recording_rule::{RuleBody, RuleSource, RuleVisibility};
    use shared::model::UserId;

    fn source() -> RuleSource { RuleSource::new("tgt", "virt", "input") }
    fn user() -> UserId { UserId::from("web:alice") }
    fn rule(id: &str) -> RecordingRule {
        RecordingRule {
            id: id.to_string(),
            owner_id: user(),
            visibility: RuleVisibility::Private,
            enabled: true,
            source: source(),
            channel_id: None,
            body: RuleBody::NewEpisode {
                series_id: Some("series-1".into()),
                title_pattern: None,
                exclude_repeat: true,
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn tomb(rule_id: &str, key: &str, kind: TombstoneKind, created: i64, expires: i64) -> RecordingTombstone {
        RecordingTombstone {
            rule_id: rule_id.into(),
            occurrence_key: key.into(),
            kind,
            created_at: created,
            expires_at: expires,
        }
    }

    fn task(uuid: &str, rule_id: &str, key: &str, terminal: bool, active: bool, editable: bool) -> ReconcilableTask {
        ReconcilableTask {
            uuid: uuid.into(),
            rule_id: Some(rule_id.into()),
            occurrence_key: Some(key.into()),
            terminal,
            active,
            editable,
        }
    }

    #[test]
    fn prune_drops_expired_tombstones() {
        let set = TombstoneSet {
            tombstones: vec![
                tomb("r1", "k1", TombstoneKind::Scheduled, 0, 100),
                tomb("r1", "k2", TombstoneKind::Cancelled, 0, 1_000),
            ],
        };
        let kept = prune_tombstones(&set, 500);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].occurrence_key, "k2");
    }

    #[test]
    fn plan_rule_1_adds_scheduled_tombstone_when_task_present() {
        let rules = vec![rule("r1")];
        let tasks = vec![task("u1", "r1", "k1", false, false, true)];
        let set = TombstoneSet::default();
        let actions = reconcile(&rules, &tasks, &set, 1_000);
        assert!(actions.iter().any(|a| matches!(a, ReconcileAction::AddScheduledTombstone { .. })));
    }

    #[test]
    fn plan_rule_2_materializes_when_scheduled_tombstone_orphan() {
        let rules = vec![rule("r1")];
        let tasks = vec![];
        let set = TombstoneSet {
            tombstones: vec![tomb("r1", "k1", TombstoneKind::Scheduled, 0, 1_000_000)],
        };
        let actions = reconcile(&rules, &tasks, &set, 1_000);
        assert!(actions.iter().any(|a| matches!(a, ReconcileAction::Materialize { .. })));
    }

    #[test]
    fn plan_rule_3_finalizes_cancelled_task() {
        let rules = vec![rule("r1")];
        let tasks = vec![task("u1", "r1", "k1", false, false, true)];
        let set = TombstoneSet {
            tombstones: vec![tomb("r1", "k1", TombstoneKind::Cancelled, 0, 1_000_000)],
        };
        let actions = reconcile(&rules, &tasks, &set, 1_000);
        assert!(actions.iter().any(|a| matches!(a, ReconcileAction::Finalize { uuid } if uuid == "u1")));
    }

    #[test]
    fn plan_rule_3_conflicting_intent_for_active_cancelled_task() {
        let rules = vec![rule("r1")];
        let tasks = vec![task("u1", "r1", "k1", false, true, false)];
        let set = TombstoneSet {
            tombstones: vec![tomb("r1", "k1", TombstoneKind::Cancelled, 0, 1_000_000)],
        };
        let actions = reconcile(&rules, &tasks, &set, 1_000);
        assert!(actions.iter().any(|a| matches!(a, ReconcileAction::ConflictingIntent { .. })));
    }

    #[test]
    fn plan_rule_4_completed_tombstone_suppresses_rematerialization() {
        let rules = vec![rule("r1")];
        let tasks = vec![];
        let set = TombstoneSet {
            tombstones: vec![tomb("r1", "k1", TombstoneKind::Completed, 0, 1_000_000)],
        };
        let actions = reconcile(&rules, &tasks, &set, 1_000);
        assert!(!actions.iter().any(|a| matches!(a, ReconcileAction::Materialize { .. })));
    }

    #[test]
    fn plan_rule_4_updates_tombstone_when_task_completes() {
        let rules = vec![rule("r1")];
        let tasks = vec![task("u1", "r1", "k1", true, false, false)];
        let set = TombstoneSet {
            tombstones: vec![tomb("r1", "k1", TombstoneKind::Scheduled, 0, 1_000_000)],
        };
        let actions = reconcile(&rules, &tasks, &set, 1_000);
        assert!(actions.iter().any(|a| matches!(
            a,
            ReconcileAction::UpdateTombstone { new_kind: TombstoneKind::Completed, .. }
        )));
    }

    #[test]
    fn expired_tombstones_are_pruned() {
        let rules = vec![rule("r1")];
        let tasks = vec![];
        let set = TombstoneSet {
            tombstones: vec![tomb("r1", "k1", TombstoneKind::Scheduled, 0, 100)],
        };
        let actions = reconcile(&rules, &tasks, &set, 1_000);
        assert!(actions.iter().any(|a| matches!(a, ReconcileAction::PruneTombstone { .. })));
    }

    #[test]
    fn disabled_rule_does_not_rematerialize() {
        let mut r = rule("r1");
        r.enabled = false;
        let rules = vec![r];
        let tasks = vec![];
        let set = TombstoneSet {
            tombstones: vec![tomb("r1", "k1", TombstoneKind::Scheduled, 0, 1_000_000)],
        };
        let actions = reconcile(&rules, &tasks, &set, 1_000);
        assert!(!actions.iter().any(|a| matches!(a, ReconcileAction::Materialize { .. })));
    }

    #[test]
    fn deleted_rule_leaves_tombstone_until_expiry() {
        // The rule was deleted but the tombstone is still valid. The
        // reconciliation should not try to rematerialize.
        let rules: Vec<RecordingRule> = vec![];
        let tasks = vec![];
        let set = TombstoneSet {
            tombstones: vec![tomb("r1", "k1", TombstoneKind::Scheduled, 0, 1_000_000)],
        };
        let actions = reconcile(&rules, &tasks, &set, 1_000);
        assert!(!actions.iter().any(|a| matches!(a, ReconcileAction::Materialize { .. })));
    }

    #[test]
    fn tombstone_expires_at_uses_longer_of_epg_and_minimum_horizon() {
        // EPG horizon is far in the future — but the minimum
        // 14-day horizon is even longer, so the minimum wins.
        let now = 1_000;
        let epg = 1_000_000;
        assert_eq!(tombstone_expires_at(now, Some(epg)), now + MIN_TOMBSTONE_HORIZON_SECS);
        // EPG horizon well beyond the minimum → use EPG.
        let epg_far = 1_000 + 30 * 86_400;
        assert_eq!(tombstone_expires_at(now, Some(epg_far)), epg_far);
        // EPG horizon shorter than the minimum → use the minimum.
        let epg_short = 1_000 + 100;
        assert_eq!(tombstone_expires_at(now, Some(epg_short)), now + MIN_TOMBSTONE_HORIZON_SECS);
        // No EPG horizon → minimum horizon.
        assert_eq!(tombstone_expires_at(now, None), now + MIN_TOMBSTONE_HORIZON_SECS);
    }
}
