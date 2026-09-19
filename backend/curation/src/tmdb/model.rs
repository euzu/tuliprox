use crate::kernel::{CuratedMediaReference, CurationMediaKind};
use serde::{Deserialize, Deserializer};
use shared::model::TmdbTrendingKind;
use std::{collections::HashSet, num::NonZeroU32};

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

pub(super) fn translate_page(body: &[u8], kind: TmdbTrendingKind) -> Result<Vec<CuratedMediaReference>, ()> {
    let page: TrendingPage = serde_json::from_slice(body).map_err(|_| ())?;
    if page.page != 1 || page.total_results < u64::try_from(page.results.len()).map_err(|_| ())? {
        return Err(());
    }
    if page.results.is_empty() {
        return if page.total_results == 0 && page.total_pages <= 1 { Ok(Vec::new()) } else { Err(()) };
    }
    if page.total_pages == 0 {
        return Err(());
    }
    let mut seen = HashSet::new();
    let mut references = Vec::with_capacity(page.results.len());
    for (index, item) in page.results.into_iter().enumerate() {
        // Validate duplicates too: malformed records cannot be silently discarded.
        if item.media_type.is_some_and(|media_type| media_type != kind) {
            return Err(());
        }
        if !seen.insert(item.id) {
            continue;
        }
        let (media_kind, title, date) = match kind {
            TmdbTrendingKind::Movie => (CurationMediaKind::Movie, item.title, item.release_date),
            TmdbTrendingKind::Tv => (CurationMediaKind::Series, item.name, item.first_air_date),
        };
        let year = date
            .as_deref()
            .and_then(|date| date.split('-').next())
            .and_then(|year| year.parse::<u32>().ok())
            .filter(|year| (1900..=2100).contains(year));
        references.push(CuratedMediaReference::new(
            media_kind,
            title.unwrap_or_default(),
            year,
            Some(item.id.get()),
            Some(u32::try_from(index + 1).map_err(|_| ())?),
        ));
    }
    Ok(references)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::CurationMediaKind;
    use serde_json::{json, Value};

    fn page(results: Value) -> Value {
        let mut page = json!({"page": 1, "total_pages": 100, "total_results": 1000, "results": null});
        page["results"] = results;
        page
    }

    fn translate(value: &Value, kind: TmdbTrendingKind) -> Result<Vec<CuratedMediaReference>, ()> {
        translate_page(&serde_json::to_vec(value).unwrap(), kind)
    }

    #[test]
    fn tmdb_declared_first_page_is_complete_even_when_more_pages_exist() {
        let result = translate(
            &page(json!([{"id": 7, "title": "Film", "release_date": "2024-01-02"}])),
            TmdbTrendingKind::Movie,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, CurationMediaKind::Movie);
        assert_eq!(result[0].tmdb_id, Some(7));
        assert_eq!(result[0].year, Some(2024));
        assert_eq!(result[0].rank, Some(1));
    }

    #[test]
    fn tmdb_tv_translation_and_absent_auxiliary_text_remain_exact_only() {
        let result = translate(
            &page(json!([{"id": 7, "name": "Show", "first_air_date": "unknown", "media_type": "tv"}, {"id": 8}])),
            TmdbTrendingKind::Tv,
        )
        .unwrap();
        assert_eq!(result[0].kind, CurationMediaKind::Series);
        assert_eq!(result[0].title, "Show");
        assert_eq!(result[0].year, None);
        assert_eq!(result[1].title, "");
    }

    #[test]
    fn tmdb_duplicate_ids_keep_first_occurrence_and_original_rank() {
        let result = translate(
            &page(json!([{"id": 7, "title": "First"}, {"id": 7, "title": "Duplicate"}, {"id": 8}])),
            TmdbTrendingKind::Movie,
        )
        .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].title, "First");
        assert_eq!(result[1].rank, Some(3));
    }

    #[test]
    fn tmdb_empty_requires_consistent_authoritative_totals() {
        for pages in [0, 1] {
            let value = json!({"page": 1, "total_pages": pages, "total_results": 0, "results": []});
            assert!(translate(&value, TmdbTrendingKind::Movie).unwrap().is_empty());
        }
        assert!(translate(&page(json!([])), TmdbTrendingKind::Movie).is_err());
    }

    #[test]
    fn tmdb_bad_record_cannot_shrink_a_successful_snapshot() {
        for bad in [
            json!({}),
            json!({"id": 0}),
            json!({"id": -1}),
            json!({"id": 4_294_967_296_u64}),
            json!({"id": "7"}),
            json!({"id": 7, "media_type": "tv"}),
            json!({"id": 7, "media_type": null}),
            json!({"id": 7, "title": 42}),
        ] {
            assert!(translate(&page(json!([{"id": 9}, bad])), TmdbTrendingKind::Movie).is_err());
        }
    }

    #[test]
    fn tmdb_invalid_page_envelopes_are_not_empty_successes() {
        for value in [
            json!({}),
            json!([]),
            json!({"page": 2, "total_pages": 2, "total_results": 1, "results": [{"id": 7}]}),
            json!({"page": 1, "total_pages": 0, "total_results": 1, "results": [{"id": 7}]}),
            json!({"page": 1, "total_pages": 1, "total_results": 0, "results": [{"id": 7}]}),
            json!({"page": 1, "total_pages": 1, "total_results": -1, "results": []}),
            json!({"page": 1, "total_pages": 1, "total_results": 0, "results": null}),
        ] {
            assert!(translate(&value, TmdbTrendingKind::Movie).is_err());
        }
        assert!(translate_page(br#"{"page":1,"results":["#, TmdbTrendingKind::Movie).is_err());
    }
}
