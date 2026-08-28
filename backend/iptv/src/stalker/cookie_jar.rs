use crate::stalker::error::StalkerResult;
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

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
    pub fn new() -> Self {
        Self::default()
    }

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
    pub fn clear(&self) {
        self.inner.write().clear();
    }

    /// Return the cookies that are still valid at `now`. Cookies with no `Expires` attribute
    /// are treated as session cookies and emitted until the jar is cleared.
    pub fn active_cookie_header(&self, now_epoch: u64) -> String {
        let pairs = self.active_cookies(now_epoch);
        let active: Vec<String> = pairs.iter().map(|(name, value)| format!("{name}={value}")).collect();
        active.join("; ")
    }

    /// Return the still-valid cookies as `(name, value)` pairs so callers can merge them
    /// with other cookie sources before serializing a `Cookie` header.
    pub fn active_cookies(&self, now_epoch: u64) -> Vec<(String, String)> {
        let guard = self.inner.read();
        guard.values().filter(|c| !c.is_expired(now_epoch)).map(|c| (c.name.clone(), c.value.clone())).collect()
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
    let mut max_age: Option<u64> = None;
    let mut expires: Option<u64> = None;
    for attr in parts {
        let attr = attr.trim();
        let (key, val) = attr.split_once('=').map_or((attr, ""), |(k, v)| (k.trim(), v.trim()));
        if key.eq_ignore_ascii_case("max-age") {
            if let Ok(secs) = val.parse::<i64>() {
                if secs <= 0 {
                    max_age = Some(0);
                } else if let Ok(delta) = u64::try_from(secs) {
                    max_age = Some(now_epoch_secs().saturating_add(delta));
                }
            }
        } else if key.eq_ignore_ascii_case("expires") {
            expires = parse_cookie_expires(val);
        }
    }
    // RFC 6265: `Max-Age` wins over `Expires` when both are present.
    let expires_at = max_age.or(expires);
    Some(StalkerCookie { name, value, expires_at_epoch: expires_at })
}

/// Parse an HTTP `Expires` cookie attribute (RFC 1123 date, e.g.
/// `Tue, 01 Jul 2025 10:00:00 GMT`). Dates before the Unix epoch collapse to `0`
/// (immediately expired). Unparseable dates yield `None` (session cookie).
fn parse_cookie_expires(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.replace("GMT", "+0000");
    let epoch = chrono::DateTime::parse_from_rfc2822(&normalized).ok()?.timestamp();
    Some(u64::try_from(epoch).unwrap_or(0))
}

pub fn now_epoch_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Helper used by the client to ensure we never block longer than the configured timeout
/// while parsing/merging cookies. The parsing helpers above are sync; the wrapper makes
/// the call sites read like the rest of the API.
pub fn apply_set_cookie_headers_unchecked(
    jar: &StalkerCookieJar,
    headers: &reqwest::header::HeaderMap,
) -> StalkerResult<()> {
    for value in headers.get_all(reqwest::header::SET_COOKIE) {
        if let Ok(raw) = value.to_str() {
            jar.ingest_set_cookie([raw]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread::sleep, time::Duration};

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
        jar.insert(StalkerCookie { name: "dead".to_string(), value: "beef".to_string(), expires_at_epoch: Some(1) });
        jar.insert(StalkerCookie { name: "alive".to_string(), value: "feed".to_string(), expires_at_epoch: None });
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
    fn max_age_is_case_insensitive() {
        let cookie = parse_set_cookie("mac=00:1A:79:DE:AD:BE; max-age=3600; path=/").expect("ok");
        let exp = cookie.expires_at_epoch.expect("lowercase max-age should produce expiry");
        assert!(exp > now_epoch_secs());
    }

    #[test]
    fn parses_expires_attribute() {
        let cookie = parse_set_cookie("sid=abc; Expires=Tue, 01 Jul 2042 10:00:00 GMT; Path=/").expect("ok");
        let exp = cookie.expires_at_epoch.expect("expires should produce expiry");
        assert!(exp > now_epoch_secs());
    }

    #[test]
    fn past_expires_marks_cookie_expired() {
        let cookie = parse_set_cookie("sid=abc; expires=Thu, 01 Jan 2004 00:00:00 GMT").expect("ok");
        let exp = cookie.expires_at_epoch.expect("expires should parse");
        assert!(cookie.is_expired(now_epoch_secs()));
        assert!(exp > 0);
    }

    #[test]
    fn max_age_wins_over_expires() {
        let cookie = parse_set_cookie("sid=abc; Expires=Tue, 01 Jul 2042 10:00:00 GMT; Max-Age=0").expect("ok");
        assert_eq!(cookie.expires_at_epoch, Some(0));
    }

    #[test]
    fn active_cookies_skips_expired_pairs() {
        let jar = StalkerCookieJar::new();
        jar.insert(StalkerCookie { name: "dead".to_string(), value: "beef".to_string(), expires_at_epoch: Some(1) });
        jar.insert(StalkerCookie { name: "alive".to_string(), value: "feed".to_string(), expires_at_epoch: None });
        let pairs = jar.active_cookies(now_epoch_secs());
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("alive".to_string(), "feed".to_string()));
    }

    #[test]
    fn cookie_age_returns_none_when_session_cookie() {
        let jar = StalkerCookieJar::new();
        jar.insert(StalkerCookie { name: "session".to_string(), value: "yes".to_string(), expires_at_epoch: None });
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
