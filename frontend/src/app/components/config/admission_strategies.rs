use crate::{i18n::YewI18n, utils::t_safe};
use shared::model::{AdmissionStrategy, StreamConfigDto};
use std::str::FromStr;

const LABEL_ADMISSION_STRATEGY_EVICT_USER_SAME_IP_OLDEST: &str = "LABEL.ADMISSION_STRATEGY_EVICT_USER_SAME_IP_OLDEST";
const LABEL_ADMISSION_STRATEGY_EVICT_USER_SAME_IP_LATEST: &str = "LABEL.ADMISSION_STRATEGY_EVICT_USER_SAME_IP_LATEST";
const LABEL_ADMISSION_STRATEGY_EVICT_USER_OLDEST: &str = "LABEL.ADMISSION_STRATEGY_EVICT_USER_OLDEST";
const LABEL_ADMISSION_STRATEGY_EVICT_USER_LATEST: &str = "LABEL.ADMISSION_STRATEGY_EVICT_USER_LATEST";
const LABEL_ADMISSION_STRATEGY_GRACE_INSTANT_STREAM: &str = "LABEL.ADMISSION_STRATEGY_GRACE_INSTANT_STREAM";
const LABEL_ADMISSION_STRATEGY_GRACE_HOLD_STREAM: &str = "LABEL.ADMISSION_STRATEGY_GRACE_HOLD_STREAM";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AdmissionStrategiesDto {
    pub strategies: Option<Vec<String>>,
}

pub(crate) fn admission_strategy_label_key(strategy: AdmissionStrategy) -> &'static str {
    match strategy {
        AdmissionStrategy::EvictUserSameIpOldest => LABEL_ADMISSION_STRATEGY_EVICT_USER_SAME_IP_OLDEST,
        AdmissionStrategy::EvictUserSameIpLatest => LABEL_ADMISSION_STRATEGY_EVICT_USER_SAME_IP_LATEST,
        AdmissionStrategy::EvictUserOldest => LABEL_ADMISSION_STRATEGY_EVICT_USER_OLDEST,
        AdmissionStrategy::EvictUserLatest => LABEL_ADMISSION_STRATEGY_EVICT_USER_LATEST,
        AdmissionStrategy::GraceInstantStream => LABEL_ADMISSION_STRATEGY_GRACE_INSTANT_STREAM,
        AdmissionStrategy::GraceHoldStream => LABEL_ADMISSION_STRATEGY_GRACE_HOLD_STREAM,
    }
}

pub(crate) fn admission_strategy_label(translate: &YewI18n, strategy: AdmissionStrategy) -> String {
    t_safe(translate, admission_strategy_label_key(strategy)).unwrap_or_else(|| match strategy {
        AdmissionStrategy::EvictUserSameIpOldest => "Evict same-IP oldest stream".to_string(),
        AdmissionStrategy::EvictUserSameIpLatest => "Evict same-IP latest stream".to_string(),
        AdmissionStrategy::EvictUserOldest => "Evict user oldest stream".to_string(),
        AdmissionStrategy::EvictUserLatest => "Evict user latest stream".to_string(),
        AdmissionStrategy::GraceInstantStream => "Grace instant stream".to_string(),
        AdmissionStrategy::GraceHoldStream => "Grace hold stream".to_string(),
    })
}

fn is_grace_strategy(strategy: AdmissionStrategy) -> bool {
    matches!(strategy, AdmissionStrategy::GraceInstantStream | AdmissionStrategy::GraceHoldStream)
}

pub(crate) fn is_grace_strategy_tag(tag: &str) -> bool {
    tag.trim().starts_with("grace_")
}

pub(crate) fn admission_strategy_tags(strategies: Option<&Vec<AdmissionStrategy>>) -> Option<Vec<String>> {
    strategies.map(|entries| entries.iter().map(|entry| (*entry).to_string()).collect())
}

pub(crate) fn parse_admission_strategy_tags(tags: Option<&[String]>) -> Option<Vec<AdmissionStrategy>> {
    let tags = tags?;
    let mut parsed = Vec::new();
    for tag in tags {
        if let Ok(strategy) = AdmissionStrategy::from_str(tag) {
            if !parsed.contains(&strategy) {
                parsed.push(strategy);
            }
        }
    }
    Some(parsed)
}

pub(crate) fn filter_disabled_grace_strategy_tags(tags: Vec<String>, grace_period_millis: u64) -> Vec<String> {
    if grace_period_millis == 0 {
        tags.into_iter().filter(|tag| !is_grace_strategy_tag(tag)).collect()
    } else {
        tags
    }
}

