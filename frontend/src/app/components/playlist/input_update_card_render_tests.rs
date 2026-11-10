use super::*;
use crate::model::{build_input_update_card_models, build_input_update_run_views, PlaylistUpdateCardStatuses};
use shared::model::{
    ConfigInputDto, InputType, LibraryScanResult, LibraryStatus, PersistedPlaylistUpdateClusterSnapshot,
    PlaylistUpdateStatusDto, SourcesConfigDto,
};
use yew::virtual_dom::{VNode, VTag};

fn tags(node: &VNode) -> Vec<&VTag> {
    match node {
        VNode::VTag(tag) => std::iter::once(tag.as_ref()).chain(tag.children().into_iter().flat_map(tags)).collect(),
        VNode::VList(list) => list.iter().flat_map(tags).collect(),
        _ => Vec::new(),
    }
}

fn has_attribute(tag: &VTag, key: &str, value: &str) -> bool {
    tag.attributes.iter().any(|(name, contents)| name == key && contents == value)
}

fn visible_text(node: &VNode, expanded: bool) -> String {
    match node {
        VNode::VText(text) => text.text.to_string(),
        VNode::VList(list) => list.iter().map(|node| visible_text(node, expanded)).collect::<Vec<_>>().join(" "),
        VNode::VTag(tag) if tag.tag() == "details" && !expanded => {
            assert!(!tag.attributes.iter().any(|(key, _)| key == "open"), "card starts collapsed");
            let summary = tags(tag.children().unwrap()).into_iter().find(|tag| tag.tag() == "summary").unwrap();
            visible_text(summary.children().unwrap(), expanded)
        }
        VNode::VTag(tag) => tag.children().map_or_else(String::new, |node| visible_text(node, expanded)),
        _ => String::new(),
    }
}

fn card_view(input_type: InputType) -> (InputUpdateCardModel, InputUpdateRunView) {
    let sources = SourcesConfigDto {
        inputs: vec![ConfigInputDto { id: 17, enabled: true, input_type, ..ConfigInputDto::default() }],
        ..SourcesConfigDto::default()
    };
    let persisted = PlaylistUpdateStatusDto::default();
    let cards = build_input_update_card_models(&sources, &persisted);
    let view =
        build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted).remove(0);
    (cards.into_iter().next().unwrap(), view)
}

fn render_content(card: &InputUpdateCardModel, view: &InputUpdateRunView) -> Html {
    // This is the complete content renderer called by InputUpdateCard, not library_details.
    // Unchanged interaction-owned nodes occupy their real Last-update/Target positions.
    input_content(
        view,
        card.capabilities,
        html! { <div class="tp__playlist-update-view__input-card-footer">{"Last update"}</div> },
        html! { <section class="tp__playlist-update-view__input-targets">{"Affected targets"}</section> },
        Html::default(),
        str::to_string,
    )
}

fn has_status_dot(badge: &yew::virtual_dom::VTag) -> bool {
    tags(badge.children().unwrap())
        .into_iter()
        .any(|tag| has_attribute(tag, "class", "tp__playlist-update-view__status-dot"))
}

fn assert_rendered_cluster_badges(node: &VNode, modifier: &str, label: &str) {
    let badges: Vec<_> = tags(node)
        .into_iter()
        .filter(|tag| {
            tag.attributes.iter().any(|(key, value)| {
                key == "class"
                    && value.split_whitespace().any(|class| class == "tp__playlist-update-view__pipeline-status")
            })
        })
        .collect();
    assert_eq!(badges.len(), 6, "three compact chips and three expanded detail badges");
    assert_eq!(badges.iter().filter(|badge| has_attribute(badge, "role", "listitem")).count(), 3);
    for badge in badges {
        assert!(badge
            .attributes
            .iter()
            .any(|(key, value)| key == "class" && value.split_whitespace().any(|class| class == modifier)));
        assert!(has_status_dot(badge), "every status badge leads with a status dot");
        if has_attribute(badge, "role", "listitem") {
            assert!(has_attribute(badge, "title", label));
        } else {
            assert!(visible_text(badge.children().unwrap(), true).contains(label));
        }
    }
}

