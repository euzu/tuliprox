//! `host::cache-image`: fetch an image once into the host's image cache.

use crate::sys;

/// Fetches `url` through the host (<= 2 MiB, SVG refused, quota-bound) and
/// returns the locally served URL to put in `logo` / `icon` fields.
pub fn cache(url: &str) -> Result<String, String> { sys::cache_image(url) }
