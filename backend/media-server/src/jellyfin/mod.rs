pub mod dto;
pub mod flavor;

pub type JellyfinCatalogClient = crate::emby::client::EmbyFamilyCatalogClient<flavor::Jellyfin>;
