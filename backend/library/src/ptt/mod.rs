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