#[test]
fn input_update_card_persisted_failed_badges_match_live_errors_after_reload() {
    for input_type in [InputType::Xtream, InputType::Stalker, InputType::M3u] {
        for snapshot in [
            None,
            Some(PersistedPlaylistUpdateClusterSnapshot {
                policy: Some(InputRefreshPolicy::FORCE),
                source: Some(InputDataSourceView::Provider),
                quality_guard_threshold: Some(95),
                ..PersistedPlaylistUpdateClusterSnapshot::default()
            }),
            Some(PersistedPlaylistUpdateClusterSnapshot {
                technical_state: Some(PersistedPlaylistUpdateTechnicalState::Failed),
                ..PersistedPlaylistUpdateClusterSnapshot::default()
            }),
        ] {
            let sources = SourcesConfigDto {
                inputs: vec![ConfigInputDto { id: 17, enabled: true, input_type, ..ConfigInputDto::default() }],
                ..SourcesConfigDto::default()
            };
            let clusters = [XtreamCluster::Live, XtreamCluster::Series, XtreamCluster::Video].map(|cluster| {
                serde_json::json!({"cluster": cluster, "status": "failed", "timestamp": 123, "last_update": snapshot})
            });
            let persisted: PlaylistUpdateStatusDto = serde_json::from_value(serde_json::json!({
                "inputs": [{"input_id": 17, "clusters": clusters}]
            }))
            .unwrap();
            let cards = build_input_update_card_models(&sources, &persisted);
            let view =
                build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted)
                    .remove(0);
            assert!(view.run_id.is_none());
            assert!(
                view.cluster_results.iter().all(|cluster| {
                    cluster.technical_state == snapshot.and_then(|saved| saved.technical_state)
                        && cluster.outcome.is_none()
                        && cluster.quality.is_none()
                        && cluster.active_count.is_none()
                }),
                "display must not invent a technical cause, Quality decision or active population"
            );
            assert_rendered_cluster_badges(
                &render_content(&cards[0], &view),
                "tp__playlist-update-view__pipeline-status--error",
                "LABEL.UPDATE_STATUS_FAILED",
            );
        }
    }
}

#[test]
fn input_update_card_persisted_failed_quality_rejection_keeps_warning_badges() {
    let sources = SourcesConfigDto {
        inputs: vec![ConfigInputDto {
            id: 17,
            enabled: true,
            input_type: InputType::Xtream,
            ..ConfigInputDto::default()
        }],
        ..SourcesConfigDto::default()
    };
    let clusters = [XtreamCluster::Live, XtreamCluster::Series, XtreamCluster::Video].map(|cluster| {
        serde_json::json!({"cluster": cluster, "status": "failed", "timestamp": 123, "last_update": {
            "source": "provider", "technical_state": "succeeded",
            "quality": {"threshold": 95, "baseline_count": 100, "candidate_count": 1, "achieved_quality": 1, "decision": "rejected"}
        }})
    });
    let persisted: PlaylistUpdateStatusDto = serde_json::from_value(serde_json::json!({
        "inputs": [{"input_id": 17, "clusters": clusters}]
    }))
    .unwrap();
    let cards = build_input_update_card_models(&sources, &persisted);
    let view =
        build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted).remove(0);
    assert!(view.cluster_results.iter().all(|cluster| cluster.outcome == Some(InputClusterOutcomeView::Rejected)));
    assert_rendered_cluster_badges(
        &render_content(&cards[0], &view),
        "tp__playlist-update-view__pipeline-status--rejected",
        "MESSAGES.PLAYLIST_UPDATE.CLUSTER_REJECTED",
    );
}

#[test]
fn pipeline_transparency_input_update_card_series_failure_survives_reload_and_followup_priority() {
    use crate::model::{PlaylistUpdateAcceptedScope, PlaylistUpdateCardStatusAction};
    use shared::model::{
        PlaylistUpdateInputTelemetry, PlaylistUpdateProgressEvent, PlaylistUpdateRunStateEvent, PlaylistUpdateState,
    };
    use std::rc::Rc;

    let sources = SourcesConfigDto {
        inputs: vec![ConfigInputDto {
            id: 17,
            enabled: true,
            input_type: InputType::Xtream,
            ..ConfigInputDto::default()
        }],
        ..SourcesConfigDto::default()
    };
    let persisted: PlaylistUpdateStatusDto = serde_json::from_value(serde_json::json!({
        "inputs": [{"input_id": 17, "clusters": [{"cluster": XtreamCluster::Series, "status": "failed", "timestamp": 123,
            "last_update": {"source": "provider", "technical_state": "failed"}}]}]
    }))
    .unwrap();
    let cards = build_input_update_card_models(&sources, &persisted);
    let connection = WebSocketConnectionContext::new(1, true);
    let project = |state: &PlaylistUpdateCardStatuses| {
        build_input_update_run_views(&sources, &cards, state, &persisted).remove(0)
    };
    let assert_failed = |view: &InputUpdateRunView| {
        let node = render_content(&cards[0], view);
        let badge = tags(&node)
            .into_iter()
            .find(|tag| {
                has_attribute(tag, "role", "listitem")
                    && has_attribute(
                        tag,
                        "aria-label",
                        "MESSAGES.PLAYLIST_UPDATE.CONTENT_SERIES: LABEL.UPDATE_STATUS_FAILED",
                    )
            })
            .unwrap();
        assert!(badge.attributes.iter().any(|(key, value)| key == "class" && value.contains("pipeline-status--error")));
        assert!(has_status_dot(badge));
        let details = tags(&node)
            .into_iter()
            .find(|tag| has_attribute(tag, "aria-labelledby", "input-update-card-17-series-details"))
            .unwrap();
        assert!(visible_text(details.children().unwrap(), true).contains("LABEL.UPDATE_STATUS_FAILED"));
    };
    assert_failed(&project(&PlaylistUpdateCardStatuses::default()));
    for decision in [serde_json::Value::Null, serde_json::json!("accepted")] {
        let telemetry: PlaylistUpdateInputTelemetry = serde_json::from_value(serde_json::json!({
            "refresh_policy": InputRefreshPolicy::FORCE,
            "clusters": [
                {"cluster": XtreamCluster::Live, "requested": true, "source": "provider", "decision": "accepted", "technical_state": "succeeded"},
                {"cluster": XtreamCluster::Series, "requested": true, "source": "provider", "decision": decision, "technical_state": "failed"},
                {"cluster": XtreamCluster::Video, "requested": true, "source": "provider", "decision": "accepted", "technical_state": "succeeded"}
            ]
        })).unwrap();
        let running = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection)).reduce(
            PlaylistUpdateCardStatusAction::Progress {
                connection_context: connection,
                progress: PlaylistUpdateProgressEvent::input_completed(
                    "R1".into(),
                    1.into(),
                    17,
                    PlaylistUpdateState::Failure,
                    "provider",
                    "completed",
                )
                .with_input_telemetry(telemetry),
            },
        );
        let queued = running.reduce(PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
            connection_context: connection,
            run_id: "R2".into(),
            input_ids: vec![17],
            refresh_policy: InputRefreshPolicy::REFRESH,
        }));
        let current = project(&queued);
        assert_eq!(current.run_id.as_ref().map(AsRef::as_ref), Some("R1"));
        assert_failed(&current);
        for sibling in current.cluster_results.iter().filter(|cluster| cluster.cluster != XtreamCluster::Series) {
            assert_eq!(cluster_summary_status(sibling), ClusterSummaryStatus::Accepted);
        }
        let next = queued.reduce(PlaylistUpdateCardStatusAction::Completed {
            connection_context: connection,
            completed: PlaylistUpdateRunStateEvent::correlated("R1".into(), 1.into(), PlaylistUpdateState::Failure),
        });
        let next = project(&next);
        assert_eq!(next.run_id.as_ref().map(AsRef::as_ref), Some("R2"));
        assert_eq!(next.overall_state, InputUpdateCardStatus::Queued);
        assert!(next
            .cluster_results
            .iter()
            .all(|cluster| cluster.technical_state.is_none() && cluster.persisted_status.is_none()));
    }
}

