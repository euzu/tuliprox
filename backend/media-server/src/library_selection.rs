use crate::{non_blank_arc, MediaServerError, MediaServerErrorKind, MediaServerLibrary, MediaServerLibraryRef};
use shared::model::{MediaServerLibraryKind, MediaServerLibrarySelector};
use std::sync::Arc;

/// A library as exposed by a media-server wire DTO, reduced to what selector
/// matching and `MediaServerLibrary` construction need.
pub(crate) trait MediaServerLibraryCandidate {
    fn library_key(&self) -> Option<&str>;
    fn library_title(&self) -> Option<&str>;
    fn library_kind(&self) -> MediaServerLibraryKind;
}

pub(crate) fn library_matches_selector<C: MediaServerLibraryCandidate>(
    candidate: &C,
    selector: &MediaServerLibrarySelector,
) -> bool {
    match selector {
        MediaServerLibrarySelector::Name(name) => matches_trimmed(candidate.library_title(), name),
        MediaServerLibrarySelector::Detailed(details) => {
            let identity_matches =
                details.key.as_deref().is_some_and(|key| matches_trimmed(candidate.library_key(), key))
                    || details.id.as_deref().is_some_and(|id| matches_trimmed(candidate.library_key(), id))
                    || details.name.as_deref().is_some_and(|name| matches_trimmed(candidate.library_title(), name));
            identity_matches
                && details.kind.is_none_or(|expected| {
                    expected != MediaServerLibraryKind::Unsupported && candidate.library_kind() == expected
                })
        }
    }
}

/// Resolves every selector to exactly one visible library, in selector order.
pub(crate) fn select_libraries<C: MediaServerLibraryCandidate>(
    provider: &'static str,
    input_name: &Arc<str>,
    server_id: &Arc<str>,
    selectors: &[MediaServerLibrarySelector],
    candidates: &[C],
) -> Result<Vec<MediaServerLibrary>, MediaServerError> {
    let unavailable = |detail: &'static str| {
        MediaServerError::new(MediaServerErrorKind::MediaServerLibraryUnavailable).provider(provider).detail(detail)
    };
    let mut libraries = Vec::with_capacity(selectors.len());
    for selector in selectors {
        let mut matches = candidates.iter().filter(|candidate| library_matches_selector(*candidate, selector));
        let candidate = match (matches.next(), matches.next()) {
            (None, _) => return Err(unavailable("selected library was not visible")),
            (Some(candidate), None) => candidate,
            (Some(_), Some(_)) => return Err(unavailable("selected library name was ambiguous")),
        };
        let library_id = non_blank_arc(candidate.library_key())
            .ok_or_else(|| unavailable("selected library did not expose a stable key"))?;
        let name = non_blank_arc(candidate.library_title()).unwrap_or_else(|| Arc::clone(&library_id));
        libraries.push(MediaServerLibrary {
            reference: MediaServerLibraryRef {
                input_name: Arc::clone(input_name),
                server_id: Arc::clone(server_id),
                library_id,
            },
            name,
            kind: candidate.library_kind(),
        });
    }
    Ok(libraries)
}

