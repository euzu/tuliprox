//! Jellyfin uses Emby-compatible JSON shapes for the MVP endpoints Tuliprox needs.
//!
//! Keep aliases in this module so caller code still depends on a provider-specific
//! seam and can diverge safely when Jellyfin fields differ.

pub type JellyfinPublicSystemInfoDto = crate::emby::dto::EmbyPublicSystemInfoDto;
pub type JellyfinItemsPageDto<T> = crate::emby::dto::EmbyItemsPageDto<T>;
pub type JellyfinViewDto = crate::emby::dto::EmbyViewDto;
pub type JellyfinItemDto = crate::emby::dto::EmbyItemDto;
pub type JellyfinMediaSourceDto = crate::emby::dto::EmbyMediaSourceDto;
pub type JellyfinPlaybackInfoDto = crate::emby::dto::EmbyPlaybackInfoDto;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::JELLYFIN_VIEWS_JSON;

    #[test]
    fn parses_jellyfin_views_through_provider_specific_aliases() {
        let page: JellyfinItemsPageDto<JellyfinViewDto> =
            serde_json::from_str(JELLYFIN_VIEWS_JSON).expect("fixture parses");

        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].collection_type.as_deref(), Some("movies"));
        assert_eq!(page.items[1].collection_type.as_deref(), Some("tvshows"));
    }
}
