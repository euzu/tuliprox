use crate::{
    library::MediaMetadata,
    media_enrichment::{
        facts::{build_missing_fact_patch, MediaFactPatch, MediaItemFacts},
        parsed_title::supplied_release_year_from_title,
        tmdb::supplied_facts_from_metadata,
    },
};
use shared::model::{SeriesStreamProperties, VideoStreamDetailProperties, VideoStreamProperties};
use std::sync::Arc;

trait FactSource {
    fn current_facts(&self) -> MediaItemFacts;
}

impl FactSource for VideoStreamProperties {
    fn current_facts(&self) -> MediaItemFacts { video_current_facts(self) }
}

impl FactSource for SeriesStreamProperties {
    fn current_facts(&self) -> MediaItemFacts { series_current_facts(self) }
}

fn fact_patch_from_metadata<S: FactSource>(properties: &S, metadata: &MediaMetadata) -> MediaFactPatch {
    build_missing_fact_patch(&properties.current_facts(), &supplied_facts_from_metadata(metadata))
}

fn fact_patch_from_title<S: FactSource>(properties: &S, title: &str) -> Option<(u32, MediaFactPatch)> {
    let current = properties.current_facts();
    let (year, supplied) = supplied_release_year_from_title(current.kind, title)?;
    Some((year, build_missing_fact_patch(&current, &supplied)))
}

fn fact_patch_from_title_candidates<'a, S, I>(properties: &S, titles: I) -> Option<(&'a str, u32, MediaFactPatch)>
where
    S: FactSource,
    I: IntoIterator<Item = &'a str>,
{
    titles.into_iter().filter(|title| !title.is_empty()).find_map(|title| {
        let (year, patch) = fact_patch_from_title(properties, title)?;
        (!patch.is_empty()).then_some((title, year, patch))
    })
}

pub fn distinct_non_empty_title_candidates<'a, I>(titles: I) -> Vec<&'a str>
where
    I: IntoIterator<Item = Option<&'a str>>,
{
    let mut result = Vec::new();
    for title in titles.into_iter().flatten().filter(|title| !title.is_empty()) {
        if !result.contains(&title) {
            result.push(title);
        }
    }
    result
}

pub fn video_fact_patch_from_metadata(properties: &VideoStreamProperties, metadata: &MediaMetadata) -> MediaFactPatch {
    fact_patch_from_metadata(properties, metadata)
}

pub fn video_fact_patch_from_title(properties: &VideoStreamProperties, title: &str) -> Option<(u32, MediaFactPatch)> {
    fact_patch_from_title(properties, title)
}

