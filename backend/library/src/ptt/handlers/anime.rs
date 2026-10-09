use crate::ptt::parser::{MatchInfo, ParseContext, PttParser};
use fancy_regex::Regex as FancyRegex;

pub(super) fn register(parser: &mut PttParser) {
    let anime_regex = FancyRegex::new(r"(?i)One.*?Piece|Bleach|Naruto").unwrap();

    let volume_regex = FancyRegex::new(r"(?i)\bvol(?:ume)?[. -]*(\d{1,2})\b").unwrap();

    parser.add_handler_fn(
        "volumes",
        Box::new(move |context: &mut ParseContext| -> Option<MatchInfo> {
            let title = &context.title;
            let matched = &context.matched;

            let start_index = matched.get("year").map_or(0, |m| {
                let mut idx = m.match_index.min(title.len());
                while idx > 0 && !title.is_char_boundary(idx) {
                    idx -= 1;
                }
                idx
            });

            if start_index >= title.len() {
                return None;
            }

            let search_slice = &title[start_index..];

            if let Ok(Some(m)) = volume_regex.find(search_slice) {
                let raw_match = m.as_str().to_string();
                let relative_start = m.start();

                if let Ok(Some(cap)) = volume_regex.captures(search_slice) {
                    let volume_number = cap.get(1).map_or(0, |m| m.as_str().parse::<i32>().unwrap_or(0));

                    context.result.volumes = vec![volume_number];
                }

                let abs_start = start_index + relative_start;

                let info = MatchInfo { raw_match, match_index: abs_start, remove: true, skip_from_title: false };

                context.matched.insert("volumes".to_string(), info.clone());
                return Some(info);
            }
            None
        }),
    );

    let ep_regex = FancyRegex::new(r"\b\d{1,4}\b").unwrap();

    parser.add_handler_fn(
        "episodes",
        Box::new(move |context: &mut ParseContext| -> Option<MatchInfo> {
            if context.matched.contains_key("episodes") {
                return None;
            }

            let title = &context.title;

            if anime_regex.is_match(title).unwrap_or(false) {
                if let Ok(Some(m)) = ep_regex.find(title) {
                    let raw_match = m.as_str().to_string();
                    let val = raw_match.parse::<u32>().unwrap_or(0);

                    context.result.episodes.push(val);

                    let info = MatchInfo { raw_match, match_index: m.start(), remove: true, skip_from_title: true };
                    context.matched.insert("episodes".to_string(), info.clone());
                    return Some(info);
                }
            }
            None
        }),
    );
}
