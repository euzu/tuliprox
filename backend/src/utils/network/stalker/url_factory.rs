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

/// Build a list of `StalkerLoadUrl` candidates for a given portal root URL. The portal
/// root may itself live under a sub-path (the classic install is
/// `http://host/stalker_portal/`), so the supplied path is preserved and used as the
/// prefix for every candidate. A URL that already points at `server/load.php` or
/// `portal.php` is treated as the single authoritative endpoint (no sibling fallback);
/// a URL ending in the `c/` web-UI path has that suffix stripped before generating the
/// sibling list.
pub fn load_url_candidates(portal_url: &str) -> StalkerResult<Vec<StalkerLoadUrl>> {
    let parsed = Url::parse(portal_url).map_err(|err| StalkerError::BodyDecode {
        message: format!("portal url parse failed: {err}"),
    })?;
    let origin = {
        let mut trimmed = parsed.clone();
        trimmed.set_path("");
        trimmed.set_query(None);
        trimmed.set_fragment(None);
        trimmed.to_string().trim_end_matches('/').to_string()
    };
    if origin.is_empty() {
        return Err(StalkerError::NoEndpoint { portal: portal_url.to_string() });
    }
    let supplied_path = parsed.path().trim_matches('/').to_string();

    // If the user already supplied a URL that points at one of the load endpoints, we
    // treat it as the single authoritative endpoint — no sibling fallback, since the user
    // has effectively told us "this is the right path".
    for endpoint in ["server/load.php", "portal.php"] {
        let matches_endpoint = supplied_path == endpoint || supplied_path.ends_with(&format!("/{endpoint}"));
        if matches_endpoint {
            let prefix = supplied_path[..supplied_path.len() - endpoint.len()].trim_matches('/');
            let base = join_base(&origin, prefix);
            return Ok(vec![StalkerLoadUrl {
                load_url: format!("{base}/{endpoint}"),
                referer: format!("{base}/c/"),
            }]);
        }
    }

    // `<base>/c/` is the MAG web-UI path, not an API base — strip it before generating
    // the sibling candidates so `http://host/stalker_portal/c/` resolves the same set as
    // `http://host/stalker_portal/`.
    let base_path = supplied_path
        .strip_suffix("/c")
        .or_else(|| (supplied_path == "c").then_some(""))
        .unwrap_or(supplied_path.as_str())
        .trim_matches('/');
    let base = join_base(&origin, base_path);
    let referer = format!("{base}/c/");
    let candidates: Vec<StalkerLoadUrl> = STALKER_LOAD_PATHS
        .iter()
        .map(|path| StalkerLoadUrl {
            load_url: format!("{base}/{path}"),
            referer: referer.clone(),
        })
        .collect();
    Ok(candidates)
}

fn join_base(origin: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        origin.to_string()
    } else {
        format!("{origin}/{prefix}")
    }
}

/// The `Referer` header Stalker portals expect on every API call. By convention this is
/// `<portal base>/c/` — the same path the MAG portal uses for its web UI. Any known load
/// endpoint suffix is stripped from the input first so a sub-path install keeps its
/// prefix (`http://host/stalker_portal/server/load.php` → `http://host/stalker_portal/c/`).
pub fn portal_referer(load_url: &str) -> String {
    let without_query = match Url::parse(load_url) {
        Ok(mut url) => {
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => load_url.to_string(),
    };
    let mut base = without_query.trim_end_matches('/');
    for path in STALKER_LOAD_PATHS {
        let suffix = format!("/{}", path.trim_end_matches('/'));
        if let Some(stripped) = base.strip_suffix(suffix.as_str()) {
            base = stripped.trim_end_matches('/');
            break;
        }
    }
    format!("{base}/c/")
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
        assert_eq!(candidates[0].referer, "http://portal.example/c/");
    }

    #[test]
    fn preserves_sub_path_prefix_for_candidates() {
        let candidates = load_url_candidates("http://portal.example/stalker_portal/").expect("ok");
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].load_url, "http://portal.example/stalker_portal/server/load.php");
        assert_eq!(candidates[1].load_url, "http://portal.example/stalker_portal/portal.php");
        assert_eq!(candidates[2].load_url, "http://portal.example/stalker_portal/c/");
        for c in &candidates {
            assert_eq!(c.referer, "http://portal.example/stalker_portal/c/");
        }
    }

    #[test]
    fn sub_path_load_url_is_single_authoritative_candidate() {
        let candidates =
            load_url_candidates("http://portal.example/stalker_portal/server/load.php").expect("ok");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].load_url, "http://portal.example/stalker_portal/server/load.php");
        assert_eq!(candidates[0].referer, "http://portal.example/stalker_portal/c/");
    }

    #[test]
    fn web_ui_path_is_stripped_before_generating_candidates() {
        let candidates = load_url_candidates("http://portal.example/stalker_portal/c/").expect("ok");
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].load_url, "http://portal.example/stalker_portal/server/load.php");
        assert_eq!(candidates[2].load_url, "http://portal.example/stalker_portal/c/");
    }

    #[test]
    fn referer_truncates_load_path() {
        let referer = portal_referer("http://portal.example/server/load.php");
        assert_eq!(referer, "http://portal.example/c/");
    }

    #[test]
    fn referer_preserves_sub_path_prefix() {
        let referer = portal_referer("http://portal.example/stalker_portal/server/load.php");
        assert_eq!(referer, "http://portal.example/stalker_portal/c/");
    }

    #[test]
    fn rejects_garbage_portal_url() {
        let err = load_url_candidates("not a url").expect_err("fail");
        assert!(matches!(err, StalkerError::BodyDecode { .. }));
    }
}