#[test]
fn update_overview_badges_cluster_labels() {
    for (locale, expected_labels) in [
        (include_str!("../../../../public/assets/i18n/en.json"), ["Live", "Series", "Movies"]),
        (include_str!("../../../../public/assets/i18n/ar.json"), ["مباشر", "مسلسلات", "أفلام"]),
        (include_str!("../../../../public/assets/i18n/ru.json"), ["Прямой эфир", "Сериалы", "Фильмы"]),
    ] {
        let translations: serde_json::Value = serde_json::from_str(locale).unwrap();
        assert!(translations.pointer("/MESSAGES/PLAYLIST_UPDATE/CONTENT_SHOWS").is_none());
        for (cluster, expected) in
            [XtreamCluster::Live, XtreamCluster::Series, XtreamCluster::Video].into_iter().zip(expected_labels)
        {
            let key = cluster_label_key(cluster).replace('.', "/");
            assert_eq!(translations.pointer(&format!("/{key}")).and_then(serde_json::Value::as_str), Some(expected));
        }
    }
}

fn content_badge_labels(card: &InputUpdateCardModel, view: &InputUpdateRunView) -> Vec<String> {
    let translations: serde_json::Value =
        serde_json::from_str(include_str!("../../../../public/assets/i18n/en.json")).unwrap();
    let rendered = input_content(view, card.capabilities, Html::default(), Html::default(), Html::default(), |key| {
        translations
            .pointer(&format!("/{}", key.replace('.', "/")))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(key)
            .to_owned()
    });
    tags(&rendered)
        .into_iter()
        .filter(|tag| has_attribute(tag, "role", "listitem"))
        .map(|tag| {
            // Decorative children (cluster dot, status symbol) are aria-hidden; the label is the visible text span.
            let label = tags(tag.children().unwrap())
                .into_iter()
                .find(|tag| tag.tag() == "span" && !has_attribute(tag, "aria-hidden", "true"))
                .unwrap();
            visible_text(label.children().unwrap(), false)
        })
        .collect()
}

#[test]
fn update_overview_badges_content_order_is_live_series_movies() {
    for input_type in [InputType::Xtream, InputType::Stalker, InputType::M3u, InputType::Staged] {
        let (card, mut view) = card_view(input_type);
        view.cluster_results = card_view(InputType::Xtream).1.cluster_results;
        assert_eq!(content_badge_labels(&card, &view), ["Live", "Series", "Movies"], "{input_type}");
        view.cluster_results.reverse();
        assert_eq!(
            content_badge_labels(&card, &view),
            ["Live", "Series", "Movies"],
            "incoming telemetry order is not presentation order"
        );
        view.cluster_results.retain(|cluster| cluster.cluster != XtreamCluster::Live);
        assert_eq!(content_badge_labels(&card, &view), ["Series", "Movies"], "do not invent missing clusters");
    }
}

#[test]
fn update_overview_badges_library_content_order_is_series_movies() {
    let (card, view) = card_view(InputType::Library);
    let view = view.with_library_catalog(Some(&LibraryStatus {
        enabled: true,
        series: 2,
        movies: 3,
        episodes: 8,
        total_items: 5,
        path: None,
    }));
    assert_eq!(content_badge_labels(&card, &view), ["Series", "Movies"]);
}

