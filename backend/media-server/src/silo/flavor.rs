use crate::{
    catalog::MediaServerCatalogSnapshot,
    emby::flavor::{
        impl_emby_family_flavor, sensitive_header_value, validate_token, EmbyClientIdentity, EmbyEndpoint, EmbyPath,
        UserScope,
    },
    MediaServerEpisode, MediaServerError, MediaServerSeason,
};
use reqwest::header::{HeaderMap, AUTHORIZATION};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug, Clone, Copy)]
pub struct Silo;

impl_emby_family_flavor!(Silo, "silo", {
    fn auth_headers(identity: &EmbyClientIdentity, token: Option<&str>) -> Result<HeaderMap, MediaServerError> {
        let token = token.map(|token| validate_token(token, "silo")).transpose()?;
        let mut headers = HeaderMap::with_capacity(1);
        headers.insert(AUTHORIZATION, sensitive_header_value(&identity.media_browser_value(token), "silo")?);
        Ok(headers)
    }

    fn items_path(user_id: Option<&str>) -> EmbyEndpoint<'_> {
        let user_scope = if user_id.is_some() { UserScope::Query } else { UserScope::None };
        EmbyEndpoint { path: EmbyPath::one("Items"), user_scope }
    }

    fn user_views_path(_user_id: &str) -> EmbyEndpoint<'_> {
        EmbyEndpoint { path: EmbyPath::one("UserViews"), user_scope: UserScope::Query }
    }

    const RESOLVES_USER_VIA_USERS_ME: bool = true;
    const SUPPORTS_VIRTUAL_FOLDERS: bool = false;

    const DISCOVERY_HINT: Option<&'static str> = Some(
        "point the input at the Silo Jellyfin-compatibility port (default 8096) and enable the compatibility in Silo",
    );

    fn post_process_catalog(snapshot: &mut MediaServerCatalogSnapshot) {
        if snapshot.seasons.is_empty() {
            snapshot.seasons = synthesize_seasons_from_episodes(&snapshot.episodes);
        }
    }
});

type SeriesKey = (Arc<str>, Arc<str>, Arc<str>);

struct SynthesizedSeason {
    season: MediaServerSeason,
    has_real_id: bool,
}

fn season_title(season_num: Option<u32>) -> Arc<str> {
    match season_num {
        Some(0) => Arc::from("Specials"),
        Some(n) => Arc::from(format!("Season {n}")),
        None => Arc::from("Season"),
    }
}

