use crate::kernel::{CuratedMediaReference, CurationMediaKind};
use serde::{Deserialize, Deserializer};
use shared::model::TmdbTrendingKind;
use std::num::NonZeroU32;

#[derive(Deserialize)]
struct TrendingPage {
    page: u64,
    total_pages: u64,
    total_results: u64,
    results: Vec<TrendingItem>,
}

#[derive(Deserialize)]
struct TrendingItem {
    id: NonZeroU32,
    #[serde(default, deserialize_with = "present_kind")]
    media_type: Option<TmdbTrendingKind>,
    title: Option<String>,
    name: Option<String>,
    release_date: Option<String>,
    first_air_date: Option<String>,
}

fn present_kind<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<TmdbTrendingKind>, D::Error> {
    TmdbTrendingKind::deserialize(deserializer).map(Some)
}

pub(super) struct ValidatedPage {
    /// All rows, including duplicates and the suffix beyond the selection limit.
    pub rows: Vec<CuratedMediaReference>,
    pub last: bool,
}

pub(super) fn translate_page(
    body: &[u8],
    kind: TmdbTrendingKind,
    requested_page: u64,
    preceding_rows: u32,
) -> Result<ValidatedPage, ()> {
    let page: TrendingPage = serde_json::from_slice(body).map_err(|_| ())?;
    if requested_page == 0
        || page.page != requested_page
        || page.total_results < u64::try_from(page.results.len()).map_err(|_| ())?
    {
        return Err(());
    }
    if page.results.is_empty() {
        return if requested_page == 1 && page.total_results == 0 && page.total_pages <= 1 {
            Ok(ValidatedPage { rows: Vec::new(), last: true })
        } else {
            Err(())
        };
    }
    if page.total_pages < requested_page {
        return Err(());
    }
    let mut rows = Vec::with_capacity(page.results.len());
    for (index, item) in page.results.into_iter().enumerate() {
        // Validate before deduplication or selection: even an unselected suffix is authoritative wire data.
        if item.media_type.is_some_and(|media_type| media_type != kind) {
            return Err(());
        }
        let rank = preceding_rows
            .checked_add(u32::try_from(index).map_err(|_| ())?)
            .and_then(|rank| rank.checked_add(1))
            .ok_or(())?;
        let (media_kind, title, date) = match kind {
            TmdbTrendingKind::Movie => (CurationMediaKind::Movie, item.title, item.release_date),
            TmdbTrendingKind::Tv => (CurationMediaKind::Series, item.name, item.first_air_date),
        };
        let year = date
            .as_deref()
            .and_then(|date| date.split('-').next())
            .and_then(|year| year.parse::<u32>().ok())
            .filter(|year| (1900..=2100).contains(year));
        rows.push(CuratedMediaReference::new(
            media_kind,
            title.unwrap_or_default(),
            year,
            Some(item.id.get()),
            Some(rank),
        ));
    }
    Ok(ValidatedPage { rows, last: page.page == page.total_pages })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn translate(results: Value, kind: TmdbTrendingKind, preceding: u32) -> Result<ValidatedPage, ()> {
        translate_page(
            &serde_json::to_vec(&json!({"page":2,"total_pages":3,"total_results":100,"results":results})).unwrap(),
            kind,
            2,
            preceding,
        )
    }

    #[test]
    fn tmdb_wire_translation_preserves_every_row_and_cumulative_rank() {
        let page = translate(
            json!([{"id":7,"title":"First","release_date":"2024-01-02"},{"id":7,"title":"Duplicate"},{"id":u32::MAX}]),
            TmdbTrendingKind::Movie,
            3,
        )
        .unwrap();
        assert!(!page.last);
        assert_eq!(page.rows.len(), 3);
        assert_eq!(page.rows[0].title, "First");
        assert_eq!(page.rows[0].year, Some(2024));
        assert_eq!(page.rows[2].rank, Some(6));
        assert_eq!(page.rows[2].tmdb_id, Some(u32::MAX));
        let page = translate(
            json!([{"id":7,"media_type":"tv","name":"Show","first_air_date":"unknown"},{"id":8,"name":null}]),
            TmdbTrendingKind::Tv,
            0,
        )
        .unwrap();
        assert_eq!(page.rows[0].kind, CurationMediaKind::Series);
        assert_eq!(page.rows[0].title, "Show");
        assert_eq!(page.rows[0].year, None);
        assert_eq!(page.rows[1].title, "");
        assert!(translate(json!([{"id":7}]), TmdbTrendingKind::Movie, u32::MAX).is_err());
    }

    #[test]
    fn tmdb_wire_invalid_duplicate_or_suffix_is_not_discarded() {
        for bad in [
            json!({}),
            json!({"id":0}),
            json!({"id":-1}),
            json!({"id":4294967296_u64}),
            json!({"id":"7"}),
            json!({"id":7.0}),
            json!({"id":7,"media_type":"tv"}),
            json!({"id":7,"media_type":null}),
            json!({"id":7,"title":42}),
            json!({"id":7,"release_date":false}),
        ] {
            assert!(translate(json!([{"id":7},bad]), TmdbTrendingKind::Movie, 0).is_err());
        }
    }
}