fn matches_trimmed(candidate: Option<&str>, expected: &str) -> bool {
    candidate.is_some_and(|candidate| candidate.trim() == expected.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::MediaServerLibrarySelectorDetailsDto;

    struct TestCandidate {
        key: Option<&'static str>,
        title: Option<&'static str>,
        kind: MediaServerLibraryKind,
    }

    impl MediaServerLibraryCandidate for TestCandidate {
        fn library_key(&self) -> Option<&str> { self.key }

        fn library_title(&self) -> Option<&str> { self.title }

        fn library_kind(&self) -> MediaServerLibraryKind { self.kind }
    }

    fn candidate(key: &'static str, title: &'static str, kind: MediaServerLibraryKind) -> TestCandidate {
        TestCandidate { key: Some(key), title: Some(title), kind }
    }

    fn details(
        id: Option<&str>,
        key: Option<&str>,
        name: Option<&str>,
        kind: Option<MediaServerLibraryKind>,
    ) -> MediaServerLibrarySelector {
        MediaServerLibrarySelector::Detailed(MediaServerLibrarySelectorDetailsDto {
            id: id.map(str::to_string),
            key: key.map(str::to_string),
            name: name.map(str::to_string),
            kind,
        })
    }

    fn select(
        selectors: &[MediaServerLibrarySelector],
        candidates: &[TestCandidate],
    ) -> Result<Vec<MediaServerLibrary>, MediaServerError> {
        select_libraries("test", &Arc::<str>::from("input"), &Arc::<str>::from("server"), selectors, candidates)
    }

    #[test]
    fn name_selector_matches_trimmed_title() {
        let candidate =
            TestCandidate { key: Some("1"), title: Some("  Movies "), kind: MediaServerLibraryKind::Movies };

        assert!(library_matches_selector(&candidate, &MediaServerLibrarySelector::Name(" Movies  ".to_string())));
        assert!(!library_matches_selector(&candidate, &MediaServerLibrarySelector::Name("Shows".to_string())));
        assert!(!library_matches_selector(
            &TestCandidate { key: Some("1"), title: None, kind: MediaServerLibraryKind::Movies },
            &MediaServerLibrarySelector::Name("Movies".to_string())
        ));
    }

    #[test]
    fn key_and_id_selectors_match_library_key() {
        let candidate = candidate("42", "Movies", MediaServerLibraryKind::Movies);

        assert!(library_matches_selector(&candidate, &details(None, Some(" 42 "), None, None)));
        assert!(library_matches_selector(&candidate, &details(Some("42"), None, None, None)));
        assert!(library_matches_selector(&candidate, &details(None, None, Some("Movies"), None)));
        assert!(!library_matches_selector(&candidate, &details(None, Some("43"), None, None)));
        assert!(!library_matches_selector(&candidate, &details(None, None, None, None)));
    }

    #[test]
    fn kind_filter_rejects_mismatch_and_unsupported() {
        let movies = candidate("1", "Movies", MediaServerLibraryKind::Movies);
        let mixed = candidate("2", "Mixed", MediaServerLibraryKind::Unsupported);

        assert!(library_matches_selector(
            &movies,
            &details(None, Some("1"), None, Some(MediaServerLibraryKind::Movies))
        ));
        assert!(!library_matches_selector(
            &movies,
            &details(None, Some("1"), None, Some(MediaServerLibraryKind::TvShows))
        ));
        assert!(!library_matches_selector(
            &mixed,
            &details(None, Some("2"), None, Some(MediaServerLibraryKind::Unsupported))
        ));
        assert!(library_matches_selector(&mixed, &details(None, Some("2"), None, None)));
    }

    #[test]
    fn duplicate_title_is_ambiguous() {
        let candidates = [
            candidate("1", "Movies", MediaServerLibraryKind::Movies),
            candidate("2", "Movies", MediaServerLibraryKind::Movies),
        ];

        let error = select(&[MediaServerLibrarySelector::Name("Movies".to_string())], &candidates)
            .expect_err("duplicate title is ambiguous");
        assert_eq!(error.kind, MediaServerErrorKind::MediaServerLibraryUnavailable);
        assert_eq!(error.provider, Some("test"));
        assert_eq!(error.detail_text(), Some("selected library name was ambiguous"));

        let libraries = select(&[details(None, Some("2"), None, None)], &candidates).expect("key disambiguates");
        assert_eq!(libraries[0].reference.library_id.as_ref(), "2");
    }

    #[test]
    fn missing_library_is_unavailable() {
        let candidates = [candidate("1", "Movies", MediaServerLibraryKind::Movies)];

        let error = select(&[MediaServerLibrarySelector::Name("Shows".to_string())], &candidates)
            .expect_err("library is not visible");
        assert_eq!(error.kind, MediaServerErrorKind::MediaServerLibraryUnavailable);
        assert_eq!(error.detail_text(), Some("selected library was not visible"));

        let blank_key =
            [TestCandidate { key: Some("  "), title: Some("Movies"), kind: MediaServerLibraryKind::Movies }];
        let error = select(&[MediaServerLibrarySelector::Name("Movies".to_string())], &blank_key)
            .expect_err("blank key is rejected");
        assert_eq!(error.kind, MediaServerErrorKind::MediaServerLibraryUnavailable);
        assert_eq!(error.detail_text(), Some("selected library did not expose a stable key"));
    }

    #[test]
    fn output_follows_selector_order() {
        let candidates = [
            candidate("1", "Movies", MediaServerLibraryKind::Movies),
            candidate("2", "Shows", MediaServerLibraryKind::TvShows),
        ];

        let libraries = select(
            &[
                MediaServerLibrarySelector::Name("Shows".to_string()),
                MediaServerLibrarySelector::Name("Movies".to_string()),
            ],
            &candidates,
        )
        .expect("libraries selected");

        assert_eq!(libraries.iter().map(|library| library.name.as_ref()).collect::<Vec<_>>(), ["Shows", "Movies"]);
        assert_eq!(libraries[0].kind, MediaServerLibraryKind::TvShows);
        assert_eq!(libraries[0].reference.input_name.as_ref(), "input");
        assert_eq!(libraries[0].reference.server_id.as_ref(), "server");
    }
}
