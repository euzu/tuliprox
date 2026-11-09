//! Integration coverage using the same model, controls and capabilities as the rendered cards.
use super::*;
use crate::model::{
    build_input_update_card_models, playlist_update_bulk_scope, InputUpdateCardRuntime, InputUpdateCardStatus,
};
use shared::{
    model::{
        ConfigInputAliasDto, ConfigInputDto, ConfigRenameDto, ConfigSourceDto, ConfigTargetDto,
        InputPlaylistUpdateStatusDto, InputType, ItemField, PlaylistUpdateStatusDto, SourcesConfigDto,
    },
    utils::Internable,
};
use std::collections::HashMap;

const INPUTS: [(u16, InputType, &[&str]); 11] = [
    (41, InputType::M3u, &["normal", "refresh", "force"]),
    (7, InputType::Xtream, &["normal", "refresh", "force"]),
    (19, InputType::Stalker, &["normal", "refresh", "force"]),
    (3, InputType::M3uBatch, &[]),
    (11, InputType::XtreamBatch, &[]),
    (2, InputType::StalkerBatch, &[]),
    (33, InputType::Staged, &[]),
    (15, InputType::Library, &["rescan"]),
    (4, InputType::Plex, &["normal", "refresh"]),
    (28, InputType::Jellyfin, &["normal", "refresh"]),
    (9, InputType::Emby, &["normal", "refresh"]),
];

fn sources() -> SourcesConfigDto {
    let mut inputs = INPUTS
        .iter()
        .map(|&(id, input_type, _)| ConfigInputDto {
            id,
            input_type,
            name: format!("input-{id}").intern(),
            ..ConfigInputDto::default()
        })
        .collect::<Vec<_>>();
    inputs[0].aliases = Some(vec![ConfigInputAliasDto {
        id: 1000,
        name: "account-not-a-card".intern(),
        ..ConfigInputAliasDto::default()
    }]);
    let targets = vec![
        ConfigTargetDto {
            id: 20,
            name: "same-target".to_owned(),
            filter: "Group ~ \"news\"".into(),
            rename: Some(vec![ConfigRenameDto {
                field: ItemField::Group,
                pattern: "news".to_owned(),
                new_name: "News".to_owned(),
                t_pattern: None,
            }]),
            mapping: Some(vec!["news-mapping".to_owned()]),
            ..ConfigTargetDto::default()
        },
        ConfigTargetDto {
            id: 3,
            name: "same-target".to_owned(),
            filter: "Group ~ \"sports\"".into(),
            mapping: Some(vec!["sports-mapping".to_owned()]),
            ..ConfigTargetDto::default()
        },
    ];
    let configured_sources = vec![
        ConfigSourceDto { inputs: inputs.iter().map(|input| input.name.clone()).collect(), targets: targets.clone() },
        ConfigSourceDto { inputs: vec![inputs[0].name.clone()], targets: vec![targets[0].clone()] },
        ConfigSourceDto {
            inputs: vec!["disabled".intern()],
            targets: vec![ConfigTargetDto {
                id: 22,
                name: "disabled-target-input".to_owned(),
                ..ConfigTargetDto::default()
            }],
        },
    ];
    inputs.push(ConfigInputDto {
        id: 100,
        name: "targetless".intern(),
        input_type: InputType::M3u,
        ..ConfigInputDto::default()
    });
    inputs.push(ConfigInputDto {
        id: 101,
        name: "disabled".intern(),
        enabled: false,
        input_type: InputType::Xtream,
        ..ConfigInputDto::default()
    });
    SourcesConfigDto { inputs, sources: configured_sources, ..SourcesConfigDto::default() }
}

