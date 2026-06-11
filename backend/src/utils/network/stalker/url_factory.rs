use url::Url;

use crate::utils::network::stalker::error::{StalkerError, StalkerResult};

/// The candidate endpoints a Stalker portal might respond on, in priority order. The portal
/// will answer on any of these depending on the firmware flavour (legacy MAG250, Ministra,
/// nginx-bundled middlewares, etc.) so the client iterates through them with a single
/// per-action retry when the first candidate returns 5xx or 404.
pub const STALKER_LOAD_PATHS: [&str; 3] = ["server/load.php", "portal.php", "c/"];

/// A single candidate endpoint along with the path used to populate the `Referer` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalkerLoadUrl {
    /// Full URL to which requests are sent.
    pub load_url: String,
    /// `Referer` value required by the portal (typically the portal root + `c/`).
    pub referer: String,
}

/// Build a list of `StalkerLoadUrl` candidates for a given portal root URL. The portal root
/// may itself be either `<scheme>://host` or a full URL pointing at one of the candidate
/// paths; if it is one of the candidate paths we use that exact URL as the only candidate
/// (no siblings), otherwise we generate the full sibling list.
pub fn load_url_candidates(portal_url: &str) -> StalkerResult<Vec<StalkerLoadUrl>> {
    let parsed = Url::parse(portal_url).map_err(|err| StalkerError::BodyDecode {
        message: format!("portal url parse failed: {err}"),
    })?;
    let base = {
        let mut trimmed = parsed.clone();
        trimmed.set_path("");
        trimmed.set_query(None);
        trimmed.set_fragment(None);
        trimmed.to_string().trim_end_matches('/').to_string()
    };
    if base.is_empty() {
        return Err(StalkerError::NoEndpoint { portal: portal_url.to_string() });
    }

    // If the user already supplied a URL that points at one of the known load paths, we
    // treat it as the single authoritative endpoint — no sibling fallback, since the user
    // has effectively told us "this is the right path".
    let normalized = portal_url.trim_end_matches('/');
    for path in STALKER_LOAD_PATHS {
        let candidate = format!("{base}/{path}");
        if normalized == candidate {
            let referer = portal_referer(&candidate);
            return Ok(vec![StalkerLoadUrl {
                load_url: candidate,
                referer,
            }]);
        }
    }

    let mut candidates: Vec<StalkerLoadUrl> = STALKER_LOAD_PATHS
        .iter()
        .map(|path| StalkerLoadUrl {
            load_url: format!("{base}/{path}"),
            referer: portal_referer(&format!("{base}/{path}")),
        })
        .collect();

    candidates.dedup_by(|a, b| a.load_url == b.load_url);
    if candidates.is_empty() {
        return Err(StalkerError::NoEndpoint { portal: portal_url.to_string() });
    }
    Ok(candidates)
}

/// The `Referer` header Stalker portals expect on every API call. By convention this is
/// `<portal root>/c/` — the same path the MAG portal uses for its web UI.
pub fn portal_referer(load_url: &str) -> String {
    if let Ok(mut url) = Url::parse(load_url) {
        url.set_query(None);
        url.set_fragment(None);
        // Strip the path component entirely; the referer is the portal root + `c/`.
        // We retain a single trailing slash so the output is `<root>/c/`.
        let mut s = url.to_string();
        if let Some(scheme_end) = s.find("://") {
            let after_scheme = scheme_end + 3;
            if let Some(path_idx) = s[after_scheme..].find('/') {
                s.truncate(after_scheme + path_idx);
            }
        }
        let trimmed = s.trim_end_matches('/');
        format!("{trimmed}/c/")
    } else {
        let trimmed = load_url.trim_end_matches('/');
        format!("{trimmed}/c/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_cover_legacy_modern_ministra() {
        let candidates = load_url_candidates("http://portal.example/").expect("ok");
        assert_eq!(candidates.len(), 3);
        assert!(candidates[0].load_url.contains("server/load.php"));
        assert!(candidates[1].load_url.contains("portal.php"));
        assert!(candidates[2].load_url.contains("c/"));
        for c in &candidates {
            assert!(c.referer.contains("portal.example/c/"));
        }
    }

    #[test]
    fn strips_existing_load_path() {
        let candidates =
            load_url_candidates("http://portal.example/server/load.php").expect("ok");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].load_url, "http://portal.example/server/load.php");
    }

    #[test]
    fn referer_truncates_load_path() {
        let referer = portal_referer("http://portal.example/server/load.php");
        assert_eq!(referer, "http://portal.example/c/");
    }

    #[test]
    fn rejects_garbage_portal_url() {
        let err = load_url_candidates("not a url").expect_err("fail");
        assert!(matches!(err, StalkerError::BodyDecode { .. }));
    }
}
