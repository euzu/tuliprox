use indexmap::IndexMap;
use log::{debug, trace};
use regex::Regex;
use shared::{
    model::{
        FieldGet, FieldSet, HeaderField, PlaylistEntry, PlaylistGroup, PlaylistItem, PlaylistItemType, UUIDType,
        XtreamCluster,
    },
    utils::{deunicode_string, hash_string, Internable, CONSTANTS},
};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
};
use strsim::normalized_levenshtein;

static TRAILING_TITLE_YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(?(\d{4})\)?$").expect("curation title-year regex must compile"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CurationMediaKind {
    Movie,
    Series,
}

impl CurationMediaKind {
    const fn from_playlist_item_type(item_type: PlaylistItemType) -> Option<Self> {
        match item_type {
            PlaylistItemType::Video | PlaylistItemType::LocalVideo => Some(Self::Movie),
            PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo => Some(Self::Series),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CurationMediaScope {
    Movies,
    Series,
    Both,
}

impl CurationMediaScope {
    const fn includes_reference(self, kind: CurationMediaKind) -> bool {
        match self {
            Self::Movies => matches!(kind, CurationMediaKind::Movie),
            Self::Series => matches!(kind, CurationMediaKind::Series),
            Self::Both => true,
        }
    }

    const fn includes_cluster(self, cluster: XtreamCluster) -> bool {
        match self {
            Self::Movies => matches!(cluster, XtreamCluster::Video),
            Self::Series => matches!(cluster, XtreamCluster::Series),
            Self::Both => matches!(cluster, XtreamCluster::Video | XtreamCluster::Series),
        }
    }

    fn includes_playlist_item(self, item_type: PlaylistItemType) -> bool {
        match self {
            Self::Movies => item_type.is_video(),
            Self::Series => matches!(item_type, PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo),
            Self::Both => matches!(
                item_type,
                PlaylistItemType::Video
                    | PlaylistItemType::LocalVideo
                    | PlaylistItemType::SeriesInfo
                    | PlaylistItemType::LocalSeriesInfo
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CurationMatchPolicy {
    ExactTmdbOnly,
    ExactTmdbThenFuzzy { threshold_percent: u8 },
}

impl CurationMatchPolicy {
    fn fuzzy_threshold(self) -> Option<f64> {
        match self {
            Self::ExactTmdbOnly => None,
            Self::ExactTmdbThenFuzzy { threshold_percent } => Some(f64::from(threshold_percent) / 100.0),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ProjectionIdentityStrategy<'a> {
    LegacyCategoryScoped { namespace: &'a str },
}

impl ProjectionIdentityStrategy<'_> {
    fn projected_uuid(self, category_name: &str, source_uuid: UUIDType) -> UUIDType {
        match self {
            Self::LegacyCategoryScoped { namespace } => {
                hash_string(&format!("{namespace}:{category_name}:{source_uuid}"))
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CurationCategorySpec<'a> {
    pub(crate) name: &'a str,
    pub(crate) media_scope: CurationMediaScope,
    pub(crate) match_policy: CurationMatchPolicy,
    pub(crate) projection_identity: ProjectionIdentityStrategy<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CuratedMediaReference {
    pub(crate) kind: CurationMediaKind,
    pub(crate) title: String,
    normalized_title: String,
    pub(crate) year: Option<u32>,
    pub(crate) tmdb_id: Option<u32>,
    pub(crate) rank: Option<u32>,
}

impl CuratedMediaReference {
    pub(crate) fn new(
        kind: CurationMediaKind,
        title: String,
        year: Option<u32>,
        tmdb_id: Option<u32>,
        rank: Option<u32>,
    ) -> Self {
        let normalized_title = normalize_title_for_matching(&title);
        Self { kind, title, normalized_title, year, tmdb_id, rank }
    }
}

struct PlaylistCandidate<'a> {
    item: &'a PlaylistItem,
    kind: CurationMediaKind,
    normalized_title: String,
    year: Option<u32>,
    tmdb_id: Option<u32>,
}

struct MatchResult<'playlist, 'reference> {
    playlist_item: &'playlist PlaylistItem,
    reference: &'reference CuratedMediaReference,
}

pub(crate) fn normalize_title_for_matching(title: &str) -> String {
    let normalized = deunicode_string(title.trim());
    let mut result = String::with_capacity(normalized.len());

    for ch in normalized.chars() {
        if ch.is_alphanumeric() {
            result.push(ch.to_ascii_lowercase());
        }
    }

    if TRAILING_TITLE_YEAR.is_match(&result) {
        TRAILING_TITLE_YEAR.replace(&result, "").into_owned()
    } else {
        result
    }
}

pub(crate) fn extract_year_from_title(title: &str) -> Option<u32> {
    if let Some(captures) = TRAILING_TITLE_YEAR.captures(title) {
        if let Some(year_str) = captures.get(1) {
            if let Ok(year) = year_str.as_str().parse::<u32>() {
                if (1900..=2100).contains(&year) {
                    return Some(year);
                }
            }
        }
    }

    None
}

fn calculate_year_bonus(playlist_year: Option<u32>, reference_year: Option<u32>) -> f64 {
    if let (Some(playlist_year), Some(reference_year)) = (playlist_year, reference_year) {
        if playlist_year == reference_year {
            return 0.5;
        }
        return -0.5;
    }
    0.0
}

fn find_best_fuzzy_match_for_item<'playlist, 'reference>(
    candidate: &PlaylistCandidate<'playlist>,
    references: &'reference [CuratedMediaReference],
    specification: &CurationCategorySpec<'_>,
    threshold: f64,
) -> Option<MatchResult<'playlist, 'reference>> {
    let mut best_match: Option<(&CuratedMediaReference, f64)> = None;

    for reference in references.iter().filter(|reference| {
        specification.media_scope.includes_reference(reference.kind) && reference.kind == candidate.kind
    }) {
        let title_score = normalized_levenshtein(&candidate.normalized_title, &reference.normalized_title);

        if title_score >= threshold {
            let year_bonus = calculate_year_bonus(candidate.year, reference.year);
            let combined_score = (title_score + year_bonus).clamp(0.0, 1.0);

            if combined_score >= threshold {
                if best_match.is_none_or(|(_, current_best_score)| combined_score > current_best_score) {
                    best_match = Some((reference, combined_score));
                }
                if combined_score >= 0.99 {
                    break;
                }
            }
        }
    }

    if let Some((reference, combined_score)) = best_match {
        trace!(
            "Fuzzy curation match: '{}' -> '{}' (final: {combined_score:.3})",
            candidate.item.header.title,
            reference.title
        );
        return Some(MatchResult { playlist_item: candidate.item, reference });
    }

    None
}

fn find_best_match_for_item<'playlist, 'reference>(
    candidate: &PlaylistCandidate<'playlist>,
    references: &'reference [CuratedMediaReference],
    specification: &CurationCategorySpec<'_>,
) -> Option<MatchResult<'playlist, 'reference>> {
    if let Some(playlist_tmdb_id) = candidate.tmdb_id {
        for reference in references.iter().filter(|reference| {
            specification.media_scope.includes_reference(reference.kind) && reference.kind == candidate.kind
        }) {
            if Some(playlist_tmdb_id) == reference.tmdb_id {
                trace!("TMDB exact curation match: '{}' (TMDB: {})", candidate.item.header.title, playlist_tmdb_id);
                return Some(MatchResult { playlist_item: candidate.item, reference });
            }
        }
    }

    let threshold = specification.match_policy.fuzzy_threshold()?;
    find_best_fuzzy_match_for_item(candidate, references, specification, threshold)
}

pub(crate) fn curate_category(
    references: &[CuratedMediaReference],
    playlist: &[PlaylistGroup],
    specification: &CurationCategorySpec<'_>,
) -> Vec<PlaylistGroup> {
    let reference_count =
        references.iter().filter(|reference| specification.media_scope.includes_reference(reference.kind)).count();
    debug!(
        "Matching {reference_count} curated media references against playlist for media scope {:?}",
        specification.media_scope
    );

    let mut matches = Vec::new();
    for playlist_group in playlist {
        for channel in &playlist_group.channels {
            if specification.media_scope.includes_cluster(channel.header.xtream_cluster)
                && specification.media_scope.includes_playlist_item(channel.header.item_type)
            {
                let Some(kind) = CurationMediaKind::from_playlist_item_type(channel.header.item_type) else {
                    continue;
                };
                let candidate = PlaylistCandidate {
                    item: channel,
                    kind,
                    normalized_title: normalize_title_for_matching(&channel.header.title),
                    year: extract_year_from_title(&channel.header.title),
                    tmdb_id: channel.get_tmdb_id(),
                };
                if let Some(matched) = find_best_match_for_item(&candidate, references, specification) {
                    matches.push(matched);
                }
            }
        }
    }

    let series_children = series_children_by_parent_code(playlist);
    create_category_from_matches(matches, specification, &series_children)
}

fn create_category_from_matches(
    mut matches: Vec<MatchResult<'_, '_>>,
    specification: &CurationCategorySpec<'_>,
    series_children_by_parent_code: &HashMap<Arc<str>, Vec<&PlaylistItem>>,
) -> Vec<PlaylistGroup> {
    if matches.is_empty() {
        return Vec::new();
    }

    matches.sort_by(|left, right| {
        (left.reference.rank.unwrap_or(9999), left.reference.title.to_lowercase())
            .cmp(&(right.reference.rank.unwrap_or(9999), right.reference.title.to_lowercase()))
    });

    let group_title = specification.name.intern();
    let mut matched_items_by_cluster: IndexMap<XtreamCluster, Vec<PlaylistItem>> = IndexMap::new();

    for matched in matches {
        let projected_item = clone_item_for_category(matched.playlist_item, specification, &group_title);
        let parent_uuid = projected_item.header.uuid.intern();
        let is_series_info =
            matches!(projected_item.header.item_type, PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo);
        let child_lookup_keys =
            if is_series_info { series_info_child_lookup_keys(matched.playlist_item) } else { Vec::new() };
        let cluster = projected_item.header.xtream_cluster;
        matched_items_by_cluster.entry(cluster).or_default().push(projected_item);

        if let Some(children) = child_lookup_keys.iter().find_map(|key| series_children_by_parent_code.get(key)) {
            for child in children {
                let mut projected_child = clone_item_for_category(child, specification, &group_title);
                projected_child.header.parent_code = parent_uuid.clone();
                matched_items_by_cluster
                    .entry(projected_child.header.xtream_cluster)
                    .or_default()
                    .push(projected_child);
            }
        }
    }

    matched_items_by_cluster
        .into_iter()
        .map(|(cluster, channels)| PlaylistGroup {
            id: 0,
            title: group_title.clone(),
            channels,
            xtream_cluster: cluster,
        })
        .collect()
}

fn clone_item_for_category(
    item: &PlaylistItem,
    specification: &CurationCategorySpec<'_>,
    group_title: &Arc<str>,
) -> PlaylistItem {
    let mut projected_item = item.clone();
    let source_uuid = if projected_item.header.uuid == UUIDType::default() {
        projected_item.get_uuid()
    } else {
        projected_item.header.uuid
    };

    let header = &mut projected_item.header;
    let title = header.get(HeaderField::Caption).map_or_else(|| Arc::clone(&header.title), |value| value.to_arc());
    if extract_quality(&title).is_none() {
        if let Some(quality) = extract_quality(&header.group) {
            let mut caption = String::with_capacity(title.len() + 6);
            caption.push('[');
            caption.push_str(quality);
            caption.push_str("] ");
            caption.push_str(&title);
            header.set(HeaderField::Caption, &caption);
        }
    }
    header.group = group_title.clone();
    header.uuid = specification.projection_identity.projected_uuid(specification.name, source_uuid);

    projected_item
}

fn extract_quality(value: &str) -> Option<&str> {
    CONSTANTS.re_quality.captures(value).and_then(|captures| captures.get(0)).map(|value| value.as_str())
}

fn series_info_child_lookup_keys(series_info: &PlaylistItem) -> Vec<Arc<str>> {
    match series_info.header.item_type {
        PlaylistItemType::LocalSeriesInfo => vec![series_info.header.id.clone(), series_info.header.uuid.intern()],
        PlaylistItemType::SeriesInfo => vec![series_info.get_uuid().intern(), series_info.header.uuid.intern()],
        _ => Vec::new(),
    }
}

fn series_children_by_parent_code(playlist: &[PlaylistGroup]) -> HashMap<Arc<str>, Vec<&PlaylistItem>> {
    let mut children = HashMap::<Arc<str>, Vec<&PlaylistItem>>::new();
    for playlist_group in playlist {
        for channel in &playlist_group.channels {
            if channel.header.item_type.is_series() && !channel.header.parent_code.is_empty() {
                children.entry(channel.header.parent_code.clone()).or_default().push(channel);
            }
        }
    }
    children
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{
        EpisodeStreamProperties, PlaylistItemHeader, SeriesStreamProperties, StreamProperties, VideoStreamProperties,
        VirtualId,
    };

    #[test]
    fn title_normalization_and_year_extraction_preserve_existing_rules() {
        assert_eq!(normalize_title_for_matching("The Matrix"), "thematrix");
        assert_eq!(normalize_title_for_matching("Spider-Man: No Way Home"), "spidermannowayhome");
        assert_eq!(normalize_title_for_matching("Élite"), "elite");
        assert_eq!(normalize_title_for_matching("The Matrix (1999)"), "thematrix");
        assert_eq!(extract_year_from_title("The Matrix (1999)"), Some(1999));
        assert_eq!(extract_year_from_title("Avengers Endgame 2019"), Some(2019));
        assert_eq!(extract_year_from_title("Just a Title"), None);
    }

    #[test]
    fn exact_tmdb_match_precedes_a_perfect_fuzzy_title_match() {
        let playlist_item = video_item("Same Title", Some(222));
        let references = vec![
            reference(CurationMediaKind::Movie, "Same Title", None, Some(111), Some(1)),
            reference(CurationMediaKind::Movie, "Different Title", None, Some(222), Some(2)),
        ];
        let candidate = candidate(&playlist_item);

        let matched = find_best_match_for_item(&candidate, &references, &specification("Featured", false))
            .expect("TMDB identity should take precedence");

        assert_eq!(matched.reference.tmdb_id, Some(222));
        assert_eq!(matched.reference.title, "Different Title");
    }

    #[test]
    fn exact_only_policy_forbids_fuzzy_fallback_but_keeps_tmdb_matches() {
        let without_tmdb = video_item("The Captive", None);
        let matching_tmdb = video_item("Cautivos", Some(456));
        let references = vec![reference(CurationMediaKind::Movie, "The Captive", Some(1915), Some(456), Some(1))];
        let exact_only = specification("Featured", true);

        assert!(find_best_match_for_item(&candidate(&without_tmdb), &references, &exact_only).is_none());
        assert!(find_best_match_for_item(&candidate(&matching_tmdb), &references, &exact_only).is_some());
    }

    #[test]
    fn fuzzy_matching_preserves_year_bonus_and_penalty() {
        let playlist_item = video_item("The Matrix 1999", None);
        let candidate = candidate(&playlist_item);
        let matching_year = vec![reference(CurationMediaKind::Movie, "The Matrix", Some(1999), None, Some(1))];
        let different_year = vec![reference(CurationMediaKind::Movie, "The Matrix", Some(2000), None, Some(2))];
        let specification = specification("Featured", false);

        assert!(find_best_match_for_item(&candidate, &matching_year, &specification).is_some());
        assert!(find_best_match_for_item(&candidate, &different_year, &specification).is_none());
    }

    #[test]
    fn fuzzy_matching_keeps_the_first_perfect_reference() {
        let playlist_item = video_item("The Matrix", None);
        let references = vec![
            reference(CurationMediaKind::Movie, "The Matrix", None, Some(111), Some(1)),
            reference(CurationMediaKind::Movie, "The Matrix", None, Some(222), Some(2)),
        ];

        let matched =
            find_best_match_for_item(&candidate(&playlist_item), &references, &specification("Featured", false))
                .expect("a perfect fuzzy title should match");

        assert_eq!(matched.reference.tmdb_id, Some(111));
    }

    #[test]
    fn fuzzy_threshold_and_best_match_selection_remain_percentage_based() {
        let playlist_item = video_item("matrix", None);
        let references = vec![
            reference(CurationMediaKind::Movie, "matri", None, Some(111), Some(1)),
            reference(CurationMediaKind::Movie, "matrixx", None, Some(222), Some(2)),
        ];

        let matched =
            find_best_match_for_item(&candidate(&playlist_item), &references, &fuzzy_specification("Featured", 80))
                .expect("the best reference above an 80 percent threshold should match");

        assert_eq!(matched.reference.tmdb_id, Some(222));
        assert!(find_best_match_for_item(
            &candidate(&playlist_item),
            &references,
            &fuzzy_specification("Featured", 90),
        )
        .is_none());
    }

    #[test]
    fn projected_items_are_ordered_by_rank_then_lowercase_reference_title() {
        let playlist = vec![PlaylistGroup {
            id: 1,
            title: "Original".intern(),
            channels: vec![video_item("Gamma", Some(3)), video_item("zebra", Some(2)), video_item("Alpha", Some(1))],
            xtream_cluster: XtreamCluster::Video,
        }];
        let references = vec![
            reference(CurationMediaKind::Movie, "Gamma", None, Some(3), Some(2)),
            reference(CurationMediaKind::Movie, "zebra", None, Some(2), Some(1)),
            reference(CurationMediaKind::Movie, "Alpha", None, Some(1), Some(1)),
        ];

        let categories = curate_category(&references, &playlist, &specification("Ranked", true));
        let titles = categories[0].channels.iter().map(|item| item.header.title.as_ref()).collect::<Vec<_>>();

        assert_eq!(titles, ["Alpha", "zebra", "Gamma"]);
    }

    #[test]
    fn projected_categories_remain_grouped_by_playlist_cluster() {
        let playlist = vec![
            PlaylistGroup {
                id: 1,
                title: "Movies".intern(),
                channels: vec![video_item("Movie", Some(1))],
                xtream_cluster: XtreamCluster::Video,
            },
            PlaylistGroup {
                id: 2,
                title: "Series".intern(),
                channels: vec![series_item("Show", Some(2))],
                xtream_cluster: XtreamCluster::Series,
            },
        ];
        let references = vec![
            reference(CurationMediaKind::Movie, "Movie", None, Some(1), Some(1)),
            reference(CurationMediaKind::Series, "Show", None, Some(2), Some(2)),
        ];
        let specification =
            CurationCategorySpec { media_scope: CurationMediaScope::Both, ..specification("Mixed", true) };

        let categories = curate_category(&references, &playlist, &specification);

        assert_eq!(categories.len(), 2);
        assert_eq!(categories[0].xtream_cluster, XtreamCluster::Video);
        assert_eq!(categories[0].channels[0].header.title.as_ref(), "Movie");
        assert_eq!(categories[1].xtream_cluster, XtreamCluster::Series);
        assert_eq!(categories[1].channels[0].header.title.as_ref(), "Show");
    }

    #[test]
    fn both_scope_matches_references_only_to_the_same_media_kind() {
        let movie = video_item("Shared Title", Some(42));
        let series = series_item("Shared Title", Some(42));
        let exact_references = vec![
            reference(CurationMediaKind::Series, "Series Reference", None, Some(42), Some(1)),
            reference(CurationMediaKind::Movie, "Movie Reference", None, Some(42), Some(2)),
        ];
        let exact_specification =
            CurationCategorySpec { media_scope: CurationMediaScope::Both, ..specification("Mixed", true) };

        let movie_match = find_best_match_for_item(&candidate(&movie), &exact_references, &exact_specification)
            .expect("movie candidate should match the movie reference");
        let series_match = find_best_match_for_item(&candidate(&series), &exact_references, &exact_specification)
            .expect("series candidate should match the series reference");

        assert_eq!(movie_match.reference.kind, CurationMediaKind::Movie);
        assert_eq!(series_match.reference.kind, CurationMediaKind::Series);

        let fuzzy_references = vec![
            reference(CurationMediaKind::Series, "Shared Title", None, None, Some(1)),
            reference(CurationMediaKind::Movie, "Shared Title", None, None, Some(2)),
        ];
        let fuzzy_specification =
            CurationCategorySpec { media_scope: CurationMediaScope::Both, ..fuzzy_specification("Mixed", 100) };

        let movie_match = find_best_match_for_item(&candidate(&movie), &fuzzy_references, &fuzzy_specification)
            .expect("movie candidate should fuzzy-match the movie reference");
        let series_match = find_best_match_for_item(&candidate(&series), &fuzzy_references, &fuzzy_specification)
            .expect("series candidate should fuzzy-match the series reference");

        assert_eq!(movie_match.reference.kind, CurationMediaKind::Movie);
        assert_eq!(series_match.reference.kind, CurationMediaKind::Series);
    }

    #[test]
    fn movie_scope_does_not_match_series_roots_and_series_scope_ignores_episodes() {
        let series = series_item("Shared Title", Some(42));
        let episode = episode_item("Shared Title", &"parent".intern(), 7001);
        let playlist = vec![PlaylistGroup {
            id: 1,
            title: "Series".intern(),
            channels: vec![series, episode],
            xtream_cluster: XtreamCluster::Series,
        }];
        let movie_references = vec![reference(CurationMediaKind::Movie, "Shared Title", None, Some(42), Some(1))];
        let series_references = vec![reference(CurationMediaKind::Series, "Shared Title", None, None, Some(1))];

        assert!(curate_category(&movie_references, &playlist, &specification("Movies", true)).is_empty());
        assert!(curate_category(
            &series_references,
            &[PlaylistGroup {
                id: 1,
                title: "Episodes".intern(),
                channels: vec![playlist[0].channels[1].clone()],
                xtream_cluster: XtreamCluster::Series,
            }],
            &series_specification("Series", false),
        )
        .is_empty());
    }

    #[test]
    fn projection_propagates_group_quality_only_when_caption_has_none() {
        let mut without_quality = video_item("Clean Title", Some(1));
        without_quality.header.group = "Provider UHD".intern();
        let mut with_quality = video_item("Already 4K", Some(2));
        with_quality.header.group = "Provider UHD".intern();
        let playlist = vec![PlaylistGroup {
            id: 1,
            title: "Original".intern(),
            channels: vec![without_quality, with_quality],
            xtream_cluster: XtreamCluster::Video,
        }];
        let references = vec![
            reference(CurationMediaKind::Movie, "Clean Title", None, Some(1), Some(1)),
            reference(CurationMediaKind::Movie, "Already 4K", None, Some(2), Some(2)),
        ];

        let categories = curate_category(&references, &playlist, &specification("Featured", true));
        let clean = &categories[0].channels[0];
        let already_tagged = &categories[0].channels[1];

        assert_eq!(clean.header.get(HeaderField::Caption).expect("quality caption").as_cow(), "[UHD] Clean Title");
        assert_eq!(already_tagged.header.get(HeaderField::Caption).expect("existing caption").as_cow(), "Already 4K");
    }

    #[test]
    fn series_children_are_cloned_and_reparented_to_the_projected_root() {
        let mut series = series_item("Slow Horses", Some(12345));
        series.header.uuid = hash_string("series-source-item");
        let source_parent_code = series.header.uuid.intern();
        let mut episode = episode_item("Old Scores", &source_parent_code, 7001);
        episode.header.uuid = hash_string("episode-source-item");
        let playlist = vec![PlaylistGroup {
            id: 1,
            title: "Series".intern(),
            channels: vec![series, episode],
            xtream_cluster: XtreamCluster::Series,
        }];
        let references = vec![reference(CurationMediaKind::Series, "Slow Horses", Some(2022), Some(12345), Some(1))];

        let categories = curate_category(&references, &playlist, &series_specification("Trending", true));
        let cloned_series = categories[0]
            .channels
            .iter()
            .find(|item| item.header.item_type == PlaylistItemType::SeriesInfo)
            .expect("series info clone");
        let cloned_episode = categories[0]
            .channels
            .iter()
            .find(|item| item.header.item_type == PlaylistItemType::Series)
            .expect("episode clone");

        assert_eq!(cloned_episode.header.parent_code, cloned_series.header.uuid.intern());
        assert_ne!(cloned_episode.header.uuid, playlist[0].channels[1].header.uuid);
    }

    fn specification(category_name: &str, exact_only: bool) -> CurationCategorySpec<'_> {
        if exact_only {
            CurationCategorySpec {
                name: category_name,
                media_scope: CurationMediaScope::Movies,
                match_policy: CurationMatchPolicy::ExactTmdbOnly,
                projection_identity: ProjectionIdentityStrategy::LegacyCategoryScoped { namespace: "legacy-category" },
            }
        } else {
            fuzzy_specification(category_name, 100)
        }
    }

    fn fuzzy_specification(category_name: &str, threshold_percent: u8) -> CurationCategorySpec<'_> {
        CurationCategorySpec {
            name: category_name,
            media_scope: CurationMediaScope::Movies,
            match_policy: CurationMatchPolicy::ExactTmdbThenFuzzy { threshold_percent },
            projection_identity: ProjectionIdentityStrategy::LegacyCategoryScoped { namespace: "legacy-category" },
        }
    }

    fn series_specification(category_name: &str, exact_only: bool) -> CurationCategorySpec<'_> {
        CurationCategorySpec { media_scope: CurationMediaScope::Series, ..specification(category_name, exact_only) }
    }

    fn candidate(item: &PlaylistItem) -> PlaylistCandidate<'_> {
        PlaylistCandidate {
            item,
            kind: CurationMediaKind::from_playlist_item_type(item.header.item_type)
                .expect("test candidate must be a movie or series root"),
            normalized_title: normalize_title_for_matching(&item.header.title),
            year: extract_year_from_title(&item.header.title),
            tmdb_id: item.get_tmdb_id(),
        }
    }

    fn reference(
        kind: CurationMediaKind,
        title: &str,
        year: Option<u32>,
        tmdb_id: Option<u32>,
        rank: Option<u32>,
    ) -> CuratedMediaReference {
        CuratedMediaReference::new(kind, title.to_string(), year, tmdb_id, rank)
    }

    fn video_item(title: &str, tmdb: Option<u32>) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                title: title.intern(),
                xtream_cluster: XtreamCluster::Video,
                item_type: PlaylistItemType::Video,
                additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                    name: title.intern(),
                    tmdb,
                    ..VideoStreamProperties::default()
                }))),
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn series_item(title: &str, tmdb: Option<u32>) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                id: format!("series-{title}").intern(),
                input_name: "input".intern(),
                title: title.intern(),
                name: title.intern(),
                url: format!("media-server://unavailable/server/shows/{title}").intern(),
                xtream_cluster: XtreamCluster::Series,
                item_type: PlaylistItemType::SeriesInfo,
                additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                    name: title.intern(),
                    tmdb,
                    ..SeriesStreamProperties::default()
                }))),
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn episode_item(title: &str, parent_code: &Arc<str>, virtual_id: u32) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                uuid: hash_string(&format!("episode:{title}:{virtual_id}")),
                id: format!("episode-{virtual_id}").intern(),
                input_name: "input".intern(),
                parent_code: parent_code.clone(),
                title: title.intern(),
                name: title.intern(),
                url: format!("media-server://plex/server/{virtual_id}?part_key=%2Flibrary%2Fparts%2Fredacted").intern(),
                virtual_id: VirtualId::new(virtual_id),
                xtream_cluster: XtreamCluster::Series,
                item_type: PlaylistItemType::Series,
                additional_properties: Some(StreamProperties::Episode(Box::new(EpisodeStreamProperties {
                    episode_id: virtual_id,
                    episode: 1,
                    season: 1,
                    added: None,
                    release_date: None,
                    series_release_date: None,
                    tmdb: None,
                    movie_image: "".intern(),
                    container_extension: "mkv".intern(),
                    video: None,
                    audio: None,
                    plot: None,
                }))),
                ..PlaylistItemHeader::default()
            },
        }
    }
}
