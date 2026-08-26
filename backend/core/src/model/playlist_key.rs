//! Small shared keys and URL helpers for playlist handling.
//!
//! Both are named by the parsers and by the repositories, so they sit below
//! both rather than in either.

use shared::model::XtreamCluster;
use std::sync::Arc;

/// Identifies a category within one cluster of a target's playlist.
pub type CategoryKey = (XtreamCluster, Arc<str>);

/// Base URL of an Xtream provider's `player_api.php`, with credentials applied.
pub fn get_xtream_stream_url_base(url: &str, username: &str, password: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("username", username)
        .append_pair("password", password)
        .finish();
    format!("{url}/player_api.php?{query}")
}
