use fancy_regex::Regex as FancyRegex;
use regex::Regex;
use std::sync::LazyLock;

pub const BRACKETS: &[(&str, &str)] = &[("{", "}"), ("[", "]"), ("(", ")")];

pub const NON_ENGLISH_RANGES: &str = "\u{3040}-\u{30ff}\u{3400}-\u{4dbf}\u{4e00}-\u{9fff}\u{f900}-\u{faff}\u{ff66}-\u{ff9f}\u{0400}-\u{04ff}\u{0600}-\u{06ff}\u{0750}-\u{077f}\u{0c80}-\u{0cff}\u{0d00}-\u{0d7f}\u{0e00}-\u{0e7f}";

pub struct PttConstants {
    pub movie_regex: Regex,
    pub russian_cast_regex_fancy: FancyRegex,
    pub alt_titles_regex: Regex,
    pub before_title_match_regex: Regex,
    pub not_only_non_english_regex: FancyRegex,
    pub not_allowed_symbols_at_start_and_end: FancyRegex,
    pub remaining_not_allowed_symbols_at_start_and_end: Regex,
    pub redundant_symbols_at_end: Regex,
    pub empty_brackets_regex: Regex,
    pub parantheses_without_content: Regex,
    pub star_regex_1: Regex,
    pub star_regex_2: Regex,
    pub mp3_regex: Regex,
    pub spacing_regex: Regex,
    pub special_char_spacing: Regex,
    pub sub_pattern: Regex,
    pub integer: Regex,
    pub word: Regex,
    pub dot: Regex,
    pub months: Vec<(Regex, &'static str)>,
}

pub static PTT_CONSTANTS: LazyLock<PttConstants> = LazyLock::new(|| {
    PttConstants {
        movie_regex: Regex::new(r"(?i)[\[(]movie[)\]]").unwrap(),
        russian_cast_regex_fancy: FancyRegex::new(
            r"(?i)\([^)]*[\u0400-\u04ff][^)]*\)$|(?<=\/.*)\(.*\)$",
        )
            .unwrap(),
        alt_titles_regex: Regex::new(&format!(
            r"(?i)[^/|(]*[{NON_ENGLISH_RANGES}][^/|]*[/|]|[/|][^/|(]*[{NON_ENGLISH_RANGES}][^/|]*"
        ))
            .unwrap(),
        before_title_match_regex: Regex::new(r"^(?:\[([^\]]+)\])").unwrap(),
        not_only_non_english_regex: FancyRegex::new(&format!(
            r"(?i)([a-zA-Z][^{NON_ENGLISH_RANGES}]+)([{NON_ENGLISH_RANGES}].*[{NON_ENGLISH_RANGES}])|[{NON_ENGLISH_RANGES}].*[{NON_ENGLISH_RANGES}](?=[^{NON_ENGLISH_RANGES}]+[a-zA-Z])"
        )).unwrap(),
        not_allowed_symbols_at_start_and_end: FancyRegex::new(&format!(
            r"(?i)^[^\w{NON_ENGLISH_RANGES}#\x5B【★]+|[ \-:/\x5C\x5B|{{(#$&^]+$"
        ))
            .unwrap(),
        remaining_not_allowed_symbols_at_start_and_end: Regex::new(
            &format!(r"^[^\w{NON_ENGLISH_RANGES}#]+|]$")
        )
            .unwrap(),
        redundant_symbols_at_end: Regex::new(r"[ \-:./\\]+$").unwrap(),
        empty_brackets_regex: Regex::new(r"\(\s*\)|\[\s*\]|\{\s*\}").unwrap(),
        parantheses_without_content: Regex::new(r"\(\W*\)|\[\W*\]|\{\W*\}").unwrap(),
        star_regex_1: Regex::new(r"^[\[【★].*[\]】★][ .]?(.+)").unwrap(),
        star_regex_2: Regex::new(r"(.+)[ .]?[\[【★].*[\]】★]$").unwrap(),
        mp3_regex: Regex::new(r"(?i)\bmp3$").unwrap(),
        spacing_regex: Regex::new(r"\s+").unwrap(),
        special_char_spacing: Regex::new(r"[\-\+\_\{\}\[\]]\W{2,}").unwrap(),
        sub_pattern: Regex::new(r"_+").unwrap(),
        integer: Regex::new(r"\d+").unwrap(),
        word: Regex::new(r"\W+").unwrap(),
        dot: Regex::new(r"\.").unwrap(),
        months: vec! [
                (Regex::new(r"(?i)\bJanu\b").unwrap(), "Jan"),
                (Regex::new(r"(?i)\bFebr\b").unwrap(), "Feb"),
                (Regex::new(r"(?i)\bMarc\b").unwrap(), "Mar"),
                (Regex::new(r"(?i)\bApri\b").unwrap(), "Apr"),
                (Regex::new(r"(?i)\bMay\b").unwrap(), "May"),
                (Regex::new(r"(?i)\bJune\b").unwrap(), "Jun"),
                (Regex::new(r"(?i)\bJuly\b").unwrap(), "Jul"),
                (Regex::new(r"(?i)\bAugu\b").unwrap(), "Aug"),
                (Regex::new(r"(?i)\bSept\b").unwrap(), "Sep"),
                (Regex::new(r"(?i)\bOcto\b").unwrap(), "Oct"),
                (Regex::new(r"(?i)\bNove\b").unwrap(), "Nov"),
                (Regex::new(r"(?i)\bDece\b").unwrap(), "Dec"),
        ]
    }
});
