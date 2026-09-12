use crate::{
    app::{map_sources_to_playlist_rows, InputRow},
    model::{InputUpdateCapabilities, InputUpdateCapabilitiesExt},
};
use shared::model::{InputRefreshPolicy, InputType, PlaylistUpdateStatusDto, SourcesConfigDto};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// Target rendered in one input-update card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputUpdateCardTarget {
    pub id: u16,
    pub name: String,
}

/// Frontend read model for one configured input, including inputs without a manual action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputUpdateCardModel {
    pub input_id: u16,
    pub input_name: Arc<str>,
    pub input_type: InputType,
    pub capabilities: InputUpdateCapabilities,
    pub targets: Vec<InputUpdateCardTarget>,
    pub last_update_at: Option<u64>,
}

impl InputUpdateCardModel {
    /// Builds the deterministic last-update presentation for a supplied clock value.
    #[must_use]
    pub fn last_update(&self, now: u64) -> LastUpdateViewModel { last_update_view_model(self.last_update_at, now) }
}

/// Local, non-persisted controls for one input card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputUpdateCardState {
    pub selected_target_ids: Vec<u16>,
    pub policy: InputRefreshPolicy,
}

/// Localization-ready relative representation of a persisted update timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LastUpdateRelative {
    Never,
    JustNow,
    Minutes(u64),
    Hours(u64),
    LocalDateTime(u64),
}

/// Relative value plus the source timestamp for a separately formatted local-time title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastUpdateViewModel {
    pub relative: LastUpdateRelative,
    pub title_timestamp: Option<u64>,
}

/// Classifies a persisted timestamp without reading the clock or browser locale.
#[must_use]
pub fn last_update_view_model(last_update_at: Option<u64>, now: u64) -> LastUpdateViewModel {
    let relative = match last_update_at {
        None => LastUpdateRelative::Never,
        Some(timestamp) => {
            let elapsed = now.saturating_sub(timestamp);
            match elapsed {
                0..=59 => LastUpdateRelative::JustNow,
                60..=3_599 => LastUpdateRelative::Minutes(elapsed / 60),
                3_600..=86_399 => LastUpdateRelative::Hours(elapsed / 3_600),
                _ => LastUpdateRelative::LocalDateTime(timestamp),
            }
        }
    };
    LastUpdateViewModel { relative, title_timestamp: last_update_at }
}

/// Builds one card per configured input ID in configuration order.
/// Aliases are accounts of their root input, not independently configured inputs.
#[must_use]
pub fn build_input_update_card_models(
    sources: &SourcesConfigDto,
    update_status: &PlaylistUpdateStatusDto,
) -> Vec<InputUpdateCardModel> {
    let rows = map_sources_to_playlist_rows(sources);
    let mut targets_by_input_id = HashMap::<u16, Vec<InputUpdateCardTarget>>::new();
    let mut target_ids_by_input_id = HashMap::<u16, HashSet<u16>>::new();

    for (source_inputs, source_targets) in rows.iter() {
        for row in source_inputs {
            let InputRow::Input(input) = &**row else {
                continue;
            };
            let targets = targets_by_input_id.entry(input.id).or_default();
            let seen_target_ids = target_ids_by_input_id.entry(input.id).or_default();
            for target in source_targets {
                if seen_target_ids.insert(target.id) {
                    targets.push(InputUpdateCardTarget { id: target.id, name: target.name.clone() });
                }
            }
        }
    }

    let last_update_by_input_id =
        update_status.inputs.iter().map(|status| (status.input_id, status.last_update)).collect::<HashMap<_, _>>();

    let mut cards = Vec::with_capacity(sources.inputs.len());
    let mut seen_input_ids = HashSet::with_capacity(sources.inputs.len());
    for input in &sources.inputs {
        if !seen_input_ids.insert(input.id) {
            continue;
        }
        cards.push(InputUpdateCardModel {
            input_id: input.id,
            input_name: Arc::clone(&input.name),
            input_type: input.input_type,
            capabilities: InputUpdateCapabilities::for_input(input),
            targets: targets_by_input_id.remove(&input.id).unwrap_or_default(),
            last_update_at: last_update_by_input_id.get(&input.id).copied().flatten(),
        });
    }
    cards
}