pub(crate) fn filter_disabled_grace_strategies(
    strategies: Option<Vec<AdmissionStrategy>>,
    grace_period_millis: u64,
) -> Option<Vec<AdmissionStrategy>> {
    strategies.map(|entries| {
        if grace_period_millis == 0 {
            entries.into_iter().filter(|strategy| !is_grace_strategy(*strategy)).collect()
        } else {
            entries
        }
    })
}

pub(crate) fn admission_strategy_tag_label(translate: &YewI18n, tag: &str) -> String {
    AdmissionStrategy::from_str(tag)
        .ok()
        .map_or_else(|| tag.to_string(), |strategy| admission_strategy_label(translate, strategy))
}

pub(crate) fn legacy_admission_strategy_tags(stream: &StreamConfigDto) -> Vec<String> {
    if stream.grace_period_millis == 0 {
        Vec::new()
    } else {
        vec![(if stream.grace_period_hold_stream {
            AdmissionStrategy::GraceHoldStream
        } else {
            AdmissionStrategy::GraceInstantStream
        })
        .to_string()]
    }
}

pub(crate) fn displayed_admission_strategy_tags(
    state: &AdmissionStrategiesDto,
    stream: &StreamConfigDto,
) -> Vec<String> {
    filter_disabled_grace_strategy_tags(
        state.strategies.clone().unwrap_or_else(|| {
            admission_strategy_tags(stream.admission_strategies.as_ref())
                .unwrap_or_else(|| legacy_admission_strategy_tags(stream))
        }),
        stream.grace_period_millis,
    )
}

pub(crate) fn available_admission_strategies(
    selected_tags: &[String],
    grace_period_millis: u64,
) -> Vec<AdmissionStrategy> {
    let has_grace = selected_tags.iter().filter_map(|tag| AdmissionStrategy::from_str(tag).ok()).any(is_grace_strategy);

    [
        AdmissionStrategy::EvictUserSameIpOldest,
        AdmissionStrategy::EvictUserSameIpLatest,
        AdmissionStrategy::EvictUserOldest,
        AdmissionStrategy::EvictUserLatest,
        AdmissionStrategy::GraceInstantStream,
        AdmissionStrategy::GraceHoldStream,
    ]
    .into_iter()
    .filter(|strategy| {
        let tag = (*strategy).to_string();
        let grace_available = grace_period_millis > 0 || !is_grace_strategy(*strategy);
        !selected_tags.iter().any(|selected| selected == &tag)
            && grace_available
            && (!has_grace || !is_grace_strategy(*strategy))
    })
    .collect()
}

pub(crate) fn add_admission_strategy_tag(current: &[String], strategy: AdmissionStrategy) -> Vec<String> {
    let mut next = current.to_vec();
    let tag = strategy.to_string();
    if next.iter().any(|selected| selected == &tag) {
        return next;
    }
    // When adding a same-IP eviction rule, insert it before any broader user-wide
    // rule (if present) so the backend ordering validation is satisfied.
    let is_narrower =
        matches!(strategy, AdmissionStrategy::EvictUserSameIpOldest | AdmissionStrategy::EvictUserSameIpLatest);
    if is_narrower {
        let broader_oldest = AdmissionStrategy::EvictUserOldest.to_string();
        let broader_latest = AdmissionStrategy::EvictUserLatest.to_string();

        let earliest_pos = next.iter().position(|t| t == &broader_oldest || t == &broader_latest);
        if let Some(pos) = earliest_pos {
            next.insert(pos, tag);
            return next;
        }
    }
    next.push(tag);
    next
}

pub(crate) fn remove_admission_strategy_tag(current: &[String], index: usize) -> Vec<String> {
    let mut next = current.to_vec();
    if index < next.len() {
        next.remove(index);
    }
    next
}

