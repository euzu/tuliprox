use std::cmp::min;
use std::collections::HashMap;
use fancy_regex::{Regex as FancyRegex, Match};
use crate::ptt::constants::{BRACKETS, PTT_CONSTANTS};
use crate::ptt::models::PttMetadata;
use shared::create_bitset;

#[derive(Debug, Clone)]
pub struct MatchInfo {
    pub raw_match: String,
    pub match_index: usize,
    pub remove: bool,
    pub skip_from_title: bool,
}

pub struct ParseContext {
    pub title: String,
    pub result: PttMetadata,
    pub matched: HashMap<String, MatchInfo>,
}

pub type HandlerFn = Box<dyn Fn(&mut ParseContext) -> Option<MatchInfo> + Send + Sync>;

create_bitset!(
    u8,
    HandlerOptionFlags,
    SkipIfAlreadyFound,
    SkipFromTitle,
    SkipIfFirst,
    Remove
);

#[derive(Default)]
pub struct PttParser {
    handlers: Vec<(String, HandlerFn)>,
}

impl PttParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_handler_fn(&mut self, name: &str, handler: HandlerFn) {
        self.handlers.push((name.to_string(), handler));
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn add_handler<T, F, S>(
        &mut self,
        name: &str,
        regex: FancyRegex,
        transformer: F,
        setter: S,
        options: HandlerOptions,
    ) where
        T: Clone + Send + Sync + 'static,
        F: Fn(&str) -> T + Send + Sync + 'static,
        S: Fn(&mut PttMetadata, T) + Send + Sync + 'static,
    {
        let name_owned = name.to_string();
        let name_clone = name.to_string();

        let handler = Box::new(move |context: &mut ParseContext| -> Option<MatchInfo> {
            if options
                .flags
                .contains(HandlerOptionFlags::SkipIfAlreadyFound)
                && context.matched.contains_key(&name_owned)
            {
                return None;
            }

            let match_result: Option<Match> = regex.find(&context.title).ok().flatten();

            if let Some(m) = match_result {
                let raw_match = m.as_str().to_string();
                let match_index = m.start();

                let clean_match = if regex.captures_len() > 1 {
                    regex
                        .captures(&context.title)
                        .ok()
                        .flatten()
                        .and_then(|c| c.get(1).map(|g| g.as_str().to_string()))
                        .unwrap_or_else(|| raw_match.clone())
                } else {
                    raw_match.clone()
                };

                let before_match_opt = PTT_CONSTANTS.before_title_match_regex.find(&context.title);
                let before_title_matched = if let Some(bm) = before_match_opt {
                    let bm_str = bm.as_str();
                    if bm_str.len() > 2 && bm_str.starts_with('[') && bm_str.ends_with(']') {
                        let content = &bm_str[1..bm_str.len() - 1];
                        content.contains(&raw_match)
                    } else {
                        false
                    }
                } else {
                    false
                };

                let current_skip_from_title = before_title_matched
                    || options.flags.contains(HandlerOptionFlags::SkipFromTitle);

                let transformed_value = transformer(&clean_match);

                if options.flags.contains(HandlerOptionFlags::SkipIfFirst) {
                    let other_matches_exist = !context.matched.is_empty();
                    if other_matches_exist {
                        let current_start = match_index;
                        let is_before_all = context
                            .matched
                            .values()
                            .all(|m| current_start < m.match_index);
                        if is_before_all {
                            return None;
                        }
                    }
                }

                setter(&mut context.result, transformed_value);

                let info = MatchInfo {
                    raw_match,
                    match_index,
                    remove: options.flags.contains(HandlerOptionFlags::Remove),
                    skip_from_title: current_skip_from_title,
                };
                context.matched.insert(name_owned.clone(), info.clone());

                return Some(info);
            }
            None
        });

        self.handlers.push((name_clone, handler));
    }

    pub fn parse(&self, raw_title: &str, _translate_languages: bool) -> PttMetadata {
        let mut context = ParseContext {
            title: raw_title.to_string(),
            result: PttMetadata::default(),
            matched: HashMap::new(),
        };

        context.title = PTT_CONSTANTS.sub_pattern.replace_all(raw_title, " ").to_string();

        let mut end_of_title = context.title.len();

        for (_name, handler) in &self.handlers {
            if let Some(match_info) = handler(&mut context) {
                let match_index = match_info.match_index;
                let raw_len = match_info.raw_match.len();

                if match_info.remove && match_index + raw_len <= context.title.len() {
                    context
                        .title
                        .replace_range(match_index..match_index + raw_len, "");
                }

                if !match_info.skip_from_title && match_index > 1 && match_index < end_of_title {
                    end_of_title = match_index;
                }
                if match_info.remove
                    && match_info.skip_from_title
                    && match_index < end_of_title
                    && end_of_title >= raw_len
                {
                    end_of_title -= min(raw_len, end_of_title - match_index);
                }
            }
        }

        let mut result = context.result;

        result.seasons.sort_unstable();
        result.seasons.dedup();
        result.episodes.sort_unstable();
        result.episodes.dedup();
        result.languages.sort_unstable();
        result.languages.dedup();

        let final_title = if end_of_title <= context.title.len() {
            context.title[..end_of_title].to_string()
        } else {
            context.title
        };

        result.title = clean_title(&final_title);

        result
    }
}

