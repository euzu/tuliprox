use crate::{
    library::MediaMetadata,
    media_enrichment::facts::{MediaItemKind, SuppliedMediaFacts},
};
use shared::utils::parse_imdb_id;

pub fn supplied_facts_from_metadata(metadata: &MediaMetadata) -> SuppliedMediaFacts {
    let kind = if metadata.is_movie() { MediaItemKind::Movie } else { MediaItemKind::Series };
    let imdb_id = metadata.imdb_id().and_then(parse_imdb_id);
    SuppliedMediaFacts::new(kind, metadata.tmdb_id(), imdb_id, None, metadata.year())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{MediaMetadata, MovieMetadata, SeriesMetadata};

    #[test]
    fn maps_movie_metadata_to_supplied_facts() {
        let metadata =
            MediaMetadata::Movie(MovieMetadata { tmdb_id: Some(603), year: Some(1999), ..MovieMetadata::default() });

        let supplied = supplied_facts_from_metadata(&metadata);

        assert_eq!(supplied.kind, MediaItemKind::Movie);
        assert_eq!(supplied.tmdb_id, Some(603));
        assert_eq!(supplied.release_year, Some(1999));
    }

    #[test]
    fn maps_metadata_imdb_ids_to_supplied_facts() {
        let movie = MediaMetadata::Movie(MovieMetadata {
            tmdb_id: Some(603),
            imdb_id: Some("tt0133093".to_string()),
            ..MovieMetadata::default()
        });

        assert_eq!(supplied_facts_from_metadata(&movie).imdb_id, Some(133_093));

        let series = MediaMetadata::Series(SeriesMetadata {
            tmdb_id: Some(1396),
            imdb_id: Some("tt0903747".to_string()),
            ..SeriesMetadata::default()
        });

        assert_eq!(supplied_facts_from_metadata(&series).imdb_id, Some(903_747));

        let untagged =
            MediaMetadata::Movie(MovieMetadata { tmdb_id: Some(603), imdb_id: None, ..MovieMetadata::default() });
        assert_eq!(supplied_facts_from_metadata(&untagged).imdb_id, None);

        let malformed = MediaMetadata::Series(SeriesMetadata {
            imdb_id: Some("not-an-id".to_string()),
            ..SeriesMetadata::default()
        });
        assert_eq!(supplied_facts_from_metadata(&malformed).imdb_id, None);
    }

    #[test]
    fn maps_series_metadata_to_supplied_facts() {
        let metadata = MediaMetadata::Series(SeriesMetadata {
            tmdb_id: Some(1396),
            year: Some(2008),
            ..SeriesMetadata::default()
        });

        let supplied = supplied_facts_from_metadata(&metadata);

        assert_eq!(supplied.kind, MediaItemKind::Series);
        assert_eq!(supplied.tmdb_id, Some(1396));
        assert_eq!(supplied.release_year, Some(2008));
    }
}