#[test]
fn update_overview_badges_no_mixed_cluster_vocabulary() {
    let translations: serde_json::Value =
        serde_json::from_str(include_str!("../../../../public/assets/i18n/en.json")).unwrap();
    for input_type in [InputType::Xtream, InputType::Stalker, InputType::M3u, InputType::Library] {
        let (card, mut view) = card_view(input_type);
        if input_type == InputType::M3u {
            view.cluster_results = card_view(InputType::Xtream).1.cluster_results;
        }
        if input_type == InputType::Library {
            view = view.with_library_catalog(Some(&LibraryStatus {
                enabled: true,
                movies: 2,
                series: 1,
                episodes: 3,
                total_items: 3,
                path: None,
            }));
        }
        let node = input_content(&view, card.capabilities, Html::default(), Html::default(), Html::default(), |key| {
            translations
                .pointer(&format!("/{}", key.replace('.', "/")))
                .and_then(serde_json::Value::as_str)
                .unwrap_or(key)
                .to_owned()
        });
        let closed = visible_text(&node, false);
        assert!(closed.contains("Series") && closed.contains("Movies"), "{input_type}: {closed}");
        assert_eq!(closed.contains("Live"), input_type != InputType::Library);
        let expanded = visible_text(&node, true);
        assert!(!expanded.contains("Shows") && !expanded.contains("VOD") && !closed.contains("Video"), "{expanded}");
    }
}

#[test]
fn update_overview_badges_no_generic_accepted_for_cluster_content() {
    let (card, mut view) = card_view(InputType::M3u);
    view.cluster_results = card_view(InputType::Xtream).1.cluster_results;
    view.input_state = InputUpdateCardStatus::Success;
    for cluster in &mut view.cluster_results {
        cluster.outcome = Some(InputClusterOutcomeView::Accepted);
        cluster.requested = Some(true);
    }
    let node = render_content(&card, &view);
    let summary = tags(&node)
        .into_iter()
        .find(|tag| has_attribute(tag, "class", "tp__playlist-update-view__pipeline-summary"))
        .unwrap();
    let text = visible_text(summary.children().unwrap(), true);
    assert!(!text.contains("CLUSTER_ACCEPTED") && !text.contains(view.input_state.label_key()));
    for cluster in &view.cluster_results {
        assert!(text.contains(cluster_label_key(cluster.cluster)));
    }
    assert_eq!(
        tags(summary.children().unwrap()).iter().filter(|tag| has_attribute(tag, "role", "listitem")).count(),
        3
    );
}

#[test]
fn update_overview_badges_catalog_content_never_invents_live_or_cluster_success() {
    for input_type in [InputType::Library, InputType::Plex, InputType::Emby, InputType::Jellyfin] {
        let (card, view) = card_view(input_type);
        let view = view.with_library_catalog(Some(&LibraryStatus {
            enabled: true,
            series: 1,
            episodes: 12,
            total_items: 1,
            ..LibraryStatus::default()
        }));
        assert_eq!(
            view.catalog_content_clusters(),
            if input_type == InputType::Library { vec![XtreamCluster::Series] } else { vec![] }
        );
        let node = render_content(&card, &view);
        let closed = visible_text(&node, false);
        assert!(
            !closed.contains("CONTENT_LIVE")
                && !closed.contains("CONTENT_MOVIES")
                && !closed.contains("CLUSTER_ACCEPTED")
        );
        assert_eq!(closed.contains("CONTENT_SERIES"), input_type == InputType::Library);
    }
}

