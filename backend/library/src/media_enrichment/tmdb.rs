use crate::{
    library::MediaMetadata,
    media_enrichment::facts::{MediaItemKind, SuppliedMediaFacts},
};

pub fn supplied_facts_from_metadata(metadata: &MediaMetadata) -> SuppliedMediaFacts {
    let kind = if metadata.is_movie() { MediaItemKind::Movie } else { MediaItemKind::Series };
    SuppliedMediaFacts::new(kind, metadata.tmdb_id(), None, metadata.year())
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