pub fn clean_title(raw_title: &str) -> String {
    let mut title = raw_title.replace('_', " ");

    title = PTT_CONSTANTS.movie_regex.replace_all(&title, "").to_string();
    title = PTT_CONSTANTS
        .not_allowed_symbols_at_start_and_end
        .replace_all(&title, "")
        .to_string();
    title = PTT_CONSTANTS
        .russian_cast_regex_fancy
        .replace_all(&title, "")
        .to_string();
    title = PTT_CONSTANTS.star_regex_1.replace_all(&title, "$1").to_string();
    title = PTT_CONSTANTS.star_regex_2.replace_all(&title, "$1").to_string();
    title = PTT_CONSTANTS.alt_titles_regex.replace_all(&title, "").to_string();
    title = PTT_CONSTANTS
        .not_only_non_english_regex
        .replace_all(&title, "$1")
        .to_string();
    title = PTT_CONSTANTS
        .remaining_not_allowed_symbols_at_start_and_end
        .replace_all(&title, "")
        .to_string();
    title = PTT_CONSTANTS.empty_brackets_regex.replace_all(&title, "").to_string();
    title = PTT_CONSTANTS.mp3_regex.replace_all(&title, "").to_string();
    title = PTT_CONSTANTS
        .parantheses_without_content
        .replace_all(&title, "")
        .to_string();
    title = PTT_CONSTANTS
        .special_char_spacing
        .replace_all(&title, "")
        .to_string();

    for (open, close) in BRACKETS {
        let open_count = title.matches(open).count();
        let close_count = title.matches(close).count();
        if open_count != close_count {
            title = title.replace(open, "").replace(close, "");
        }
    }

    if !title.trim().contains(' ') && title.contains('.') {
        title = PTT_CONSTANTS.dot.replace_all(&title, " ").to_string();
    }

    title = PTT_CONSTANTS.redundant_symbols_at_end.replace_all(&title, "").to_string();
    title = PTT_CONSTANTS.spacing_regex.replace_all(&title, " ").trim().to_string();

    title
}

pub struct HandlerOptions {
    pub flags: HandlerOptionFlagsSet,
}

impl HandlerOptions {
    pub fn set_flag(&mut self, flag: HandlerOptionFlags, enabled: bool) {
        if enabled {
            self.flags.add(flag);
        } else {
            self.flags.remove(flag);
        }
    }
}

impl Default for HandlerOptions {
    fn default() -> Self {
        let mut flags = HandlerOptionFlagsSet::new();
        flags.add(HandlerOptionFlags::SkipIfAlreadyFound);
        Self {
            flags,
        }
    }
}

macro_rules! handler_options_set_field {
    ($opts:ident, skip_if_already_found, $value:expr) => {
        $opts.set_flag($crate::ptt::parser::HandlerOptionFlags::SkipIfAlreadyFound, $value);
    };
    ($opts:ident, skip_from_title, $value:expr) => {
        $opts.set_flag($crate::ptt::parser::HandlerOptionFlags::SkipFromTitle, $value);
    };
    ($opts:ident, skip_if_first, $value:expr) => {
        $opts.set_flag($crate::ptt::parser::HandlerOptionFlags::SkipIfFirst, $value);
    };
    ($opts:ident, remove, $value:expr) => {
        $opts.set_flag($crate::ptt::parser::HandlerOptionFlags::Remove, $value);
    };
}
pub(crate) use handler_options_set_field;

macro_rules! handler_options {
    ($($field:ident : $value:expr),* , ..Default::default() $(,)?) => {{
        let mut options = $crate::ptt::parser::HandlerOptions::default();
        $($crate::ptt::parser::handler_options_set_field!(options, $field, $value);)*
        options
    }};
    ($($field:ident : $value:expr),* $(,)?) => {{
        let mut options = $crate::ptt::parser::HandlerOptions::default();
        $($crate::ptt::parser::handler_options_set_field!(options, $field, $value);)*
        options
    }};
}
pub(crate) use handler_options;