#[test]
fn pipeline_transparency_input_update_card_library_content_is_compact_and_details_are_library_only() {
    let (card, mut view) = card_view(InputType::Library);
    view.input_state = InputUpdateCardStatus::Failed;
    view.cache_source = Some(InputDataSourceView::Provider); // Actual internal acquisition remains intact.
    view.library_catalog =
        Some(LibraryStatus { enabled: true, movies: 2, series: 3, episodes: 19, total_items: 5, path: None });
    view.library_scan_result = Some(LibraryScanResult {
        files_scanned: 11,
        groups_scanned: 7,
        files_added: 3,
        files_updated: 2,
        files_removed: 1,
        errors: 4,
    });
    let rendered = render_content(&card, &view);
    let all_tags = tags(&rendered);
    let summary =
        all_tags.iter().find(|tag| has_attribute(tag, "class", "tp__playlist-update-view__pipeline-summary")).unwrap();
    let target_position =
        all_tags.iter().position(|tag| has_attribute(tag, "class", "tp__playlist-update-view__input-targets")).unwrap();
    let details_position = all_tags.iter().position(|tag| tag.tag() == "details").unwrap();
    assert!(target_position < details_position);
    let closed = visible_text(&rendered, false);
    assert!(
        closed.contains("CONTENT_SERIES") && closed.contains("CONTENT_MOVIES"),
        "catalog content remains compact: {closed}"
    );
    assert!(!closed.contains(view.input_state.label_key()), "aggregate input outcome is not a content type");
    assert!(closed.contains("Affected targets") && closed.contains("UPDATE_DETAILS") && closed.contains("Last update"));
    assert!(!closed.contains("LIBRARY_"), "Library counters must not leak out of the closed disclosure: {closed}");
    let chips = tags(summary.children().unwrap());
    assert_eq!(chips.iter().filter(|tag| has_attribute(tag, "role", "list")).count(), 1);
    assert_eq!(chips.iter().filter(|tag| has_attribute(tag, "role", "listitem")).count(), 2);
    assert!(!chips.iter().any(|tag| matches!(tag.tag(), "section" | "dl")), "no detail sections inside the chip list");
    let details = visible_text(all_tags[details_position].children().unwrap(), true);
    for label in [
        "CATALOG",
        "LAST_RESCAN",
        "EPISODES",
        "TOTAL_ITEMS",
        "FILES_SCANNED",
        "GROUPS_SCANNED",
        "ADDED",
        "UPDATED",
        "REMOVED",
        "ERRORS",
    ] {
        assert!(details.contains(&format!("LIBRARY_{label}")), "{label} missing from real disclosure: {details}");
    }
    assert!(details.contains("CONTENT_MOVIES") && details.contains("CONTENT_SERIES"));
    for forbidden in ["LABEL.SOURCE", "LABEL.CACHE", "LABEL.PROVIDER", "CACHE_NOT_USED", "QUALITY_"] {
        assert!(!details.contains(forbidden), "Library rendered generic details: {details}");
    }
    assert!(details.contains("19") && details.contains("11") && details.contains('4'));
    view.library_scan_result = None;
    let reload = visible_text(&render_content(&card, &view), true);
    assert_eq!(reload.matches('—').count(), 6, "catalog survives reload; scan metrics remain unknown");
    view.library_catalog = None;
    assert_eq!(visible_text(&render_content(&card, &view), true).matches('—').count(), 11);
}

#[test]
fn pipeline_transparency_input_update_card_provider_content_keeps_source_cache_and_cluster_quality_details() {
    for input_type in [InputType::Xtream, InputType::Stalker, InputType::M3u, InputType::Plex] {
        for source in [InputDataSourceView::Provider, InputDataSourceView::Cache] {
            let (card, mut view) = card_view(input_type);
            view.cache_source = Some(source);
            view.refresh_policy = Some(InputRefreshPolicy::NORMAL);
            for cluster in &mut view.cluster_results {
                cluster.requested = Some(true);
                cluster.source = Some(source);
                cluster.threshold = Some(95);
                cluster.quality = Some(99);
                cluster.outcome = Some(InputClusterOutcomeView::Accepted);
            }
            let rendered = render_content(&card, &view);
            let closed = visible_text(&rendered, false);
            assert!(!closed.contains("LABEL.SOURCE") && !closed.contains("QUALITY_THRESHOLD"));
            let expanded = visible_text(&rendered, true);
            assert!(!expanded.contains("LIBRARY_"));
            assert!(expanded.contains("LABEL.SOURCE") && expanded.contains("LABEL.CACHE"), "{input_type}: {expanded}");
            assert!(expanded.contains(if source == InputDataSourceView::Provider {
                "CACHE_NOT_USED"
            } else {
                "CACHE_USED"
            }));
            assert_eq!(expanded.contains("QUALITY_THRESHOLD"), !view.cluster_results.is_empty());
            assert_eq!(expanded.contains("QUALITY_ACHIEVED"), !view.cluster_results.is_empty());
        }
    }
}

#[test]
fn input_update_card_general_status_keeps_colors_in_details_without_becoming_a_content_badge() {
    for input_type in [InputType::M3u, InputType::Library, InputType::Plex] {
        for (state, modifier) in [
            (InputUpdateCardStatus::Ready, "neutral"),
            (InputUpdateCardStatus::Queued, "queued"),
            (InputUpdateCardStatus::Updating, "updating"),
            (InputUpdateCardStatus::Success, "accepted"),
            (InputUpdateCardStatus::Partial, "rejected"),
            (InputUpdateCardStatus::Failed, "error"),
        ] {
            let (card, mut view) = card_view(input_type);
            view.input_state = state;
            let rendered = render_content(&card, &view);
            let expected_class = format!(
                "tp__playlist-update-view__pipeline-status tp__playlist-update-view__pipeline-status--{modifier}"
            );
            let all_tags = tags(&rendered);
            let badges = all_tags.iter().filter(|tag| has_attribute(tag, "class", &expected_class)).collect::<Vec<_>>();
            assert_eq!(badges.len(), usize::from(input_type != InputType::Library), "{input_type}: {state:?}");
            assert!(!badges.iter().any(|badge| has_attribute(badge, "role", "listitem")));
            for badge in badges {
                let text = visible_text(badge.children().unwrap(), true);
                assert!(text.contains(state.label_key()) && has_status_dot(badge), "{text}");
            }
            assert!(!all_tags.iter().any(|tag| tag
                .attributes
                .iter()
                .any(|(key, value)| { key == "class" && value.contains("tp__task-status") })));
        }
    }
}