#[test]
fn playlist_update_view_configured_cards_keep_identity_order_context_and_capability_actions() {
    let sources = sources();
    let status = PlaylistUpdateStatusDto {
        inputs: vec![
            InputPlaylistUpdateStatusDto {
                input_id: 41,
                last_update: Some(800),
                last_input_update: None,
                clusters: Vec::new(),
            },
            InputPlaylistUpdateStatusDto {
                input_id: 4,
                last_update: Some(600),
                last_input_update: None,
                clusters: Vec::new(),
            },
        ],
        ..PlaylistUpdateStatusDto::default()
    };
    let cards = build_input_update_card_models(&sources, &status);
    assert_eq!(
        cards.iter().map(|card| card.input_id).collect::<Vec<_>>(),
        [41, 7, 19, 3, 11, 2, 33, 15, 4, 28, 9, 100, 101]
    );
    for ((input_id, input_type, expected_modes), card) in INPUTS.iter().zip(&cards) {
        assert_eq!(card.input_id, *input_id);
        assert_eq!(card.input_type, *input_type);
        assert_eq!(card.targets.iter().map(|target| target.id).collect::<Vec<_>>(), [20, 3]);
        assert_eq!(card.targets[0].name, card.targets[1].name);
        let options = policy_options(card.capabilities, InputRefreshPolicy::NORMAL, str::to_owned);
        assert_eq!(options.iter().map(|option| option.id.as_str()).collect::<Vec<_>>(), *expected_modes);
        let state = InputUpdateCardInteractionState::from_model(card);
        assert_eq!(state.controls.selected_target_ids, [20, 3]);
        assert_eq!(state.controls.policy, InputRefreshPolicy::NORMAL);
        assert_eq!(can_start_update(&state, card, true), card.capabilities.rebuilds_targets());
        assert!(!can_start_update(&state, card, false));
        assert_eq!(InputUpdateCardRuntime::default().status(), InputUpdateCardStatus::Ready);
    }
    assert_eq!(cards[0].last_update(860).relative, LastUpdateRelative::Minutes(1));
    assert_eq!(cards[8].last_update(860).title_timestamp, Some(600));
    for card in &cards[11..] {
        assert!(!can_start_update(&InputUpdateCardInteractionState::from_model(card), card, true));
        assert_eq!(card.last_update(860).relative, LastUpdateRelative::Never);
    }
    let scope = playlist_update_bulk_scope(&sources, &cards);
    assert!(scope.can_submit);
    assert_eq!(scope.visible_input_ids, [41, 7, 19, 3, 11, 2, 33, 15, 4, 28, 9]);
}

#[test]
fn playlist_update_view_target_rules_do_not_become_input_or_action_capabilities() {
    let mut sources = sources();
    let before = sources.clone();
    let cards = build_input_update_card_models(&sources, &Default::default());
    let m3u = &cards[0];
    let state = reduce_input_update_card_state(
        InputUpdateCardInteractionState::from_model(m3u),
        InputUpdateCardAction::ToggleTarget(3),
    );
    assert_eq!(selected_current_target_ids(m3u, &state.controls.selected_target_ids), [20]);
    assert_eq!(sources, before);
    let first_target = sources.sources[0].targets[0].clone();
    let second_target = &mut sources.sources[0].targets[1];
    assert_ne!(first_target.filter, second_target.filter);
    assert!(first_target.rename.is_some() && second_target.rename.is_none());
    assert_ne!(first_target.mapping, second_target.mapping);
    second_target.filter = "Name ~ \"changed\"".into();
    second_target.rename = first_target.rename.clone();
    second_target.mapping = None;
    assert_eq!(sources.inputs, before.inputs);
    assert_eq!(sources.sources[0].targets[0], first_target);
    // Optional pipeline indicators are not introduced: changes to target rules do not
    // change card identity, labels, action eligibility, target IDs or last-update data.
    assert_eq!(build_input_update_card_models(&sources, &Default::default()), cards);
}

