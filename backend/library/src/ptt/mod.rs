use crate::ptt::parser::PttParser;
use std::sync::LazyLock;

mod constants;
mod handlers;
mod models;
mod parser;
mod transformers;

pub use models::PttMetadata;

static PTT_PARSER: LazyLock<PttParser> = LazyLock::new(|| {
    let mut parser = PttParser::new();
    handlers::add_defaults(&mut parser);
    parser
});

pub fn ptt_parse_title(title: &str) -> PttMetadata { PTT_PARSER.parse(title, false) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ptt_parse_title_with_unicode_en_dash_does_not_panic() {
        // En dash '–' is 3 bytes (0xE2 0x80 0x93). When byte offsets are shifted
        // by preceding handler removals, indexing without char boundary checks previously panicked.
        let titles = [
            "Alpha – 2024 Vol 1",
            "Movie – 2023 – 1080p",
            "Series – S01E02 – 2021 – Title",
            "Hindi – 2020 Vol 2",
            "Avatar: The Way of Water – 2022 [1080p] [Multi]",
            "Title – 2021 Vol.03 720p",
            "Test — Em Dash — 2019 Vol 4",
            "日本語タイトル – 2023 Vol 1",
            "Русский сериал – 2022",
        ];

        for title in titles {
            let meta = ptt_parse_title(title);
            assert!(!meta.title.is_empty(), "Parsed title should not be empty for {title}");
        }
    }

    #[test]
    fn test_ptt_parse_volume_after_year_with_multibyte() {
        let meta = ptt_parse_title("Attack on Titan – 2023 Vol 1");
        assert_eq!(meta.year, Some(2023));
        assert_eq!(meta.volumes, vec![1]);
    }
}