#[test]
fn input_update_card_header_colors_reload_result_without_claiming_target_success() {
    let (_, mut view) = card_view(InputType::M3u);
    view.input_state = InputUpdateCardStatus::Success;
    let rendered = card_status_badge(&view, str::to_string);
    let text = visible_text(&rendered, true);
    assert!(text.contains("UPDATE_STATUS_SUCCESS") && !text.contains("LABEL.INPUT"));
    let all_tags = tags(&rendered);
    let badge = all_tags[0];
    assert!(has_attribute(badge, "role", "status"));
    assert!(has_attribute(badge, "aria-live", "polite"));
    assert!(has_attribute(badge, "aria-atomic", "true"));
    assert!(has_attribute(
        badge,
        "class",
        "tp__playlist-update-view__pipeline-status tp__playlist-update-view__pipeline-status--accepted"
    ));
    view.run_id = Some("current".into());
    view.overall_state = InputUpdateCardStatus::Updating;
    let active = visible_text(&card_status_badge(&view, str::to_string), true);
    assert!(active.contains("UPDATE_STATUS_UPDATING"));
    assert!(!active.contains("UPDATE_STATUS_SUCCESS") && !active.contains("LABEL.INPUT:"));
    // A finished run shows this input's own result, not a target failure caused by another input.
    view.overall_state = InputUpdateCardStatus::Failed;
    assert!(visible_text(&card_status_badge(&view, str::to_string), true).contains("UPDATE_STATUS_SUCCESS"));
    view.input_state = InputUpdateCardStatus::Failed;
    assert!(visible_text(&card_status_badge(&view, str::to_string), true).contains("UPDATE_STATUS_FAILED"));
}

#[test]
fn input_update_card_header_badges_share_geometry_and_have_no_input_prefix() {
    for input_type in [InputType::Xtream, InputType::M3u, InputType::Library, InputType::Plex] {
        for status in [
            InputUpdateCardStatus::Ready,
            InputUpdateCardStatus::Queued,
            InputUpdateCardStatus::Updating,
            InputUpdateCardStatus::Success,
            InputUpdateCardStatus::Partial,
            InputUpdateCardStatus::Failed,
        ] {
            let (_, mut view) = card_view(input_type);
            view.run_id = Some("active".into());
            view.overall_state = status;
            let rendered = card_status_badge(&view, str::to_string);
            let badge = tags(&rendered)[0];
            assert!(has_attribute(
                badge,
                "class",
                &format!("tp__playlist-update-view__pipeline-status {}", status.modifier())
            ));
            assert!(!badge.attributes.iter().any(|(name, _)| name == "style"));
            assert_eq!(visible_text(&rendered, true).trim(), status.label_key());
            assert!(has_status_dot(badge));
        }
    }
}

#[test]
fn cluster_status_without_quality_uses_general_provider_result_and_keeps_failure_priority() {
    for input_type in [InputType::Xtream, InputType::Stalker, InputType::M3u] {
        for (general, label) in [("ok", "LABEL.UPDATE_STATUS_SUCCESS"), ("failed", "LABEL.UPDATE_STATUS_FAILED")] {
            for snapshot in
                [serde_json::Value::Null, serde_json::json!({"source":"provider","quality_guard_threshold":0})]
            {
                let sources = SourcesConfigDto {
                    inputs: vec![ConfigInputDto { id: 17, enabled: true, input_type, ..Default::default() }],
                    ..Default::default()
                };
                let persisted:PlaylistUpdateStatusDto=serde_json::from_value(serde_json::json!({"inputs":[{"input_id":17,"clusters":[{"cluster":"Live","status":general,"timestamp":7,"last_update":snapshot}]}]})).unwrap();
                let cards = build_input_update_card_models(&sources, &persisted);
                let view =
                    build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted)
                        .remove(0);
                let cluster = view.cluster_results.iter().find(|c| c.cluster == XtreamCluster::Live).unwrap();
                assert_eq!(cluster.outcome, None, "general success does not invent Quality acceptance");
                assert_eq!(cluster_summary_status(cluster).label_key(), label);
                let rendered = render_content(&cards[0], &view);
                let chip = tags(&rendered)
                    .into_iter()
                    .find(|tag| {
                        has_attribute(tag, "role", "listitem")
                            && tag.attributes.iter().any(|(key, value)| key == "aria-label" && value.contains(label))
                    })
                    .unwrap();
                assert!(has_status_dot(chip));
                assert!(!visible_text(&rendered, true).contains("QUALITY_ACHIEVED"));
            }
        }
    }
    let (card, mut view) = card_view(InputType::Xtream);
    let cluster = &mut view.cluster_results[0];
    cluster.technical_state = Some(PersistedPlaylistUpdateTechnicalState::Failed);
    cluster.outcome = Some(InputClusterOutcomeView::Accepted);
    assert_eq!(cluster_summary_status(cluster), ClusterSummaryStatus::TechnicalError);
    assert!(visible_text(&render_content(&card, &view), true).contains("LABEL.UPDATE_STATUS_FAILED"));
}