#[cfg(test)]
mod tests {
    use super::{
        build_input_update_card_models, last_update_view_model, InputUpdateCapabilities, InputUpdateCapabilitiesExt,
        InputUpdateCardModel, LastUpdateRelative,
    };
    use shared::{
        model::{
            ConfigInputAliasDto, ConfigInputDto, ConfigSourceDto, ConfigTargetDto, InputPlaylistUpdateStatusDto,
            InputRefreshPolicy, InputType, PlaylistUpdateStatusDto, SourcesConfigDto,
        },
        utils::Internable,
    };
    use std::sync::Arc;

    fn input(id: u16, name: &str, input_type: InputType) -> ConfigInputDto {
        ConfigInputDto { id, name: name.intern(), input_type, enabled: true, ..ConfigInputDto::default() }
    }

    fn target(id: u16, name: &str) -> ConfigTargetDto {
        ConfigTargetDto { id, name: name.to_string(), ..ConfigTargetDto::default() }
    }

    fn card_fixture() -> (SourcesConfigDto, PlaylistUpdateStatusDto) {
        let sources = SourcesConfigDto {
            inputs: vec![
                input(30, "zulu", InputType::Xtream),
                input(10, "alpha", InputType::Stalker),
                input(40, "without-targets", InputType::Xtream),
                input(50, "m3u", InputType::M3u),
            ],
            sources: vec![
                ConfigSourceDto {
                    inputs: vec!["alpha".intern()],
                    targets: vec![target(2, "second"), target(1, "shared-name")],
                },
                ConfigSourceDto { inputs: vec!["zulu".intern()], targets: vec![target(3, "third")] },
                ConfigSourceDto {
                    inputs: vec!["alpha".intern()],
                    targets: vec![target(2, "duplicate-id"), target(4, "shared-name")],
                },
                ConfigSourceDto { inputs: vec!["without-targets".intern()], targets: Vec::new() },
                ConfigSourceDto { inputs: vec!["m3u".intern()], targets: vec![target(5, "m3u-target")] },
            ],
            ..SourcesConfigDto::default()
        };
        let status = PlaylistUpdateStatusDto {
            inputs: vec![
                InputPlaylistUpdateStatusDto {
                    input_id: 10,
                    last_update: Some(700),
                    last_input_update: None,
                    clusters: Vec::new(),
                },
                InputPlaylistUpdateStatusDto {
                    input_id: 30,
                    last_update: Some(500),
                    last_input_update: None,
                    clusters: Vec::new(),
                },
            ],
            ..PlaylistUpdateStatusDto::default()
        };
        (sources, status)
    }

    #[test]
    fn input_update_card_model_preserves_input_and_target_configuration_order() {
        let (sources, status) = card_fixture();

        let cards = build_input_update_card_models(&sources, &status);

        assert_eq!(cards.iter().map(|card| card.input_id).collect::<Vec<_>>(), vec![30, 10, 40, 50]);
        assert_eq!(cards[1].targets.iter().map(|target| target.id).collect::<Vec<_>>(), vec![2, 1, 4]);
        assert_eq!(cards[0].input_type, InputType::Xtream);
        assert_eq!(cards[1].input_type, InputType::Stalker);
        assert_eq!(cards[3].input_type, InputType::M3u);
        assert_eq!(cards[3].targets.iter().map(|target| target.id).collect::<Vec<_>>(), vec![5]);
    }

    #[test]
    fn input_update_card_model_deduplicates_only_by_stable_target_id() {
        let (sources, status) = card_fixture();

        let cards = build_input_update_card_models(&sources, &status);
        let alpha = cards.iter().find(|card| card.input_id == 10).expect("alpha card");

        assert_eq!(
            alpha.targets.iter().map(|target| target.name.as_str()).collect::<Vec<_>>(),
            ["second", "shared-name", "shared-name",]
        );
    }

