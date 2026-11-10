use crate::model::REGEX_CACHE;
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq)]
pub enum SearchRequest {
    Clear,
    Text(String, Option<Rc<Vec<String>>>),
    Regexp(String, Option<Rc<Vec<String>>>),
}

impl SearchRequest {
    /// Test for a single name: case-insensitive substring for text, a regular expression otherwise.
    /// A cleared search matches everything, an invalid expression nothing.
    pub fn name_matcher(&self) -> Box<dyn Fn(&str) -> bool + '_> {
        match self {
            Self::Clear => Box::new(|_| true),
            Self::Text(text, _) => {
                let needle = text.trim().to_lowercase();
                Box::new(move |name| needle.is_empty() || name.to_lowercase().contains(&needle))
            }
            Self::Regexp(pattern, _) => {
                let regex = REGEX_CACHE.get_or_compile(pattern).ok();
                Box::new(move |name| regex.as_ref().is_some_and(|regex| regex.is_match(name)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SearchRequest;

    #[test]
    fn name_matcher_matches_text_regex_and_clear() {
        assert!(SearchRequest::Clear.name_matcher()("anything"));
        let text = SearchRequest::Text(" ZDF ".to_string(), None);
        assert!(text.name_matcher()("zdf hd"));
        assert!(!text.name_matcher()("arte"));
        assert!(SearchRequest::Text("  ".to_string(), None).name_matcher()("arte"));
        let regex = SearchRequest::Regexp("^(zdf|rtl)$".to_string(), None);
        assert!(regex.name_matcher()("rtl"));
        assert!(!regex.name_matcher()("rtl2"));
        // a regular expression is case sensitive
        assert!(!SearchRequest::Regexp("zdf".to_string(), None).name_matcher()("ZDF"));
        assert!(!SearchRequest::Regexp("(".to_string(), None).name_matcher()("zdf"));
    }
}
