use super::series::{parse_season_episode_field, parse_season_field};

/// Field names accepted by resource-proxy endpoints. This is deliberately defined beside the
/// resource traversal and selector so adding a new endpoint-visible resource has one review site.
///
/// Stream URLs are not resources: serving them here would sidestep the stream routes and their
/// admission accounting.
pub fn is_resource_field_name(field: &str) -> bool {
    if matches!(field, "logo" | "logo_small" | "cover" | "movie_image" | "nfo_cover_big" | "nfo_movie_image")
        || field.starts_with("backdrop_path")
        || field.starts_with("nfo_backdrop_path")
    {
        return true;
    }
    parse_season_field(field).is_some_and(|(_, field)| matches!(field.as_str(), "cover" | "cover_tmdb" | "cover_big"))
        || parse_season_episode_field(field).is_some_and(|(_, _, field)| field == "movie_image")
}