#[test]
fn cluster_quality_acceptance_without_publication_does_not_claim_technical_success() {
    let (_, mut view) = card_view(InputType::Xtream);
    let cluster = &mut view.cluster_results[0];
    cluster.source = Some(InputDataSourceView::Provider);
    cluster.outcome = Some(InputClusterOutcomeView::Accepted);
    cluster.technical_state = None;
    cluster.quality = Some(95);
    cluster.threshold = Some(90);
    assert_eq!(cluster_summary_status(cluster), ClusterSummaryStatus::Unavailable);
    assert!(matches!(quality_presentation(cluster, None), Some(QualityPresentation::Evaluated { quality: 95, .. })));
    cluster.technical_state = Some(PersistedPlaylistUpdateTechnicalState::Failed);
    assert_eq!(cluster_summary_status(cluster), ClusterSummaryStatus::TechnicalError);
    cluster.technical_state = Some(PersistedPlaylistUpdateTechnicalState::Succeeded);
    assert_eq!(cluster_summary_status(cluster), ClusterSummaryStatus::Accepted);
}

#[test]
fn cluster_success_survives_a_later_target_failure_in_the_same_run() {
    use crate::model::PlaylistUpdateCardStatusAction;
    use shared::model::{
        PlaylistUpdateInputTelemetry, PlaylistUpdateProgressEvent, PlaylistUpdateRunStateEvent, PlaylistUpdateState,
    };
    use std::rc::Rc;
    let sources = SourcesConfigDto {
        inputs: vec![ConfigInputDto { id: 17, enabled: true, input_type: InputType::Xtream, ..Default::default() }],
        ..Default::default()
    };
    let persisted = PlaylistUpdateStatusDto::default();
    let cards = build_input_update_card_models(&sources, &persisted);
    let connection = WebSocketConnectionContext::new(3, true);
    let telemetry: PlaylistUpdateInputTelemetry =
        serde_json::from_value(serde_json::json!({"refresh_policy":InputRefreshPolicy::NORMAL,"clusters":[
        {"cluster":"Live","requested":true,"source":"provider","technical_state":"succeeded"},
        {"cluster":"Video","requested":true,"source":"provider","technical_state":"succeeded"},
        {"cluster":"Series","requested":true,"source":"provider","technical_state":"succeeded"}]}))
        .unwrap();
    let state = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection))
        .reduce(PlaylistUpdateCardStatusAction::Progress {
            connection_context: connection,
            progress: PlaylistUpdateProgressEvent::input_completed(
                "input-then-target".into(),
                7.into(),
                17,
                PlaylistUpdateState::Success,
                "provider",
                "input complete",
            )
            .with_input_telemetry(telemetry),
        })
        .reduce(PlaylistUpdateCardStatusAction::Completed {
            connection_context: connection,
            completed: PlaylistUpdateRunStateEvent::correlated(
                "input-then-target".into(),
                7.into(),
                PlaylistUpdateState::Failure,
            ),
        });
    let view = build_input_update_run_views(&sources, &cards, &state, &persisted).remove(0);
    assert_eq!(view.input_state, InputUpdateCardStatus::Success);
    assert_eq!(view.overall_state, InputUpdateCardStatus::Failed);
    assert!(view
        .cluster_results
        .iter()
        .all(|cluster| cluster_summary_status(cluster) == ClusterSummaryStatus::Accepted));
    assert!(visible_text(&card_status_badge(&view, str::to_string), true).contains("UPDATE_STATUS_SUCCESS"));
}

#[test]
fn pipeline_transparency_compact_populations_keep_zero_missing_and_exact_large_differences() {
    assert_eq!(population_difference(None, Some(0)), None);
    assert_eq!(population_difference(Some(0), None), None);
    assert_eq!(population_difference(Some(0), Some(0)), None);
    assert_eq!(population_difference(Some(9705), Some(9673)).as_deref(), Some("−32"));
    assert_eq!(population_difference(Some(usize::MAX), Some(usize::MAX - 1)).as_deref(), Some("−1"));
    assert_eq!(population_difference(Some(0), Some(usize::MAX)), Some(format!("+{}", usize::MAX)));
    let (_, mut view) = card_view(InputType::Xtream);
    let cluster = &mut view.cluster_results[0];
    cluster.baseline_count = None;
    cluster.candidate_count = Some(0);
    cluster.active_count = Some(0);
    let rendered = population_details(cluster, &str::to_string);
    assert_eq!(tags(&rendered).iter().filter(|t| t.tag() == "dt").count(), 2);
    assert_eq!(tags(&rendered).iter().filter(|t| t.tag() == "dd").count(), 2);
    assert!(!visible_text(&rendered, true).contains("BASELINE"));
    assert!(!tags(&rendered).iter().any(|t| has_attribute(t, "role", "img")));
    cluster.candidate_count = None;
    cluster.active_count = None;
    assert_eq!(tags(&population_details(cluster, &str::to_string)).len(), 0);
}

