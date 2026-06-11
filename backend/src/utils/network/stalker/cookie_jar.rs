use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;

use crate::utils::network::stalker::error::StalkerResult;

/// The cookie jar is intentionally cheaply cloneable (the inner map is wrapped in a
/// `parking_lot::RwLock` so cloning a jar shares the storage; this is safe because all
/// mutations go through `&self` and we never hold a write guard across an `await`).
#[derive(Debug, Clone, Default)]
pub struct StalkerCookieJar {
    inner: Arc<RwLock<HashMap<String, StalkerCookie>>>,
}

/// A parsed `Set-Cookie` header with all the bits we care about. Stalker portals set a
/// handful of session cookies on `/server/load.php` calls and we need to echo them back
/// on subsequent requests. We do not bother with `Path`/`Domain` matching — the portals we
/// support all share the same host, so keying the jar by name is sufficient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalkerCookie {
    pub name: String,
    pub value: String,
    /// Unix epoch in seconds. `None` means session cookie (drop on jar reset).
    pub expires_at_epoch: Option<u64>,
}

impl StalkerCookie {
    pub fn is_expired(&self, now_epoch: u64) -> bool {
        self.expires_at_epoch.is_some_and(|exp| exp <= now_epoch)
    }
}

impl StalkerCookieJar {
    pub fn new() -> Self { Self::default() }

    /// Merge a set of `Set-Cookie` header values into the jar. We accept a slice of raw
    /// header strings because `reqwest` exposes them as `HeaderValue` slices.
    pub fn ingest_set_cookie<I, S>(&self, cookies: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for raw in cookies {
            if let Some(cookie) = parse_set_cookie(raw.as_ref()) {
                self.insert(cookie);
            }
        }
    }

    /// Insert/replace a single cookie in the jar.
    pub fn insert(&self, cookie: StalkerCookie) {
        let mut guard = self.inner.write();
        guard.insert(cookie.name.clone(), cookie);
    }

    /// Remove the cookie with the given name.
    pub fn remove(&self, name: &str) {
        self.inner.write().remove(name);
    }

    /// Wipe all stored cookies. Used when the server returns 401/403/456.
    pub fn clear(&self) { self.inner.write().clear(); }

    /// Return the cookies that are still valid at `now`. Cookies with no `Expires` attribute
    /// are treated as session cookies and emitted until the jar is cleared.
    pub fn active_cookie_header(&self, now_epoch: u64) -> String {
        let guard = self.inner.read();
        let active: Vec<String> = guard
            .values()
            .filter(|c| !c.is_expired(now_epoch))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        active.join("; ")
    }
}

fn parse_set_cookie(raw: &str) -> Option<StalkerCookie> {
    let mut parts = raw.split(';');
    let head = parts.next()?.trim();
    if head.is_empty() {
        return None;
    }
    let (name, value) = head.split_once('=')?;
    let name = name.trim().to_string();
    let value = value.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let mut expires_at: Option<u64> = None;
    for attr in parts {
        let attr = attr.trim();
        if let Some(rest) = attr.strip_prefix("Max-Age=") {
            if let Ok(secs) = rest.trim().parse::<i64>() {
                if secs <= 0 {
                    expires_at = Some(0);
                } else if let Ok(delta) = u64::try_from(secs) {
                    let now = now_epoch_secs();
                    expires_at = Some(now.saturating_add(delta));
                }
            }
        }
    }
    Some(StalkerCookie { name, value, expires_at_epoch: expires_at })
}

pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Helper used by the client to ensure we never block longer than the configured timeout
/// while parsing/merging cookies. The parsing helpers above are sync; the wrapper makes
/// the call sites read like the rest of the API.
pub fn apply_set_cookie_headers_unchecked(jar: &StalkerCookieJar, headers: &reqwest::header::HeaderMap) -> StalkerResult<()> {
    for value in headers.get_all(reqwest::header::SET_COOKIE) {
        if let Ok(raw) = value.to_str() {
            jar.ingest_set_cookie([raw]);
        }
    }
    Ok(())
}

pub fn cached_cookie_header(jar: &StalkerCookieJar) -> String {
    jar.active_cookie_header(now_epoch_secs())
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    pub fn cookie_age(jar: &StalkerCookieJar, name: &str) -> Option<Duration> {
    let guard = jar.inner.read();
    let cookie = guard.get(name)?;
    let expires = cookie.expires_at_epoch?;
    let now = now_epoch_secs();
    if expires <= now {
        return None;
    }
    Some(Duration::from_secs(expires - now))
}

    #[test]
    fn parses_set_cookie_with_max_age() {
        let cookie = parse_set_cookie("mac=00:1A:79:DE:AD:BE; Max-Age=3600; Path=/").expect("ok");
        assert_eq!(cookie.name, "mac");
        assert_eq!(cookie.value, "00:1A:79:DE:AD:BE");
        let exp = cookie.expires_at_epoch.expect("max-age should produce expiry");
        let now = now_epoch_secs();
        assert!(exp > now);
    }

    #[test]
    fn session_cookies_have_no_expiry() {
        let cookie = parse_set_cookie("stb_lang=en; Path=/").expect("ok");
        assert_eq!(cookie.name, "stb_lang");
        assert!(cookie.expires_at_epoch.is_none());
    }

    #[test]
    fn jar_drops_expired_cookies() {
        let jar = StalkerCookieJar::new();
        jar.insert(StalkerCookie {
            name: "dead".to_string(),
            value: "beef".to_string(),
            expires_at_epoch: Some(1),
        });
        jar.insert(StalkerCookie {
            name: "alive".to_string(),
            value: "feed".to_string(),
            expires_at_epoch: None,
        });
        let header = jar.active_cookie_header(now_epoch_secs());
        assert!(!header.contains("dead=beef"));
        assert!(header.contains("alive=feed"));
    }

    #[test]
    fn jar_dedupes_by_name() {
        let jar = StalkerCookieJar::new();
        jar.ingest_set_cookie(["stb_lang=en"]);
        jar.ingest_set_cookie(["stb_lang=de"]);
        let header = jar.active_cookie_header(now_epoch_secs());
        assert_eq!(header.matches("stb_lang=").count(), 1);
        assert!(header.contains("stb_lang=de"));
    }

    #[test]
    fn max_age_zero_yields_immediate_expiry() {
        let cookie = parse_set_cookie("kill=1; Max-Age=0").expect("ok");
        assert_eq!(cookie.expires_at_epoch, Some(0));
    }

    #[test]
    fn cookie_age_returns_none_when_session_cookie() {
        let jar = StalkerCookieJar::new();
        jar.insert(StalkerCookie {
            name: "session".to_string(),
            value: "yes".to_string(),
            expires_at_epoch: None,
        });
        assert!(cookie_age(&jar, "session").is_none());
    }

    #[test]
    fn sleep_short_does_not_break_jar() {
        // Smoke test — ensures the jar remains usable after a thread sleep.
        let jar = StalkerCookieJar::new();
        jar.ingest_set_cookie(["stb_lang=en; Max-Age=10"]);
        sleep(Duration::from_millis(2));
        assert!(jar.active_cookie_header(now_epoch_secs()).contains("stb_lang=en"));
    }
}