/// Synthesizes season catalog entries from episode metadata when a server returns an empty season list.
pub fn synthesize_seasons_from_episodes(episodes: &[MediaServerEpisode]) -> Vec<MediaServerSeason> {
    let mut seasons_by_series: HashMap<SeriesKey, Vec<SynthesizedSeason>> = HashMap::new();

    for ep in episodes {
        let Some(series_id) = ep.series_id.as_ref() else { continue };
        let season_num = ep.season;
        let season_id = ep.season_id.as_ref();

        // Episodes without season info follow existing mapper behavior and are not synthesized into seasons.
        if season_id.is_none() && season_num.is_none() {
            continue;
        }

        let series_key = (Arc::clone(&ep.server_id), Arc::clone(&ep.library_id), Arc::clone(series_id));
        let series_seasons = seasons_by_series.entry(series_key).or_default();

        let matched = series_seasons.iter_mut().find(|s| {
            // Both have real season_id: must match exactly
            if let (Some(ep_sid), true) = (season_id, s.has_real_id) {
                return s.season.item_id.as_ref() == ep_sid.as_ref();
            }
            // Same season number within the series
            if let (Some(ep_snum), Some(s_num)) = (season_num, s.season.season) {
                if ep_snum == s_num {
                    return true;
                }
            }
            // Same item_id (e.g. synthetic or matching)
            if let Some(ep_sid) = season_id {
                if s.season.item_id.as_ref() == ep_sid.as_ref() {
                    return true;
                }
            }
            false
        });

        if let Some(s) = matched {
            // If the existing season had a synthetic ID and this episode has a real season_id, adopt it
            if let Some(ep_sid) = season_id {
                if !s.has_real_id {
                    s.season.item_id = Arc::clone(ep_sid);
                    s.has_real_id = true;
                }
            }
            // If created without season number, adopt later episode's number and title
            if s.season.season.is_none() && season_num.is_some() {
                s.season.season = season_num;
                s.season.title = season_title(season_num);
            }
            if let Some(count) = s.season.episode_count.as_mut() {
                *count = count.saturating_add(1);
            }
        } else {
            let title = season_title(season_num);
            let has_real_id = season_id.is_some();
            let key_id = season_id
                .cloned()
                .unwrap_or_else(|| Arc::from(format!("{series_id}-season-{}", season_num.unwrap_or_default())));

            let season = MediaServerSeason {
                input_name: Arc::clone(&ep.input_name),
                server_id: Arc::clone(&ep.server_id),
                library_id: Arc::clone(&ep.library_id),
                item_id: key_id,
                series_id: Some(Arc::clone(series_id)),
                series_title: ep.series_title.clone(),
                title,
                season: season_num,
                year: None,
                release_date: ep.release_date.clone(),
                source_version_hint: ep.source_version_hint.clone(),
                provider_hints: Vec::new(),
                descriptive_facts: None,
                episode_count: Some(1),
                image_ref: None,
            };
            series_seasons.push(SynthesizedSeason { season, has_real_id });
        }
    }

    let mut result: Vec<MediaServerSeason> =
        seasons_by_series.into_values().flat_map(|seasons| seasons.into_iter().map(|s| s.season)).collect();
    result.sort_by(|a, b| {
        a.season.unwrap_or_default().cmp(&b.season.unwrap_or_default()).then_with(|| a.item_id.cmp(&b.item_id))
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{emby::flavor::EmbyFamilyFlavor, MediaServerErrorKind};
    use shared::utils::short_hash;

    fn identity() -> EmbyClientIdentity { EmbyClientIdentity::new("silo_input", Some("alice")) }

    fn expected_identity() -> String {
        format!(
            "MediaBrowser Client=\"Tuliprox\", Device=\"Tuliprox\", DeviceId=\"tuliprox-{}\", Version=\"{}\"",
            short_hash("silo_input\u{1f}alice"),
            env!("CARGO_PKG_VERSION")
        )
    }

    #[test]
    fn auth_headers_use_only_the_authorization_header() {
        let headers = Silo::auth_headers(&identity(), Some("secret")).expect("headers build");

        assert_eq!(headers.len(), 1);
        let value = headers.get(AUTHORIZATION).expect("authorization header").to_str().expect("ascii");
        assert_eq!(value, format!("{}, Token=\"secret\"", expected_identity()));
        assert!(headers.get(AUTHORIZATION).expect("authorization header").is_sensitive());
    }

    #[test]
    fn identity_only_headers_carry_no_token() {
        let headers = Silo::auth_headers(&identity(), None).expect("headers build");

        assert_eq!(headers.len(), 1);
        assert_eq!(
            headers.get(AUTHORIZATION).expect("authorization header").to_str().expect("ascii"),
            expected_identity()
        );
    }

    #[test]
    fn tokens_that_cannot_be_quoted_are_auth_denied() {
        for token in ["a\"b", "a\\b", "a b", "a\nb", "tök\u{e9}n", ""] {
            let error = Silo::auth_headers(&identity(), Some(token)).expect_err("token is rejected");

            assert_eq!(error.kind, MediaServerErrorKind::MediaServerAuthDenied, "token {token:?}");
        }
    }

    #[test]
    fn paths_scope_to_the_user_with_a_query_parameter() {
        assert_eq!(Silo::items_path(None), EmbyEndpoint { path: EmbyPath::one("Items"), user_scope: UserScope::None });
        assert_eq!(
            Silo::items_path(Some("u1")),
            EmbyEndpoint { path: EmbyPath::one("Items"), user_scope: UserScope::Query }
        );
        assert_eq!(
            Silo::user_views_path("u1"),
            EmbyEndpoint { path: EmbyPath::one("UserViews"), user_scope: UserScope::Query }
        );
    }

    fn sample_episode(
        id: &str,
        series_id: Option<&str>,
        season_id: Option<&str>,
        season_num: Option<u32>,
        ep_num: Option<u32>,
    ) -> MediaServerEpisode {
        MediaServerEpisode {
            input_name: "silo_in".into(),
            server_id: "srv1".into(),
            library_id: "lib1".into(),
            item_id: id.into(),
            season_id: season_id.map(Arc::from),
            series_id: series_id.map(Arc::from),
            series_title: Some("Series Title".into()),
            title: format!("Episode {id}").into(),
            season: season_num,
            episode: ep_num,
            release_date: None,
            source_version_hint: None,
            provider_hints: Vec::new(),
            descriptive_facts: None,
            technical_facts: None,
            stream_ref: None,
            image_ref: None,
        }
    }

    #[test]
    fn season_synthesis_from_episodes_synthesizes_correctly_including_specials_and_counts() {
        let episodes = vec![
            sample_episode("e0", Some("show1"), Some("season_specials"), Some(0), Some(1)),
            sample_episode("e1", Some("show1"), Some("season_1"), Some(1), Some(1)),
            sample_episode("e2", Some("show1"), Some("season_1"), Some(1), Some(2)),
            sample_episode("e3", Some("show1"), Some("season_2"), Some(2), Some(1)),
            // Episode without season info
            sample_episode("e4", Some("show1"), None, None, Some(99)),
        ];

        let seasons = synthesize_seasons_from_episodes(&episodes);
        assert_eq!(seasons.len(), 3);

        // Specials (season 0)
        assert_eq!(seasons[0].item_id.as_ref(), "season_specials");
        assert_eq!(seasons[0].season, Some(0));
        assert_eq!(seasons[0].title.as_ref(), "Specials");
        assert_eq!(seasons[0].episode_count, Some(1));
        assert_eq!(seasons[0].series_id.as_deref(), Some("show1"));

        // Season 1 (2 episodes)
        assert_eq!(seasons[1].item_id.as_ref(), "season_1");
        assert_eq!(seasons[1].season, Some(1));
        assert_eq!(seasons[1].title.as_ref(), "Season 1");
        assert_eq!(seasons[1].episode_count, Some(2));
        assert_eq!(seasons[1].series_id.as_deref(), Some("show1"));

        // Season 2 (1 episode)
        assert_eq!(seasons[2].item_id.as_ref(), "season_2");
        assert_eq!(seasons[2].season, Some(2));
        assert_eq!(seasons[2].title.as_ref(), "Season 2");
        assert_eq!(seasons[2].episode_count, Some(1));
    }

    #[test]
    fn season_synthesis_does_not_synthesize_when_seasons_already_exist() {
        let existing = vec![MediaServerSeason {
            input_name: "silo_in".into(),
            server_id: "srv1".into(),
            library_id: "lib1".into(),
            item_id: "existing_season_1".into(),
            series_id: Some("show1".into()),
            series_title: Some("Show".into()),
            title: "Existing Season 1".into(),
            season: Some(1),
            year: None,
            release_date: None,
            source_version_hint: None,
            provider_hints: Vec::new(),
            descriptive_facts: None,
            episode_count: Some(10),
            image_ref: None,
        }];

        let episodes = vec![sample_episode("e1", Some("show1"), Some("season_1"), Some(1), Some(1))];

        let mut snapshot =
            MediaServerCatalogSnapshot { seasons: existing, episodes, ..MediaServerCatalogSnapshot::default() };
        Silo::post_process_catalog(&mut snapshot);
        assert_eq!(snapshot.seasons.len(), 1);
        assert_eq!(snapshot.seasons[0].item_id.as_ref(), "existing_season_1");
        assert_eq!(snapshot.seasons[0].title.as_ref(), "Existing Season 1");
    }

    #[test]
    fn seasons_without_season_ids_stay_separate_per_series() {
        let episodes = vec![
            sample_episode("a1", Some("show_a"), None, Some(1), Some(1)),
            sample_episode("b1", Some("show_b"), None, Some(1), Some(1)),
            sample_episode("b2", Some("show_b"), None, Some(1), Some(2)),
        ];
        let seasons = synthesize_seasons_from_episodes(&episodes);
        assert_eq!(seasons.len(), 2, "one season 1 per series: {seasons:?}");
        let count_of = |series: &str| {
            seasons.iter().find(|season| season.series_id.as_deref() == Some(series)).and_then(|s| s.episode_count)
        };
        assert_eq!(count_of("show_a"), Some(1));
        assert_eq!(count_of("show_b"), Some(2));
    }

    #[test]
    fn season_identity_normalized_when_episode_with_season_id_comes_first() {
        let episodes = vec![
            sample_episode("e1", Some("show1"), Some("real_s1"), Some(1), Some(1)),
            sample_episode("e2", Some("show1"), None, Some(1), Some(2)),
        ];
        let seasons = synthesize_seasons_from_episodes(&episodes);
        assert_eq!(seasons.len(), 1, "both episodes must merge into one season");
        assert_eq!(seasons[0].item_id.as_ref(), "real_s1");
        assert_eq!(seasons[0].season, Some(1));
        assert_eq!(seasons[0].title.as_ref(), "Season 1");
        assert_eq!(seasons[0].episode_count, Some(2));
    }

    #[test]
    fn season_identity_normalized_when_episode_without_season_id_comes_first() {
        let episodes = vec![
            sample_episode("e1", Some("show1"), None, Some(1), Some(1)),
            sample_episode("e2", Some("show1"), Some("real_s1"), Some(1), Some(2)),
        ];
        let seasons = synthesize_seasons_from_episodes(&episodes);
        assert_eq!(seasons.len(), 1, "both episodes must merge into one season");
        assert_eq!(seasons[0].item_id.as_ref(), "real_s1", "must adopt real season_id");
        assert_eq!(seasons[0].season, Some(1));
        assert_eq!(seasons[0].title.as_ref(), "Season 1");
        assert_eq!(seasons[0].episode_count, Some(2));
    }

    #[test]
    fn entry_created_without_season_number_adopts_later_episode_season_number_and_title() {
        let episodes = vec![
            sample_episode("e1", Some("show1"), Some("real_s2"), None, Some(1)),
            sample_episode("e2", Some("show1"), Some("real_s2"), Some(2), Some(2)),
        ];
        let seasons = synthesize_seasons_from_episodes(&episodes);
        assert_eq!(seasons.len(), 1);
        assert_eq!(seasons[0].item_id.as_ref(), "real_s2");
        assert_eq!(seasons[0].season, Some(2));
        assert_eq!(seasons[0].title.as_ref(), "Season 2");
        assert_eq!(seasons[0].episode_count, Some(2));
    }

    #[test]
    fn distinct_real_seasons_stay_distinct_even_if_season_number_matches() {
        let episodes = vec![
            sample_episode("e1", Some("show1"), Some("s1_a"), Some(1), Some(1)),
            sample_episode("e2", Some("show1"), Some("s1_b"), Some(1), Some(2)),
        ];
        let seasons = synthesize_seasons_from_episodes(&episodes);
        assert_eq!(seasons.len(), 2, "distinct real season IDs must stay distinct");
    }
}
