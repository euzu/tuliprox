use crate::{
    library::MediaMetadata,
    media_enrichment::facts::{
        build_missing_fact_patch, MediaFactPatch, MediaItemFacts, MediaItemKind, SuppliedMediaFacts,
    },
};
use shared::model::{SeriesStreamProperties, VideoStreamDetailProperties, VideoStreamProperties};
use std::sync::Arc;

pub fn video_fact_patch_from_metadata(
    properties: &VideoStreamProperties,
    metadata: &MediaMetadata,
) -> MediaFactPatch {
    build_missing_fact_patch(&video_current_facts(properties), &SuppliedMediaFacts::from_metadata(metadata))
}

pub fn video_fact_patch_from_year(properties: &VideoStreamProperties, year: u32) -> MediaFactPatch {
    build_missing_fact_patch(
        &video_current_facts(properties),
        &SuppliedMediaFacts::from_release_year(MediaItemKind::Movie, year),
    )
}

pub fn series_fact_patch_from_metadata(
    properties: &SeriesStreamProperties,
    metadata: &MediaMetadata,
) -> MediaFactPatch {
    build_missing_fact_patch(&series_current_facts(properties), &SuppliedMediaFacts::from_metadata(metadata))
}

pub fn series_fact_patch_from_year(properties: &SeriesStreamProperties, year: u32) -> MediaFactPatch {
    build_missing_fact_patch(
        &series_current_facts(properties),
        &SuppliedMediaFacts::from_release_year(MediaItemKind::Series, year),
    )
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
        if properties.details.is_none() {
            properties.details = Some(VideoStreamDetailProperties::default());
        }
        if let Some(details) = properties.details.as_mut() {
            if details.release_date.is_none() {
                details.release_date = Some(Arc::<str>::from(release_date));
                changed = true;
            }
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
        properties.details.as_ref().and_then(|details| details.release_date.as_ref()).map(ToString::to_string),
    )
}

fn series_current_facts(properties: &SeriesStreamProperties) -> MediaItemFacts {
    MediaItemFacts::series(properties.tmdb, properties.release_date.as_ref().map(ToString::to_string))
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
        assert_eq!(
            properties.details.as_ref().and_then(|details| details.release_date.as_deref()),
            Some("1999-03-31")
        );
    }

    #[test]
    fn applies_video_patch_to_missing_facts() {
        let mut properties = VideoStreamProperties::default();
        let patch = MediaFactPatch { tmdb_id: Some(603), release_date: Some("1999-01-01".to_string()) };

        assert!(apply_fact_patch_to_video(&mut properties, &patch));
        assert_eq!(properties.tmdb, Some(603));
        assert_eq!(
            properties.details.as_ref().and_then(|details| details.release_date.as_deref()),
            Some("1999-01-01")
        );
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
    fn applies_series_patch_to_missing_facts() {
        let mut properties = SeriesStreamProperties::default();
        let patch = MediaFactPatch { tmdb_id: Some(1396), release_date: Some("2008-01-01".to_string()) };

        assert!(apply_fact_patch_to_series(&mut properties, &patch));
        assert_eq!(properties.tmdb, Some(1396));
        assert_eq!(properties.release_date.as_deref(), Some("2008-01-01"));
    }
}
