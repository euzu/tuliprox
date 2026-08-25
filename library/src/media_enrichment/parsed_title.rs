use crate::{
    media_enrichment::facts::{MediaItemKind, SuppliedMediaFacts},
    ptt::ptt_parse_title,
};

pub fn supplied_release_year_from_title(kind: MediaItemKind, title: &str) -> Option<(u32, SuppliedMediaFacts)> {
    let year = ptt_parse_title(title).year?;
    Some((year, SuppliedMediaFacts::from_release_year(kind, year)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supplies_release_year_from_parseable_title() {
        let Some((year, supplied)) = supplied_release_year_from_title(MediaItemKind::Movie, "The Matrix 1999") else {
            panic!("expected parsed year");
        };

        assert_eq!(year, 1999);
        assert_eq!(supplied.kind, MediaItemKind::Movie);
        assert_eq!(supplied.release_year, Some(1999));
    }

    #[test]
    fn returns_none_when_title_has_no_year() {
        assert!(supplied_release_year_from_title(MediaItemKind::Series, "Breaking Bad").is_none());
    }
}