pub fn video_fact_patch_from_title_candidates<'a, I>(
    properties: &VideoStreamProperties,
    titles: I,
) -> Option<(&'a str, u32, MediaFactPatch)>
where
    I: IntoIterator<Item = &'a str>,
{
    fact_patch_from_title_candidates(properties, titles)
}

pub fn series_fact_patch_from_metadata(
    properties: &SeriesStreamProperties,
    metadata: &MediaMetadata,
) -> MediaFactPatch {
    fact_patch_from_metadata(properties, metadata)
}

pub fn series_fact_patch_from_title(properties: &SeriesStreamProperties, title: &str) -> Option<(u32, MediaFactPatch)> {
    fact_patch_from_title(properties, title)
}

pub fn series_fact_patch_from_title_candidates<'a, I>(
    properties: &SeriesStreamProperties,
    titles: I,
) -> Option<(&'a str, u32, MediaFactPatch)>
where
    I: IntoIterator<Item = &'a str>,
{
    fact_patch_from_title_candidates(properties, titles)
}

pub fn apply_fact_patch_to_video(properties: &mut VideoStreamProperties, patch: &MediaFactPatch) -> bool {
    let mut changed = false;

    if properties.tmdb.is_none() {
        if let Some(tmdb_id) = patch.tmdb_id {
            properties.tmdb = Some(tmdb_id);
            changed = true;
        }
    }

    if let Some(release_date) = patch.release_date.as_deref() {
        match properties.details.as_mut() {
            Some(details) if details.release_date.is_none() => {
                details.release_date = Some(Arc::<str>::from(release_date));
                changed = true;
            }
            None => {
                properties.details = Some(VideoStreamDetailProperties {
                    release_date: Some(Arc::<str>::from(release_date)),
                    ..VideoStreamDetailProperties::default()
                });
                changed = true;
            }
            Some(_) => {}
        }
    }

    changed
}

pub fn apply_fact_patch_to_series(properties: &mut SeriesStreamProperties, patch: &MediaFactPatch) -> bool {
    let mut changed = false;

    if properties.tmdb.is_none() {
        if let Some(tmdb_id) = patch.tmdb_id {
            properties.tmdb = Some(tmdb_id);
            changed = true;
        }
    }

    if properties.release_date.is_none() {
        if let Some(release_date) = patch.release_date.as_deref() {
            properties.release_date = Some(Arc::<str>::from(release_date));
            changed = true;
        }
    }

    changed
}

fn video_current_facts(properties: &VideoStreamProperties) -> MediaItemFacts {
    MediaItemFacts::movie(
        properties.tmdb,
        properties.details.as_ref().and_then(|details| details.release_date.as_ref()).map(Arc::clone),
    )
}

fn series_current_facts(properties: &SeriesStreamProperties) -> MediaItemFacts {
    MediaItemFacts::series(properties.tmdb, properties.release_date.as_ref().map(Arc::clone))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_video_patch_without_overwriting_existing_facts() {
        let mut properties = VideoStreamProperties {
            tmdb: Some(603),
            details: Some(VideoStreamDetailProperties {
                release_date: Some("1999-03-31".into()),
                ..VideoStreamDetailProperties::default()
            }),
            ..VideoStreamProperties::default()
        };
        let patch = MediaFactPatch { tmdb_id: Some(999), release_date: Some("2000-01-01".to_string()) };

        assert!(!apply_fact_patch_to_video(&mut properties, &patch));
        assert_eq!(properties.tmdb, Some(603));
        assert_eq!(properties.details.as_ref().and_then(|details| details.release_date.as_deref()), Some("1999-03-31"));
    }

    #[test]
    fn builds_video_patch_from_parseable_title() {
        let properties = VideoStreamProperties::default();
        let Some((year, patch)) = video_fact_patch_from_title(&properties, "The Matrix 1999") else {
            panic!("expected title year patch");
        };

        assert_eq!(year, 1999);
        assert_eq!(patch.release_date.as_deref(), Some("1999-01-01"));
    }

    #[test]
    fn builds_video_patch_from_first_parseable_title_candidate() {
        let properties = VideoStreamProperties { tmdb: Some(603), ..VideoStreamProperties::default() };

        let Some((title, year, patch)) =
            video_fact_patch_from_title_candidates(&properties, ["No year", "The Matrix 1999"])
        else {
            panic!("expected fallback title year patch");
        };

        assert_eq!(title, "The Matrix 1999");
        assert_eq!(year, 1999);
        assert_eq!(patch.release_date.as_deref(), Some("1999-01-01"));
        assert!(patch.tmdb_id.is_none());
    }

    #[test]
    fn title_candidates_drop_empty_values_and_preserve_first_distinct_names() {
        let candidates = distinct_non_empty_title_candidates([
            Some(""),
            Some("The Matrix 1999"),
            Some("The Matrix 1999"),
            Some("Matrix 1999"),
        ]);

        assert_eq!(candidates, vec!["The Matrix 1999", "Matrix 1999"]);
    }

    #[test]
    fn video_title_candidates_skip_empty_patches_after_current_facts_are_complete() {
        let properties = VideoStreamProperties {
            tmdb: Some(603),
            details: Some(VideoStreamDetailProperties {
                release_date: Some("1999-03-31".into()),
                ..VideoStreamDetailProperties::default()
            }),
            ..VideoStreamProperties::default()
        };

        assert!(video_fact_patch_from_title_candidates(&properties, ["The Matrix 1999"]).is_none());
    }

    #[test]
    fn applies_video_patch_to_missing_facts() {
        let mut properties = VideoStreamProperties::default();
        let patch = MediaFactPatch { tmdb_id: Some(603), release_date: Some("1999-01-01".to_string()) };

        assert!(apply_fact_patch_to_video(&mut properties, &patch));
        assert_eq!(properties.tmdb, Some(603));
        assert_eq!(properties.details.as_ref().and_then(|details| details.release_date.as_deref()), Some("1999-01-01"));
    }

    #[test]
    fn applies_series_patch_without_overwriting_existing_facts() {
        let mut properties = SeriesStreamProperties {
            tmdb: Some(1396),
            release_date: Some("2008-01-20".into()),
            ..SeriesStreamProperties::default()
        };
        let patch = MediaFactPatch { tmdb_id: Some(999), release_date: Some("2009-01-01".to_string()) };

        assert!(!apply_fact_patch_to_series(&mut properties, &patch));
        assert_eq!(properties.tmdb, Some(1396));
        assert_eq!(properties.release_date.as_deref(), Some("2008-01-20"));
    }

    #[test]
    fn builds_series_patch_from_parseable_title() {
        let properties = SeriesStreamProperties::default();
        let Some((year, patch)) = series_fact_patch_from_title(&properties, "Breaking Bad 2008") else {
            panic!("expected title year patch");
        };

        assert_eq!(year, 2008);
        assert_eq!(patch.release_date.as_deref(), Some("2008-01-01"));
    }

    #[test]
    fn builds_series_patch_from_first_parseable_title_candidate() {
        let properties = SeriesStreamProperties { tmdb: Some(1396), ..SeriesStreamProperties::default() };

        let Some((title, year, patch)) =
            series_fact_patch_from_title_candidates(&properties, ["No year", "Breaking Bad 2008"])
        else {
            panic!("expected fallback title year patch");
        };

        assert_eq!(title, "Breaking Bad 2008");
        assert_eq!(year, 2008);
        assert_eq!(patch.release_date.as_deref(), Some("2008-01-01"));
        assert!(patch.tmdb_id.is_none());
    }

    #[test]
    fn series_title_candidates_skip_empty_patches_after_current_facts_are_complete() {
        let properties = SeriesStreamProperties {
            tmdb: Some(1396),
            release_date: Some("2008-01-20".into()),
            ..SeriesStreamProperties::default()
        };

        assert!(series_fact_patch_from_title_candidates(&properties, ["Breaking Bad 2008"]).is_none());
    }

    #[test]
    fn applies_series_patch_to_missing_facts() {
        let mut properties = SeriesStreamProperties::default();
        let patch = MediaFactPatch { tmdb_id: Some(1396), release_date: Some("2008-01-01".to_string()) };

        assert!(apply_fact_patch_to_series(&mut properties, &patch));
        assert_eq!(properties.tmdb, Some(1396));
        assert_eq!(properties.release_date.as_deref(), Some("2008-01-01"));
    }
}
