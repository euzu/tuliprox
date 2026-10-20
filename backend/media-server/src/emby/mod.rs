pub mod client;
pub mod dto;
pub mod flavor;
pub mod mapper;

pub type EmbyCatalogClient = client::EmbyFamilyCatalogClient<flavor::Emby>;