#[test]
fn playlist_update_view_same_names_keep_real_card_state_and_actions_independent_by_id() {
    let mut sources = sources();
    sources.inputs[0].name = "same-input".intern();
    sources.inputs[1].name = "same-input".intern();
    sources.sources[0].inputs[0] = "same-input".intern();
    sources.sources[0].inputs[1] = "same-input".intern();
    sources.sources.truncate(1);
    let cards = build_input_update_card_models(&sources, &Default::default());
    assert_eq!(cards[0].input_name, cards[1].input_name);
    let mut states = cards
        .iter()
        .map(|card| (card.input_id, InputUpdateCardInteractionState::from_model(card)))
        .collect::<HashMap<_, _>>();
    let untouched = states.get(&7).expect("Xtream state").clone();
    let m3u = states.remove(&41).expect("M3U state");
    let m3u = reduce_input_update_card_state(m3u, InputUpdateCardAction::ToggleTarget(3));
    let m3u = reduce_input_update_card_state(m3u, InputUpdateCardAction::SelectPolicy(InputRefreshPolicy::REFRESH));
    assert_eq!(m3u.controls.selected_target_ids, [20]);
    assert_eq!(states.get(&7), Some(&untouched));
    assert_eq!(untouched.controls.selected_target_ids, [20, 3]);
    assert_eq!(untouched.controls.policy, InputRefreshPolicy::NORMAL);
    assert!(cards[0].capabilities.supports(InputRefreshPolicy::FORCE));
    assert!(cards[1].capabilities.supports(InputRefreshPolicy::FORCE));
    let before_scope = playlist_update_bulk_scope(&sources, &cards);
    states.insert(41, m3u);
    assert_eq!(playlist_update_bulk_scope(&sources, &cards), before_scope);
}

#[test]
fn playlist_update_view_supported_actions_keep_confirmation_conflict_and_acceptance_contracts() {
    let cards = build_input_update_card_models(&sources(), &Default::default());
    let connection_context = WebSocketConnectionContext::new(1, true);
    for card in &cards {
        for &policy in card.capabilities.policies() {
            if card.targets.is_empty() {
                continue;
            }
            let state = reduce_input_update_card_state(
                InputUpdateCardInteractionState::from_model(card),
                InputUpdateCardAction::SelectPolicy(policy),
            );
            assert!(can_start_update(&state, card, true));
            assert_eq!(requires_confirmation(policy), policy == InputRefreshPolicy::FORCE);
            if policy == InputRefreshPolicy::FORCE {
                assert_eq!(
                    confirmation_submission_decision(
                        connection_context,
                        connection_context,
                        policy,
                        Some(DialogResult::Cancel)
                    ),
                    ConfirmationSubmissionDecision::Cancel
                );
            }
            let submitting = reduce_input_update_card_state(
                state.clone(),
                InputUpdateCardAction::BeginSubmission(connection_context),
            );
            let rejected = reduce_input_update_card_state(
                submitting.clone(),
                InputUpdateCardAction::FinishSubmission {
                    connection_context,
                    outcome: classify_update_outcome(Err(Error::Conflict("busy".to_owned()))),
                },
            );
            assert_eq!(rejected, state);
            let accepted = reduce_input_update_card_state(
                submitting,
                InputUpdateCardAction::FinishSubmission {
                    connection_context,
                    outcome: classify_update_outcome(Ok(OperationRunAccepted::playlist_update("accepted".into()))),
                },
            );
            assert_eq!(accepted.controls.policy, InputRefreshPolicy::NORMAL);
            assert_eq!(accepted.controls.selected_target_ids, state.controls.selected_target_ids);
            assert!(!accepted.is_submitting());
        }
    }
}