    #[test]
    fn input_update_card_model_keeps_updateable_input_from_source_without_targets() {
        let (sources, status) = card_fixture();

        let cards = build_input_update_card_models(&sources, &status);
        let card = cards.iter().find(|card| card.input_id == 40).expect("card without targets");

        assert!(card.targets.is_empty());
    }

    #[test]
    fn input_update_card_model_keeps_updateable_input_without_source_reference() {
        let sources = SourcesConfigDto {
            inputs: vec![input(7, "unreferenced", InputType::Xtream)],
            sources: Vec::new(),
            ..SourcesConfigDto::default()
        };

        let cards = build_input_update_card_models(&sources, &PlaylistUpdateStatusDto::default());

        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].input_id, 7);
        assert_eq!(cards[0].input_type, InputType::Xtream);
        assert!(cards[0].targets.is_empty());
    }

    #[test]
    fn input_update_card_model_visibility_is_independent_of_action_capabilities() {
        let mut disabled = input(2, "disabled", InputType::Xtream);
        disabled.enabled = false;
        let mut sources = SourcesConfigDto {
            inputs: vec![
                input(1, "xtream", InputType::Xtream),
                disabled,
                input(3, "xtream-batch", InputType::XtreamBatch),
                input(4, "stalker-batch", InputType::StalkerBatch),
                input(5, "m3u", InputType::M3u),
                input(6, "stalker", InputType::Stalker),
                input(7, "m3u-batch", InputType::M3uBatch),
                input(8, "staged", InputType::Staged),
                input(9, "library", InputType::Library),
                input(10, "plex", InputType::Plex),
                input(11, "jellyfin", InputType::Jellyfin),
                input(12, "emby", InputType::Emby),
            ],
            sources: Vec::new(),
            ..SourcesConfigDto::default()
        };
        sources.sources.push(ConfigSourceDto {
            inputs: sources.inputs.iter().map(|input| Arc::clone(&input.name)).collect(),
            targets: vec![target(2, "same-name"), target(1, "same-name"), target(2, "duplicate-id")],
        });

        let cards = build_input_update_card_models(&sources, &PlaylistUpdateStatusDto::default());

        assert_eq!(cards.iter().map(|card| card.input_id).collect::<Vec<_>>(), (1..=12).collect::<Vec<_>>());
        assert_eq!(cards[1].capabilities, InputUpdateCapabilities::Disabled);
        for card in &cards {
            assert_eq!(card.targets.iter().map(|target| target.id).collect::<Vec<_>>(), vec![2, 1]);
            assert_eq!(
                card.capabilities,
                InputUpdateCapabilities::for_input(&sources.inputs[usize::from(card.input_id - 1)])
            );
        }
        assert_eq!(
            cards
                .iter()
                .filter(|card| card.capabilities.supports(InputRefreshPolicy::NORMAL))
                .map(|card| card.input_id)
                .collect::<Vec<_>>(),
            vec![1, 5, 6, 10]
        );
    }

    #[test]
    fn input_update_card_model_keeps_duplicate_names_but_not_duplicate_ids_or_aliases() {
        let first = ConfigInputDto {
            aliases: Some(vec![ConfigInputAliasDto {
                id: 99,
                name: "alias".intern(),
                ..ConfigInputAliasDto::default()
            }]),
            ..input(42, "same", InputType::M3u)
        };
        let second = input(7, "same", InputType::Plex);
        let duplicate = ConfigInputDto { name: "repeat".intern(), ..first.clone() };
        let sources = SourcesConfigDto {
            inputs: vec![first, second, duplicate],
            sources: vec![
                ConfigSourceDto { inputs: vec!["same".intern(), "same".intern()], targets: vec![target(8, "one")] },
                ConfigSourceDto { inputs: vec!["repeat".intern()], targets: vec![target(9, "two")] },
            ],
            ..SourcesConfigDto::default()
        };
        let status = PlaylistUpdateStatusDto {
            inputs: vec![
                InputPlaylistUpdateStatusDto {
                    input_id: 7,
                    last_update: Some(700),
                    last_input_update: None,
                    clusters: Vec::new(),
                },
                InputPlaylistUpdateStatusDto {
                    input_id: 42,
                    last_update: Some(500),
                    last_input_update: None,
                    clusters: Vec::new(),
                },
            ],
            ..PlaylistUpdateStatusDto::default()
        };
        let cards = build_input_update_card_models(&sources, &status);
        assert_eq!(cards.iter().map(|card| card.input_id).collect::<Vec<_>>(), vec![42, 7]);
        assert_eq!(cards[0].input_name, cards[1].input_name);
        assert_eq!(cards[0].targets.iter().map(|target| target.id).collect::<Vec<_>>(), vec![8, 9]);
        assert_eq!(cards[1].targets.iter().map(|target| target.id).collect::<Vec<_>>(), vec![8]);
        assert_eq!(cards[0].last_update_at, Some(500));
        assert_eq!(cards[1].last_update_at, Some(700));
    }

    #[test]
    fn input_update_card_model_maps_status_by_stable_input_id() {
        let (sources, status) = card_fixture();
        let cards = build_input_update_card_models(&sources, &status);

        assert_eq!(cards[0].last_update_at, Some(500));
        assert_eq!(cards[1].last_update_at, Some(700));
        assert_eq!(cards[2].last_update_at, None);
    }

    #[test]
    fn last_update_covers_relative_time_boundaries() {
        let now = 100_000;
        let cases = [
            (None, LastUpdateRelative::Never),
            (Some(now), LastUpdateRelative::JustNow),
            (Some(now - 59), LastUpdateRelative::JustNow),
            (Some(now - 60), LastUpdateRelative::Minutes(1)),
            (Some(now - 3_599), LastUpdateRelative::Minutes(59)),
            (Some(now - 3_600), LastUpdateRelative::Hours(1)),
            (Some(now - 86_399), LastUpdateRelative::Hours(23)),
            (Some(now - 86_400), LastUpdateRelative::LocalDateTime(now - 86_400)),
        ];

        for (timestamp, expected) in cases {
            assert_eq!(last_update_view_model(timestamp, now).relative, expected);
        }
    }

    #[test]
    fn last_update_treats_future_clock_skew_as_just_now() {
        assert_eq!(last_update_view_model(Some(101), 100).relative, LastUpdateRelative::JustNow);
    }

    #[test]
    fn last_update_exposes_full_timestamp_separately_for_local_title_formatting() {
        let view_model = last_update_view_model(Some(12_345), 12_405);

        assert_eq!(view_model.relative, LastUpdateRelative::Minutes(1));
        assert_eq!(view_model.title_timestamp, Some(12_345));
        assert_eq!(last_update_view_model(None, 12_405).title_timestamp, None);
    }

    #[test]
    fn playlist_update_view_reclassifies_last_update_from_one_shared_clock_value() {
        let first = InputUpdateCardModel {
            input_id: 1,
            input_name: "first".intern(),
            input_type: InputType::Xtream,
            capabilities: InputUpdateCapabilities::for_input(&input(1, "first", InputType::Xtream)),
            targets: Vec::new(),
            last_update_at: Some(1_000),
        };
        let second = InputUpdateCardModel {
            input_id: 2,
            input_name: "second".intern(),
            input_type: InputType::Stalker,
            capabilities: InputUpdateCapabilities::for_input(&input(2, "second", InputType::Stalker)),
            targets: Vec::new(),
            last_update_at: Some(940),
        };

        assert_eq!(first.last_update(1_059).relative, LastUpdateRelative::JustNow);
        assert_eq!(second.last_update(1_059).relative, LastUpdateRelative::Minutes(1));
        assert_eq!(first.last_update(1_060).relative, LastUpdateRelative::Minutes(1));
        assert_eq!(second.last_update(1_060).relative, LastUpdateRelative::Minutes(2));
    }
}
