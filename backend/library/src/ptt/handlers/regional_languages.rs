use super::{const_string, options_no_skip, options_remove_no_skip, options_skip_from_title_no_skip, push_language};
use crate::ptt::parser::{handler_options, PttParser};
use fancy_regex::Regex as FancyRegex;

#[allow(clippy::too_many_lines)]
pub(super) fn register(parser: &mut PttParser) {
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(PLDUB|PLSUB|DUBPL|DubbingPL|LekPL|LektorPL)\b").unwrap(),
        const_string("pl"),
        push_language,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(PLDUB|PLSUB|DUBPL|DubbingPL|LekPL|LektorPL)\b").unwrap(),
        const_string("pl"),
        push_language,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(PLDUB|PLSUB|DUBPL|DubbingPL|LekPL|LektorPL)\b").unwrap(),
        const_string("pl"),
        push_language,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)tel(?!\W*aviv)|telugu)\b").unwrap(),
        const_string("te"),
        push_language,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bt[aâ]m(?:il)?\b").unwrap(),
        const_string("ta"),
        push_language,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)MAL(?:ay)?|malayalam)\b").unwrap(),
        const_string("ml"),
        push_language,
        handler_options! {
            remove: true,
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)KAN(?:nada)?|kannada)\b").unwrap(),
        const_string("kn"),
        push_language,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)MAR(?:a(?:thi)?)?|marathi)\b").unwrap(),
        const_string("mr"),
        push_language,
        options_no_skip(),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)GUJ(?:arati)?|gujarati)\b").unwrap(),
        const_string("gu"),
        push_language,
        options_no_skip(),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)PUN(?:jabi)?|punjabi)\b").unwrap(),
        const_string("pa"),
        push_language,
        options_no_skip(),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)BEN(?!.\bThe|and|of\b)(?:gali)?|bengali)\b").unwrap(),
        const_string("bn"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)(?<!shang-?)\bCH(?:I|T)\b").unwrap(),
        const_string("zh"),
        push_language,
        options_skip_from_title_no_skip(),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(chinese|chin[eê]s)\b").unwrap(),
        const_string("zh"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bzh-hans\b").unwrap(),
        const_string("zh"),
        push_language,
        options_no_skip(),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\benglish?\b").unwrap(),
        const_string("en"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
}
