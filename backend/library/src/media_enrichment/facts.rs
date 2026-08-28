use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaItemKind {
    Movie,
    Series,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaItemFacts {
    pub kind: MediaItemKind,
    pub tmdb_id: Option<u32>,
    pub release_date: Option<Arc<str>>,
}

impl MediaItemFacts {
    pub fn new(kind: MediaItemKind, tmdb_id: Option<u32>, release_date: Option<Arc<str>>) -> Self {
        Self { kind, tmdb_id: valid_tmdb_id(tmdb_id), release_date }
    }

    pub fn movie(tmdb_id: Option<u32>, release_date: Option<Arc<str>>) -> Self {
        Self::new(MediaItemKind::Movie, tmdb_id, release_date)
    }

    pub fn series(tmdb_id: Option<u32>, release_date: Option<Arc<str>>) -> Self {
        Self::new(MediaItemKind::Series, tmdb_id, release_date)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedMediaFacts {
    pub kind: MediaItemKind,
    pub tmdb_id: Option<u32>,
    pub release_date: Option<String>,
    pub release_year: Option<u32>,
}

impl SuppliedMediaFacts {
    pub fn new(
        kind: MediaItemKind,
        tmdb_id: Option<u32>,
        release_date: Option<String>,
        release_year: Option<u32>,
    ) -> Self {
        Self { kind, tmdb_id: valid_tmdb_id(tmdb_id), release_date, release_year: valid_year(release_year) }
    }

    pub fn from_release_year(kind: MediaItemKind, year: u32) -> Self {
        Self::new(kind, None, None, Some(year))
    }

    fn resolved_release_date(&self) -> Option<String> {
        self.release_date.clone().or_else(|| self.release_year.map(synthetic_release_date_from_year))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaFactPatch {
    pub tmdb_id: Option<u32>,
    pub release_date: Option<String>,
}

impl MediaFactPatch {
    pub fn is_empty(&self) -> bool {
        self.tmdb_id.is_none() && self.release_date.is_none()
    }
}

pub fn build_missing_fact_patch(current: &MediaItemFacts, supplied: &SuppliedMediaFacts) -> MediaFactPatch {
    if current.kind != supplied.kind {
        return MediaFactPatch::default();
    }

    MediaFactPatch {
        tmdb_id: current.tmdb_id.is_none().then_some(supplied.tmdb_id).flatten(),
        release_date: current.release_date.is_none().then(|| supplied.resolved_release_date()).flatten(),
    }
}

pub fn synthetic_release_date_from_year(year: u32) -> String {
    format!("{year:04}-01-01")
}

fn valid_tmdb_id(tmdb_id: Option<u32>) -> Option<u32> {
    tmdb_id.filter(|id| *id > 0)
}

fn valid_year(year: Option<u32>) -> Option<u32> {
    year.filter(|year| (1..=9999).contains(year))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fact_patch_fills_tmdb_and_synthetic_release_date() {
        let current = MediaItemFacts::movie(None, None);
        let supplied = SuppliedMediaFacts::new(MediaItemKind::Movie, Some(603), None, Some(1999));

        let patch = build_missing_fact_patch(&current, &supplied);

        assert_eq!(patch.tmdb_id, Some(603));
        assert_eq!(patch.release_date.as_deref(), Some("1999-01-01"));
    }

    #[test]
    fn missing_fact_patch_does_not_overwrite_current_facts() {
        let current = MediaItemFacts::series(Some(1396), Some(Arc::<str>::from("2008-01-20")));
        let supplied = SuppliedMediaFacts::new(MediaItemKind::Series, Some(999), None, Some(2009));

        let patch = build_missing_fact_patch(&current, &supplied);

        assert!(patch.is_empty());
    }

    #[test]
    fn missing_fact_patch_ignores_wrong_media_kind() {
        let current = MediaItemFacts::movie(None, None);
        let supplied = SuppliedMediaFacts::new(MediaItemKind::Series, Some(1396), None, Some(2008));

        let patch = build_missing_fact_patch(&current, &supplied);

        assert!(patch.is_empty());
    }

    #[test]
    fn supplied_explicit_release_date_wins_over_year() {
        let current = MediaItemFacts::series(None, None);
        let supplied = SuppliedMediaFacts::new(MediaItemKind::Series, None, Some("2008-01-20".to_string()), Some(2009));

        let patch = build_missing_fact_patch(&current, &supplied);

        assert_eq!(patch.release_date.as_deref(), Some("2008-01-20"));
    }

    #[test]
    fn zero_ids_and_years_are_not_trusted_facts() {
        let current = MediaItemFacts::movie(None, None);
        let supplied = SuppliedMediaFacts::new(MediaItemKind::Movie, Some(0), None, Some(0));

        let patch = build_missing_fact_patch(&current, &supplied);

        assert!(patch.is_empty());
    }

    #[test]
    fn synthetic_release_date_is_zero_padded() {
        assert_eq!(synthetic_release_date_from_year(99), "0099-01-01");
    }

    #[test]
    fn years_over_9999_are_not_trusted_facts() {
        let current = MediaItemFacts::movie(None, None);
        let supplied = SuppliedMediaFacts::new(MediaItemKind::Movie, None, None, Some(10000));

        let patch = build_missing_fact_patch(&current, &supplied);

        assert!(patch.is_empty());
    }
}