#[test]
fn playlist_update_view_rescan_and_unavailable_actions_keep_explicit_accessible_context(
) -> Result<(), serde_json::Error> {
    let cards = build_input_update_card_models(&sources(), &Default::default());
    let source = include_str!("input_update_card.rs").split("#[cfg(test)]").next().expect("card component");
    let common_button = source
        .split_once("name={format!(\"input_update_{}\", model.input_id)}")
        .expect("common action button")
        .1
        .split_once("/>")
        .expect("button end")
        .0;
    assert!(common_button.contains("aria_label={Some(start_aria_label)}"));
    assert!(common_button.contains("can_start_update"));
    assert!(common_button.contains("onclick={on_start}"));
    assert!(!source.contains("library_action_callback"));
    assert!(source.contains("\"LABEL.START_RESCAN\""));
    for (fixture, card) in INPUTS.iter().zip(&cards) {
        let options = policy_options(card.capabilities, InputRefreshPolicy::NORMAL, str::to_owned);
        assert_eq!(options.iter().map(|option| option.id.as_str()).collect::<Vec<_>>(), fixture.2);
    }
    for locale in [
        include_str!("../../../../public/assets/i18n/en.json"),
        include_str!("../../../../public/assets/i18n/es.json"),
        include_str!("../../../../public/assets/i18n/ru.json"),
        include_str!("../../../../public/assets/i18n/ar.json"),
    ] {
        let locale: serde_json::Value = serde_json::from_str(locale)?;
        for card in &cards {
            let pointer =
                format!("/{}", card.capabilities.description_key(InputRefreshPolicy::NORMAL).replace('.', "/"));
            assert!(locale
                .pointer(&pointer)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.trim().is_empty()));
        }
        let template = locale["MESSAGES"]["PLAYLIST_UPDATE"]["START_LABEL"].as_str().expect("action template");
        let label = locale["LABEL"]["START_RESCAN"].as_str().expect("library label");
        let accessible = action_message(template.to_owned(), &cards[7].input_name, label);
        assert!(accessible.contains(cards[7].input_name.as_ref()) && accessible.contains(label));
    }
    Ok(())
}

// Optional typed fixture export for the browser audit; inactive during ordinary tests.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn update_view_export_browser_fixture() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(directory) = std::env::var("TULIPROX_UPDATE_VIEW_FIXTURE_DIR") else {
        return Ok(());
    };
    use shared::model::{AppConfigDto, LibraryStatus, UserSettingsDto};
    let mut config = AppConfigDto::default();
    config.config.library = Some(serde_json::from_value(serde_json::json!({"enabled": true}))?);
    let input_data = [
        (1, "dev0", InputType::Xtream, 10, "dev00"),
        (2, "dev1", InputType::M3u, 20, "dev10"),
        (3, "dev2", InputType::Library, 30, "dev20"),
        (4, "trex", InputType::Xtream, 40, "dev01"),
    ];
    for (id, name, input_type, target_id, target_name) in input_data {
        config.sources.inputs.push(ConfigInputDto {
            id,
            name: name.intern(),
            input_type,
            enabled: true,
            ..Default::default()
        });
        config.sources.sources.push(ConfigSourceDto {
            inputs: vec![name.intern()],
            targets: vec![ConfigTargetDto { id: target_id, name: target_name.into(), ..Default::default() }],
        });
    }
    let now = 1_800_000_000_u64;
    let inputs = input_data.iter().map(|(id, _, input_type, _, _)| {
        let last_update = now - if *id == 1 { 59 * 60 } else { 3 * 3600 };
        let clusters = if *input_type == InputType::Library { vec![] } else {
            [XtreamCluster::Live, XtreamCluster::Series, XtreamCluster::Video].iter().map(|cluster| {
                let rejected = *id <= 2 && *cluster == XtreamCluster::Live;
                serde_json::json!({"cluster":cluster,"status":if rejected { "failed" } else { "ok" },"timestamp":last_update,
                    "last_update":{"source":"provider","technical_state":"succeeded","policy":{"cache":"respect","quality":"enforce"},
                    "quality":{"threshold":95,"baseline_count":100,"candidate_count":if rejected {80} else {100},"achieved_quality":if rejected {80} else {100},"decision":if rejected {"rejected"} else {"accepted"}}}})
            }).collect()
        };
        serde_json::json!({"input_id":id,"last_update":last_update,"last_input_update":{"state":if *id <= 2 {"Partial"} else {"Success"},"timestamp":last_update},"clusters":clusters})
    }).collect::<Vec<_>>();
    let status: PlaylistUpdateStatusDto = serde_json::from_value(serde_json::json!({"inputs":inputs}))?;
    let value = serde_json::json!({"config":config,"status":status,"now":now,
        "library": LibraryStatus { enabled:true, ..Default::default() },"settings":UserSettingsDto::default()});
    std::fs::create_dir_all(&directory)?;
    std::fs::write(std::path::Path::new(&directory).join("fixture.json"), serde_json::to_vec_pretty(&value)?)?;
    Ok(())
}
