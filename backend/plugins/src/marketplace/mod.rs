//! The repository marketplace backend core: the
//! `marketplace.json` format, the fetch policy, and the per-source catalog
//! cache. Nothing here installs anything; a download goes through
//! [`crate::PluginSystem::api_upload_from_marketplace`] and the existing
//! review and approve routes.
//!
//! The narrow public surface is re-exported at the crate root: [`FetchPolicy`],
//! [`MarketplaceError`], [`sync_source`], [`source_info`], [`has_catalog`],
//! [`source_warnings`], [`remove_cache`], [`browse`], [`resolve_release`],
//! [`fetch_package`], the size caps and the update discovery of installed
//! plugins ([`UpdateCandidate`], [`Eligibility`], [`Blocker`]).

mod catalog;
mod fetch;
mod format;
mod updates;

pub use catalog::{
    browse, has_catalog, remove_cache, resolve_release, source_info, source_warnings, sync_source, ReleaseLookup,
    ResolvedRelease,
};
pub use fetch::{fetch_package, FetchPolicy, MarketplaceError, CATALOG_MAX_BYTES, PACKAGE_MAX_BYTES};
pub use updates::{Blocker, Eligibility, UpdateCandidate};