pub(crate) fn move_admission_strategy_tag(current: &[String], index: usize, delta: isize) -> Vec<String> {
    let mut next = current.to_vec();
    if let Some(target_index) = index.checked_add_signed(delta) {
        if index < next.len() && target_index < next.len() {
            next.swap(index, target_index);
            // Reject the move if it would create an invalid ordering (broader before narrower).
            let strategy_dtos: Vec<AdmissionStrategy> =
                next.iter().filter_map(|t| AdmissionStrategy::from_str(t).ok()).collect();
            if !shared::model::is_valid_admission_strategy_order(&strategy_dtos) {
                next.swap(index, target_index); // revert
            }
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_strategy_tags_roundtrip() {
        let tags = admission_strategy_tags(Some(&vec![
            AdmissionStrategy::EvictUserOldest,
            AdmissionStrategy::GraceHoldStream,
        ]))
        .unwrap_or_default();
        assert_eq!(
            parse_admission_strategy_tags(Some(&tags)),
            Some(vec![AdmissionStrategy::EvictUserOldest, AdmissionStrategy::GraceHoldStream,])
        );
    }

    #[test]
    fn admission_strategy_tags_roundtrip_evict_user_latest() {
        let tags = admission_strategy_tags(Some(&vec![AdmissionStrategy::EvictUserLatest])).unwrap_or_default();
        assert_eq!(parse_admission_strategy_tags(Some(&tags)), Some(vec![AdmissionStrategy::EvictUserLatest]));
    }

    #[test]
    fn invalid_admission_strategy_tags_are_ignored() {
        let tags = vec!["evict_user_latest".to_string(), "not-a-strategy".to_string(), "evict_user_latest".to_string()];
        assert_eq!(parse_admission_strategy_tags(Some(&tags)), Some(vec![AdmissionStrategy::EvictUserLatest]));
    }

    #[test]
    fn displayed_admission_strategies_fall_back_to_legacy_grace() {
        let state = AdmissionStrategiesDto::default();
        let stream = StreamConfigDto {
            grace_period_millis: 2_000,
            grace_period_hold_stream: true,
            ..StreamConfigDto::default()
        };

        assert_eq!(displayed_admission_strategy_tags(&state, &stream), vec!["grace_hold_stream".to_string()]);
    }

    #[test]
    fn available_admission_strategies_hide_second_grace_option() {
        let available = available_admission_strategies(&["grace_hold_stream".to_string()], 2_000);
        assert!(!available.contains(&AdmissionStrategy::GraceInstantStream));
        assert!(!available.contains(&AdmissionStrategy::GraceHoldStream));
        assert!(available.contains(&AdmissionStrategy::EvictUserSameIpOldest));
        assert!(available.contains(&AdmissionStrategy::EvictUserOldest));
    }

    #[test]
    fn available_admission_strategies_hide_grace_when_disabled() {
        let available = available_admission_strategies(&[], 0);
        assert!(!available.contains(&AdmissionStrategy::GraceInstantStream));
        assert!(!available.contains(&AdmissionStrategy::GraceHoldStream));
        assert!(available.contains(&AdmissionStrategy::EvictUserSameIpOldest));
        assert!(available.contains(&AdmissionStrategy::EvictUserSameIpLatest));
        assert!(available.contains(&AdmissionStrategy::EvictUserOldest));
        assert!(available.contains(&AdmissionStrategy::EvictUserLatest));
    }

    #[test]
    fn displayed_admission_strategies_hide_disabled_grace_tags() {
        let state = AdmissionStrategiesDto { strategies: Some(vec!["grace_hold_stream".to_string()]) };
        let stream = StreamConfigDto { grace_period_millis: 0, ..StreamConfigDto::default() };

        assert_eq!(displayed_admission_strategy_tags(&state, &stream), Vec::<String>::new());
    }

    #[test]
    fn filtered_admission_strategies_drop_grace_when_disabled() {
        let parsed = parse_admission_strategy_tags(Some(&[
            "evict_user_same_ip_oldest".to_string(),
            "grace_hold_stream".to_string(),
        ]));

        assert_eq!(filter_disabled_grace_strategies(parsed, 0), Some(vec![AdmissionStrategy::EvictUserSameIpOldest]));
    }

    #[test]
    fn add_admission_strategy_enforces_narrower_before_broader() {
        let current = vec!["evict_user_oldest".to_string()];
        let new_tags = add_admission_strategy_tag(&current, AdmissionStrategy::EvictUserSameIpOldest);
        assert_eq!(new_tags, vec!["evict_user_same_ip_oldest".to_string(), "evict_user_oldest".to_string()]);
    }

    #[test]
    fn add_admission_strategy_inserts_between_existing_narrower_and_broader_rules() {
        let current =
            vec![AdmissionStrategy::EvictUserSameIpOldest.to_string(), AdmissionStrategy::EvictUserOldest.to_string()];

        let new_tags = add_admission_strategy_tag(&current, AdmissionStrategy::EvictUserSameIpLatest);

        assert_eq!(
            new_tags,
            vec![
                AdmissionStrategy::EvictUserSameIpOldest.to_string(),
                AdmissionStrategy::EvictUserSameIpLatest.to_string(),
                AdmissionStrategy::EvictUserOldest.to_string(),
            ]
        );
    }

    #[test]
    fn move_admission_strategy_reverts_invalid_order() {
        let current = vec!["evict_user_same_ip_oldest".to_string(), "evict_user_oldest".to_string()];
        // Attempt to move broader EvictUserOldest up before narrower EvictUserSameIpOldest
        let next = move_admission_strategy_tag(&current, 1, -1);
        assert_eq!(next, current);
    }
}
