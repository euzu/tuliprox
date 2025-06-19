// Common utilities for Trakt functionality
pub mod client;
pub mod errors;

use deunicode::deunicode;
use crate::utils::CONSTANTS;

/// Normalize title for matching - optimized version with reduced allocations
pub fn normalize_title_for_matching(title: &str) -> String {
    let normalized = deunicode(title.trim());

    let mut result = String::with_capacity(normalized.len());

    for ch in normalized.chars() {
        if ch.is_alphanumeric() {
            result.push(ch.to_ascii_lowercase());
        }
    }

    if CONSTANTS.re_trakt_year.is_match(&result) {
        CONSTANTS.re_trakt_year.replace(&result, "").into_owned()
    } else {
        result
    }
}

/// Extract year from title using cached regex pattern - optimized version
pub fn extract_year_from_title(title: &str) -> Option<u32> {
    if let Some(captures) = CONSTANTS.re_trakt_year.captures(title) {
        if let Some(year_str) = captures.get(1) {
            if let Ok(year) = year_str.as_str().parse::<u32>() {
                if (1900..=2100).contains(&year) {
                    return Some(year);
                }
            }
        }
    }
    
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_title() {
        assert_eq!(normalize_title_for_matching("The Matrix"), "thematrix");
        assert_eq!(normalize_title_for_matching("Spider-Man: No Way Home"), "spidermannowayhome");
        assert_eq!(normalize_title_for_matching("Élite"), "elite");
    }

    #[test]
    fn test_extract_year() {
        let year = extract_year_from_title("The Matrix (1999)");
        assert_eq!(year, Some(1999));

        let year = extract_year_from_title("Avengers Endgame 2019");
        assert_eq!(year, Some(2019));

        let year = extract_year_from_title("Just a Title");
        assert_eq!(year, None);
    }
} 