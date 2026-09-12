use super::detail_row;
use crate::model::InputUpdateRunView;
use yew::{html, Html};

pub(super) fn library_details(view: &InputUpdateRunView, translate: impl Fn(&str) -> String) -> Html {
    let catalog = view.library_catalog.as_ref();
    let scan = view.library_scan_result.as_ref();
    let label = |key| translate(&format!("MESSAGES.PLAYLIST_UPDATE.LIBRARY_{key}"));
    let sections = [
        (
            "CATALOG",
            vec![
                (translate(super::cluster_label_key(shared::model::XtreamCluster::Video)), catalog.map(|c| c.movies)),
                (translate(super::cluster_label_key(shared::model::XtreamCluster::Series)), catalog.map(|c| c.series)),
                (label("EPISODES"), catalog.map(|c| c.episodes)),
                (label("TOTAL_ITEMS"), catalog.map(|c| c.total_items)),
            ],
        ),
        (
            "LAST_RESCAN",
            vec![
                (label("FILES_SCANNED"), scan.map(|s| s.files_scanned)),
                (label("GROUPS_SCANNED"), scan.map(|s| s.groups_scanned)),
                (label("ADDED"), scan.map(|s| s.files_added)),
                (label("UPDATED"), scan.map(|s| s.files_updated)),
                (label("REMOVED"), scan.map(|s| s.files_removed)),
                (label("ERRORS"), scan.map(|s| s.errors)),
            ],
        ),
    ];
    html! {
        {for sections.into_iter().map(|(title, rows)| html! {
            <section class="tp__playlist-update-view__cluster-detail">
                <h4>{translate(&format!("MESSAGES.PLAYLIST_UPDATE.LIBRARY_{title}"))}</h4>
                <dl>
                    {for rows.into_iter().map(|(label, value)| detail_row(
                        label,
                        value.map_or_else(|| "—".into(), |value| value.to_string()),
                    ))}
                </dl>
            </section>
        })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{build_input_update_card_models, build_input_update_run_views, PlaylistUpdateCardStatuses};
    use shared::model::{
        ConfigInputDto, InputType, LibraryScanResult, LibraryStatus, PlaylistUpdateStatusDto, SourcesConfigDto,
    };

    #[test]
    fn pipeline_transparency_input_update_card_library_renders_catalog_and_real_scan_counts_or_unknown() {
        let sources = SourcesConfigDto {
            inputs: vec![ConfigInputDto { id: 17, input_type: InputType::Library, ..ConfigInputDto::default() }],
            ..SourcesConfigDto::default()
        };
        let persisted = PlaylistUpdateStatusDto::default();
        let cards = build_input_update_card_models(&sources, &persisted);
        let mut view =
            build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted)
                .remove(0);
        for locale in [
            include_str!("../../../../public/assets/i18n/en.json"),
            include_str!("../../../../public/assets/i18n/ru.json"),
            include_str!("../../../../public/assets/i18n/ar.json"),
        ] {
            let messages: serde_json::Value = serde_json::from_str(locale).unwrap();
            for label in [
                "CATALOG",
                "MOVIES",
                "SERIES",
                "EPISODES",
                "TOTAL_ITEMS",
                "LAST_RESCAN",
                "FILES_SCANNED",
                "GROUPS_SCANNED",
                "ADDED",
                "UPDATED",
                "REMOVED",
                "ERRORS",
            ] {
                assert!(messages["MESSAGES"]["PLAYLIST_UPDATE"][format!("LIBRARY_{label}")]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()));
            }
        }
        let translate = |key: &str| key.rsplit('.').next().unwrap().to_string();
        let unknown = format!("{:?}", library_details(&view, translate));
        assert_eq!(unknown.matches('—').count(), 10);
        view.library_catalog =
            Some(LibraryStatus { enabled: true, movies: 2, series: 3, episodes: 19, total_items: 5, path: None });
        let reload = format!("{:?}", library_details(&view, translate));
        assert_eq!(reload.matches('—').count(), 6, "Only rescan values remain unknown after catalog reload");
        view.library_scan_result = Some(LibraryScanResult {
            files_scanned: 11,
            groups_scanned: 7,
            files_added: 3,
            files_updated: 2,
            files_removed: 1,
            errors: 4,
        });
        let rendered = format!("{:?}", library_details(&view, translate));
        assert!(rendered.contains("CONTENT_MOVIES") && rendered.contains("CONTENT_SHOWS"));
        for label in [
            "CATALOG",
            "EPISODES",
            "TOTAL_ITEMS",
            "LAST_RESCAN",
            "FILES_SCANNED",
            "GROUPS_SCANNED",
            "ADDED",
            "UPDATED",
            "REMOVED",
            "ERRORS",
        ] {
            assert!(rendered.contains(&format!("LIBRARY_{label}")), "{rendered}");
        }
        assert!(!rendered.contains('—'));
        assert!(!rendered.contains("PROVIDER") && !rendered.contains("CACHE") && !rendered.contains("QUALITY"));
        assert!(rendered.contains("19") && rendered.contains("11") && rendered.contains('4'));
    }
}