#[test]
fn pipeline_transparency_compact_quality_keeps_accepted_separate_from_technical_failure() {
    let (_, mut view) = card_view(InputType::Xtream);
    let cluster = &mut view.cluster_results[0];
    cluster.threshold = Some(90);
    cluster.quality = Some(100);
    cluster.outcome = Some(InputClusterOutcomeView::Accepted);
    cluster.technical_state = Some(PersistedPlaylistUpdateTechnicalState::Failed);
    let rendered = cluster_details(17, cluster, None, &str::to_string);
    let text = visible_text(&rendered, true);
    assert!(text.contains("LABEL.UPDATE_STATUS_FAILED"));
    assert!(text.contains("DECISION") && text.contains("CLUSTER_ACCEPTED"));
    assert!(text.contains("100%") && text.contains("90%"));
    assert_eq!(tags(&rendered).iter().filter(|t| t.tag() == "svg").count(), 1);
    assert!(tags(&rendered).iter().any(|t| has_attribute(
        t,
        "class",
        "tp__playlist-update-view__quality-visual tp__playlist-update-view__quality-visual--accepted"
    )));
    cluster.outcome = Some(InputClusterOutcomeView::Rejected);
    cluster.technical_state = Some(PersistedPlaylistUpdateTechnicalState::Succeeded);
    cluster.quality = Some(0);
    let zero = cluster_details(17, cluster, None, &str::to_string);
    assert!(visible_text(&zero, true).contains("0%"));
    assert!(tags(&zero).iter().any(|t| t.tag() == "rect" && has_attribute(t, "width", "0")));
    assert!(!visible_text(&zero, true).contains("DECISION"), "same typed rejection is already in its cluster badge");
    for policy in [InputRefreshPolicy::FORCE, InputRefreshPolicy::NORMAL] {
        if policy == InputRefreshPolicy::NORMAL {
            cluster.quality = None;
        }
        let unknown = quality_details(cluster, Some(policy), &str::to_string);
        assert!(!tags(&unknown).iter().any(|t| t.tag() == "svg"));
    }
    cluster.threshold = None;
    let disabled = quality_details(cluster, None, &str::to_string);
    assert!(visible_text(&disabled, true).contains("DISABLED"));
    assert!(!tags(&disabled).iter().any(|t| t.tag() == "svg"));
}

#[test]
fn pipeline_transparency_compact_targets_have_fixed_rule_cells_ids_and_unknown_fallback() {
    let (_, mut view) = card_view(InputType::Xtream);
    view.target_results = [11, 12]
        .into_iter()
        .map(|id| TargetRunView {
            target_id: id,
            target_name: "duplicate".into(),
            input_ids: vec![17],
            pipeline: Some(TargetPipelineView {
                processing_order: shared::model::ProcessingOrder::Mrf,
                filter_summary: RuleStageSummary { configured_count: 9 },
                rename_summary: RuleStageSummary { configured_count: 0 },
                mapping_summary: RuleStageSummary { configured_count: 2 },
            }),
        })
        .collect();
    let rendered = target_details(&view, "targets".into(), &str::to_string);
    let all = tags(&rendered);
    assert_eq!(all.iter().filter(|t| has_attribute(t, "scope", "row")).count(), 2);
    assert_eq!(all.iter().filter(|t| has_attribute(t, "scope", "col")).count(), 5);
    for id in [11, 12] {
        assert!(all.iter().any(|t| has_attribute(t, "id", &format!("input-update-card-17-target-{id}-pipeline"))));
    }
    for row in &view.target_results {
        let rendered = target_pipeline_details(17, row, &str::to_string);
        let cells = tags(&rendered).into_iter().filter(|t| t.tag() == "td").collect::<Vec<_>>();
        assert_eq!(cells.len(), 4);
        for (cell, label, number) in [(cells[1], "FILTER", "9"), (cells[2], "RENAME", "0"), (cells[3], "MAPPING", "2")]
        {
            let text = visible_text(cell.children().unwrap(), true);
            assert!(text.contains(label) && text.contains(number));
            assert!(cell
                .attributes
                .iter()
                .any(|(k, v)| k == "headers" && v.contains(&format!("target-{}-pipeline", row.target_id))));
        }
    }
    view.target_results[0].pipeline = None;
    let missing = target_pipeline_details(17, &view.target_results[0], &str::to_string);
    assert!(visible_text(&missing, true).contains("DATA_UNAVAILABLE"));
    assert!(!tags(&missing).iter().any(|t| t.tag() == "bdi"));
    assert!(!tags(&rendered).iter().any(|t| t.attributes.iter().any(|(k, _)| k == "style" || k == "tabindex")));
}

#[test]
fn pipeline_transparency_compact_labels_exist_in_every_product_locale() {
    for locale in [
        include_str!("../../../../public/assets/i18n/en.json"),
        include_str!("../../../../public/assets/i18n/es.json"),
        include_str!("../../../../public/assets/i18n/ru.json"),
        include_str!("../../../../public/assets/i18n/ar.json"),
    ] {
        let values: serde_json::Value = serde_json::from_str(locale).unwrap();
        for key in ["CONFIGURED_RULES", "TARGET_ORDER", "POPULATION_DIFFERENCE", "QUALITY_GRAPH_LABEL"] {
            assert!(values["MESSAGES"]["PLAYLIST_UPDATE"][key].as_str().is_some_and(|s| !s.is_empty()));
        }
        assert!(values["MESSAGES"]["PLAYLIST_UPDATE"]["POPULATION_DIFFERENCE"]
            .as_str()
            .unwrap()
            .contains("{difference}"));
        for parameter in ["{quality}", "{threshold}"] {
            assert!(values["MESSAGES"]["PLAYLIST_UPDATE"]["QUALITY_GRAPH_LABEL"].as_str().unwrap().contains(parameter));
        }
    }
}
