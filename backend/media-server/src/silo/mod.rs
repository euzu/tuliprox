pub mod flavor;

pub use flavor::Silo;

pub type SiloCatalogClient = crate::emby::client::EmbyFamilyCatalogClient<flavor::Silo>;
