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

type SeasonKey = (Arc<str>, Arc<str>, Arc<str>);

/// Synthesizes season catalog entries from episode metadata when a server returns an empty season list.
pub fn synthesize_seasons_from_episodes(episodes: &[MediaServerEpisode]) -> Vec<MediaServerSeason> {
    let mut seasons_map: HashMap<SeasonKey, MediaServerSeason> = HashMap::new();

    for ep in episodes {
        let Some(series_id) = ep.series_id.as_ref() else { continue };
        let season_num = ep.season;
        let season_id = ep.season_id.as_ref();

        // Episodes without season info follow existing mapper behavior and are not synthesized into seasons.
        if season_id.is_none() && season_num.is_none() {
            continue;
        }

        // Without a season id the season number is only unique within its series.
        let key_id = season_id
            .cloned()
            .unwrap_or_else(|| Arc::from(format!("{series_id}-season-{}", season_num.unwrap_or_default())));
        let map_key = (Arc::clone(&ep.server_id), Arc::clone(&ep.library_id), Arc::clone(&key_id));

        let entry = seasons_map.entry(map_key).or_insert_with(|| {
            let title = match season_num {
                Some(0) => Arc::from("Specials"),
                Some(n) => Arc::from(format!("Season {n}")),
                None => Arc::from("Season"),
            };
            MediaServerSeason {
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
                episode_count: Some(0),
                image_ref: None,
            }
        });

        if let Some(count) = entry.episode_count.as_mut() {
            *count = count.saturating_add(1);
        }
    }

    let mut result: Vec<MediaServerSeason> = seasons_map.into_values().collect();
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
}
