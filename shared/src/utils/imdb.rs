use super::CONSTANTS;

/// Parses an `IMDb` id into its numeric part.
///
/// Accepts the bare number and the `tt` form (`tt0133093`, `TT0133093`).
/// Returns `None` for anything else, for an empty or zero id, and for a value
/// that does not fit into a `u32`.
pub fn parse_imdb_id(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    let digits = trimmed.strip_prefix("tt").or_else(|| trimmed.strip_prefix("TT")).unwrap_or(trimmed);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id = digits.parse::<u32>().ok()?;
    (id > 0).then_some(id)
}

/// Extracts the first `IMDb` id from a free-text title.
///
/// Recognizes the forms providers use in stream names: `imdbid-tt6549510`,
/// `[tt6549510]`, `imdb=tt6549510` and `{imdb-tt6549510}`. A title with
/// several ids yields the leftmost one.
pub fn parse_imdb_id_from_title(title: &str) -> Option<u32> {
    let captures = CONSTANTS.re_classifier_imdb_id.captures(title)?;
    parse_imdb_id(captures.get(1)?.as_str())
}

#[cfg(test)]
mod tests {
    use super::{parse_imdb_id, parse_imdb_id_from_title};

    #[test]
    fn parses_tt_and_bare_ids() {
        assert_eq!(parse_imdb_id("tt0133093"), Some(133093));
        assert_eq!(parse_imdb_id("TT0133093"), Some(133093));
        assert_eq!(parse_imdb_id("0133093"), Some(133093));
        assert_eq!(parse_imdb_id("  tt0000001  "), Some(1));
        assert_eq!(parse_imdb_id("tt9999999"), Some(9_999_999));
    }

    #[test]
    fn rejects_invalid_ids() {
        assert_eq!(parse_imdb_id(""), None);
        assert_eq!(parse_imdb_id("tt"), None);
        assert_eq!(parse_imdb_id("tt0000000"), None);
        assert_eq!(parse_imdb_id("0"), None);
        assert_eq!(parse_imdb_id("tt12a45"), None);
        assert_eq!(parse_imdb_id("-12345"), None);
        assert_eq!(parse_imdb_id("tmdb12345"), None);
    }

    #[test]
    fn rejects_overflowing_ids() {
        assert_eq!(parse_imdb_id("tt99999999999"), None);
        assert_eq!(parse_imdb_id_from_title("[tt99999999999]"), None);
    }

    #[test]
    fn extracts_imdb_id_from_title_tags() {
        assert_eq!(parse_imdb_id_from_title("Kill Em All 2 (2024) imdbid-tt6549510"), Some(6_549_510));
        assert_eq!(parse_imdb_id_from_title("Kill Em All 2 [tt6549510]"), Some(6_549_510));
        assert_eq!(parse_imdb_id_from_title("Kill Em All 2 imdb=tt6549510"), Some(6_549_510));
        assert_eq!(parse_imdb_id_from_title("{imdb-tt6549510} Kill Em All 2"), Some(6_549_510));
        assert_eq!(parse_imdb_id_from_title("tt6549510"), Some(6_549_510));
    }

    #[test]
    fn rejects_titles_whose_tt_id_is_too_short() {
        assert_eq!(parse_imdb_id_from_title("Movie [tt654951]"), None);
        assert_eq!(parse_imdb_id_from_title("Movie tt123456"), None);
        assert_eq!(parse_imdb_id_from_title("The Matrix 1999"), None);
        assert_eq!(parse_imdb_id_from_title("tmdb603 The Matrix"), None);
        assert_eq!(parse_imdb_id_from_title(""), None);
    }

    #[test]
    fn takes_the_first_of_several_ids() {
        assert_eq!(parse_imdb_id_from_title("Movie [tt1111111] [tt2222222]"), Some(1_111_111));
        assert_eq!(parse_imdb_id_from_title("Movie imdb=tt3333333 [tt2222222]"), Some(3_333_333));
    }

    #[test]
    fn does_not_match_an_embedded_tt_id() {
        assert_eq!(parse_imdb_id_from_title("xtt1234567"), None);
    }
}
