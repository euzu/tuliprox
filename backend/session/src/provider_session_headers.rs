use cookie::Cookie;
use log::debug;
use reqwest::header::{HeaderMap, SET_COOKIE};
use std::{borrow::Cow, collections::HashMap, sync::Arc};
use url::Url;

// Bounds per playback session so a provider cannot grow the jar without limit. The cookie
// limits follow the RFC 6265 minimum capabilities for user agents.
const MAX_ORIGINS: usize = 16;
const MAX_HEADERS_PER_ORIGIN: usize = 32;
const MAX_COOKIES_PER_ORIGIN: usize = 50;
const MAX_COOKIE_BYTES: usize = 4096;

/// Provider response metadata on its way into the session store: selected headers plus the raw
/// `Set-Cookie` values, whose attributes (path, expiry, `Secure`, domain) the store evaluates.
#[derive(Clone, Debug, Default)]
pub struct ProviderSessionHeaders {
    pub headers: HashMap<String, String>,
    pub cookies: Vec<String>,
}

impl ProviderSessionHeaders {
    pub fn is_empty(&self) -> bool { self.headers.is_empty() && self.cookies.is_empty() }

    pub fn from_response(headers: &HeaderMap, selected: HashMap<String, String>) -> Self {
        Self {
            headers: selected,
            cookies: headers
                .get_all(SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok().map(str::to_owned))
                .collect(),
        }
    }
}

#[derive(Clone, Debug)]
struct StoredCookie {
    cookie: Cookie<'static>,
    path: String,
    expires_at: Option<i64>,
}

impl StoredCookie {
    fn applies_to(&self, url: &Url, now: i64) -> bool {
        let path = url.path();
        self.expires_at.is_none_or(|expiry| expiry > now)
            && (self.cookie.secure() != Some(true) || url.scheme() == "https")
            && (path == self.path
                || (path.starts_with(&self.path)
                    && (self.path.ends_with('/') || path.as_bytes().get(self.path.len()) == Some(&b'/'))))
    }
}

#[derive(Clone, Debug, Default)]
struct OriginHeaders {
    headers: HashMap<String, String>,
    cookies: Vec<StoredCookie>,
}

impl OriginHeaders {
    /// Whether the origin still carries anything to send: a provider header or a live cookie.
    fn has_content(&self, now: i64) -> bool {
        self.headers.keys().any(|name| name != "cookie")
            || self.cookies.iter().any(|cookie| cookie.expires_at.is_none_or(|expiry| expiry > now))
    }
}

/// Cookies stay within the exact origin that issued them, including scheme and port.
/// Origins are shared copy-on-write, so updating one origin never copies the others.
#[derive(Clone, Debug, Default)]
pub struct ProviderSessionCookieStore {
    origins: HashMap<String, Arc<OriginHeaders>>,
}

pub(crate) fn origin_key(url: &Url) -> Option<String> {
    Some(format!("{}://{}:{}", url.scheme(), url.host_str()?, url.port_or_known_default()?))
}

impl ProviderSessionCookieStore {
    pub fn is_empty(&self) -> bool { self.origins.is_empty() }
    pub fn clear(&mut self) { self.origins.clear(); }

    pub fn update(&mut self, source: &Url, response: &ProviderSessionHeaders) {
        if response.is_empty() {
            return;
        }
        let Some(origin) = origin_key(source) else {
            return;
        };
        let now = chrono::Utc::now().timestamp();
        if !self.origins.contains_key(&origin) && self.origins.len() >= MAX_ORIGINS {
            // Origins whose cookies all expired must not block new ones.
            self.origins.retain(|_, entry| entry.has_content(now));
            if self.origins.len() >= MAX_ORIGINS {
                debug!("Provider session cookie store is full; ignoring headers from a new origin");
                return;
            }
        }
        let entry = Arc::make_mut(self.origins.entry(origin.clone()).or_default());
        for (name, value) in response.headers.iter().filter(|(name, _)| !name.eq_ignore_ascii_case("cookie")) {
            if entry.headers.len() < MAX_HEADERS_PER_ORIGIN || entry.headers.contains_key(name) {
                entry.headers.insert(name.clone(), value.clone());
            }
        }
        entry.cookies.retain(|cookie| cookie.expires_at.is_none_or(|expiry| expiry > now));
        for raw in &response.cookies {
            if raw.len() > MAX_COOKIE_BYTES {
                continue;
            }
            let Ok(cookie) = Cookie::parse(raw.as_str()) else {
                continue;
            };
            if cookie.name().is_empty() {
                continue;
            }
            if let Some(domain) = cookie.domain() {
                let domain = domain.trim_start_matches('.').to_ascii_lowercase();
                let host = source.host_str().unwrap_or_default();
                let ip_host = matches!(source.host(), Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)));
                if host != domain
                    && (ip_host || !host.strip_suffix(&domain).is_some_and(|prefix| prefix.ends_with('.')))
                {
                    continue;
                }
            }
            let path = cookie.path().filter(|path| path.starts_with('/')).map_or_else(
                || {
                    source
                        .path()
                        .rsplit_once('/')
                        .map_or("/", |(parent, _)| if parent.is_empty() { "/" } else { parent })
                        .to_string()
                },
                str::to_owned,
            );
            let expires_at = cookie
                .max_age()
                .map(|age| now.saturating_add(age.whole_seconds()))
                .or_else(|| cookie.expires_datetime().map(cookie::time::OffsetDateTime::unix_timestamp));
            entry.cookies.retain(|existing| existing.cookie.name() != cookie.name() || existing.path != path);
            if expires_at.is_none_or(|expiry| expiry > now) {
                if entry.cookies.len() >= MAX_COOKIES_PER_ORIGIN {
                    debug!("Provider session cookie store is full for an origin; ignoring a new cookie");
                    continue;
                }
                entry.cookies.push(StoredCookie { cookie: cookie.into_owned(), path, expires_at });
            }
        }
        entry.cookies.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
        replace_cookie_header(&mut entry.headers, entry.cookies.iter().map(|cookie| &cookie.cookie));
        if entry.headers.is_empty() && entry.cookies.is_empty() {
            self.origins.remove(&origin);
        }
    }

    pub fn headers_for(&self, target: &str) -> Option<Cow<'_, HashMap<String, String>>> {
        let url = Url::parse(target).ok()?;
        let entry = self.origins.get(&origin_key(&url)?)?;
        let now = chrono::Utc::now().timestamp();
        if entry.cookies.iter().all(|cookie| cookie.applies_to(&url, now)) {
            return (!entry.headers.is_empty()).then_some(Cow::Borrowed(&entry.headers));
        }
        let mut headers = entry.headers.clone();
        replace_cookie_header(
            &mut headers,
            entry.cookies.iter().filter(|cookie| cookie.applies_to(&url, now)).map(|cookie| &cookie.cookie),
        );
        (!headers.is_empty()).then_some(Cow::Owned(headers))
    }
}

fn replace_cookie_header<'a>(
    headers: &mut HashMap<String, String>,
    cookies: impl Iterator<Item = &'a Cookie<'static>>,
) {
    let pairs = cookies.map(|cookie| format!("{}={}", cookie.name(), cookie.value())).collect::<Vec<_>>();
    if pairs.is_empty() {
        headers.remove("cookie");
    } else {
        headers.insert("cookie".to_string(), pairs.join("; "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(cookies: &[&str]) -> ProviderSessionHeaders {
        ProviderSessionHeaders {
            headers: HashMap::new(),
            cookies: cookies.iter().map(|value| (*value).to_string()).collect(),
        }
    }

    fn cookie_header(store: &ProviderSessionCookieStore, url: &str) -> Option<String> {
        store.headers_for(url).and_then(|headers| headers.get("cookie").cloned())
    }

    #[test]
    fn origins_keep_independent_cookies_and_partial_updates_merge_by_name() -> Result<(), Box<dyn std::error::Error>> {
        let mut store = ProviderSessionCookieStore::default();
        let manifest = Url::parse("https://manifest.example/channel/index.m3u8")?;
        let cdn = Url::parse("https://cdn.example/init.mp4")?;
        store.update(&manifest, &response(&["sid=manifest; Path=/", "pref=1; Path=/"]));
        store.update(&cdn, &response(&["sid=cdn; Path=/"]));
        store.update(&manifest, &response(&["sid=rotated; Path=/"]));
        let header = cookie_header(&store, manifest.as_str()).ok_or("manifest cookies missing")?;
        assert!(header.contains("sid=rotated"));
        assert!(header.contains("pref=1"));
        assert!(!header.contains("sid=manifest"));
        assert_eq!(cookie_header(&store, cdn.as_str()).as_deref(), Some("sid=cdn"));
        assert!(cookie_header(&store, "https://manifest.example:444/channel/index.m3u8").is_none());
        assert!(cookie_header(&store, "http://manifest.example/channel/index.m3u8").is_none());
        Ok(())
    }

    #[test]
    fn path_matching_default_paths_and_duplicate_names_are_preserved() -> Result<(), Box<dyn std::error::Error>> {
        let mut store = ProviderSessionCookieStore::default();
        let source = Url::parse("https://provider.example/channel/index.m3u8")?;
        store.update(&source, &response(&["sid=channel", "sid=video; Path=/channel/tracks-v1"]));
        assert_eq!(
            cookie_header(&store, "https://provider.example/channel/tracks-v1/init.mp4").as_deref(),
            Some("sid=video; sid=channel")
        );
        assert_eq!(
            cookie_header(&store, "https://provider.example/channel/tracks-a1/init.mp4").as_deref(),
            Some("sid=channel")
        );
        assert_eq!(
            cookie_header(&store, "https://provider.example/channel/tracks-v10/init.mp4").as_deref(),
            Some("sid=channel")
        );
        assert!(cookie_header(&store, "https://provider.example/channel-other/init.mp4").is_none());
        Ok(())
    }

    #[test]
    fn deletion_expiry_and_max_age_precedence_do_not_remove_other_cookies() -> Result<(), Box<dyn std::error::Error>> {
        let mut store = ProviderSessionCookieStore::default();
        let source = Url::parse("https://provider.example/index.m3u8")?;
        store.update(&source, &response(&["sid=first; Path=/", "pref=1; Path=/"]));
        store.update(
            &source,
            &response(&[
                "sid=gone; Path=/; Max-Age=0",
                "old=x; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
                "fresh=yes; Path=/; Max-Age=60; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            ]),
        );
        let header = cookie_header(&store, source.as_str()).ok_or("cookies missing")?;
        assert!(header.contains("pref=1"));
        assert!(header.contains("fresh=yes"));
        assert!(!header.contains("sid="));
        assert!(!header.contains("old="));
        // Read-time expiry applies even if no further Set-Cookie update arrives.
        for entry in store.origins.values_mut() {
            for cookie in &mut Arc::make_mut(entry).cookies {
                if cookie.cookie.name() == "fresh" {
                    cookie.expires_at = Some(chrono::Utc::now().timestamp() - 1);
                }
            }
        }
        assert_eq!(cookie_header(&store, source.as_str()).as_deref(), Some("pref=1"));
        store.update(&source, &response(&["pref=x; Path=/; Max-Age=-1"]));
        assert!(cookie_header(&store, source.as_str()).is_none());
        Ok(())
    }

    #[test]
    fn secure_cookies_and_foreign_domains_are_filtered() -> Result<(), Box<dyn std::error::Error>> {
        let mut store = ProviderSessionCookieStore::default();
        let source = Url::parse("https://cdn.example.org/index.m3u8")?;
        store.update(
            &source,
            &response(&[
                "sid=secure; Secure; Path=/",
                "spoof=no; Domain=other.org; Path=/",
                "domain=yes; Domain=.example.org; Path=/",
            ]),
        );
        let header = cookie_header(&store, source.as_str()).ok_or("secure cookies missing")?;
        assert!(header.contains("sid=secure"));
        assert!(header.contains("domain=yes"));
        assert!(!header.contains("spoof="));
        assert!(cookie_header(&store, "https://other.example.org/index.m3u8").is_none());
        let insecure = Url::parse("http://cdn.example.org/index.m3u8")?;
        store.update(&insecure, &response(&["sid=secure; Secure; Path=/"]));
        assert!(cookie_header(&store, insecure.as_str()).is_none());
        Ok(())
    }

    #[test]
    fn empty_responses_do_not_create_origin_entries() -> Result<(), Box<dyn std::error::Error>> {
        let mut store = ProviderSessionCookieStore::default();
        store.update(&Url::parse("https://provider.example/index.m3u8")?, &ProviderSessionHeaders::default());
        assert!(store.is_empty());
        Ok(())
    }

    #[test]
    fn ip_hosts_reject_domain_cookies() -> Result<(), Box<dyn std::error::Error>> {
        for source in ["http://10.0.0.1/index.m3u8", "http://[::1]/index.m3u8"] {
            let mut store = ProviderSessionCookieStore::default();
            let source = Url::parse(source)?;
            store.update(&source, &response(&["domain=no; Domain=0.0.1; Path=/", "host=yes; Path=/"]));
            assert_eq!(cookie_header(&store, source.as_str()).as_deref(), Some("host=yes"), "{source}");
        }
        Ok(())
    }

    #[test]
    fn store_limits_origins_cookies_headers_and_cookie_size() -> Result<(), Box<dyn std::error::Error>> {
        let mut store = ProviderSessionCookieStore::default();
        for index in 0..=MAX_ORIGINS {
            store.update(&Url::parse(&format!("https://cdn{index}.example/x.ts"))?, &response(&["sid=1"]));
        }
        assert_eq!(store.origins.len(), MAX_ORIGINS);
        assert!(cookie_header(&store, &format!("https://cdn{MAX_ORIGINS}.example/x.ts")).is_none());

        let source = Url::parse("https://cdn0.example/x.ts")?;
        let many = (0..MAX_COOKIES_PER_ORIGIN + 5).map(|index| format!("c{index}=v")).collect::<Vec<_>>();
        store.update(&source, &response(&many.iter().map(String::as_str).collect::<Vec<_>>()));
        let oversized = format!("big={}", "x".repeat(MAX_COOKIE_BYTES));
        store.update(&source, &response(&[oversized.as_str()]));
        let entry = store.origins.values().find(|entry| entry.cookies.iter().any(|c| c.cookie.name() == "c0"));
        let entry = entry.ok_or("origin missing")?;
        assert_eq!(entry.cookies.len(), MAX_COOKIES_PER_ORIGIN);
        assert!(entry.cookies.iter().all(|cookie| cookie.cookie.name() != "big"));
        // Replacing a known cookie is still possible at the limit.
        store.update(&source, &response(&["sid=2"]));
        assert!(cookie_header(&store, source.as_str()).ok_or("cookies missing")?.contains("sid=2"));

        let headers = ProviderSessionHeaders {
            headers: (0..MAX_HEADERS_PER_ORIGIN + 3).map(|index| (format!("x-h{index}"), "v".to_string())).collect(),
            cookies: Vec::new(),
        };
        store.update(&source, &headers);
        let entry =
            store.origins.values().find(|entry| entry.headers.contains_key("cookie")).ok_or("origin missing")?;
        assert!(entry.headers.len() <= MAX_HEADERS_PER_ORIGIN + 1, "header names are capped besides the cookie");
        Ok(())
    }

    #[test]
    fn origins_without_live_content_are_dropped_and_free_their_slot() -> Result<(), Box<dyn std::error::Error>> {
        let mut store = ProviderSessionCookieStore::default();
        let source = Url::parse("https://provider.example/index.m3u8")?;
        store.update(&source, &response(&["sid=1; Path=/"]));
        store.update(&source, &response(&["sid=1; Path=/; Max-Age=0"]));
        assert!(store.is_empty(), "deleting the last cookie removes the origin");

        for index in 0..MAX_ORIGINS {
            store.update(&Url::parse(&format!("https://cdn{index}.example/x.ts"))?, &response(&["sid=1"]));
        }
        let expired = origin_key(&Url::parse("https://cdn0.example/x.ts")?).ok_or("origin key")?;
        for cookie in &mut Arc::make_mut(store.origins.get_mut(&expired).ok_or("origin missing")?).cookies {
            cookie.expires_at = Some(chrono::Utc::now().timestamp() - 1);
        }
        let fresh = Url::parse("https://fresh.example/x.ts")?;
        store.update(&fresh, &response(&["sid=fresh"]));
        assert_eq!(cookie_header(&store, fresh.as_str()).as_deref(), Some("sid=fresh"));
        assert!(!store.origins.contains_key(&expired), "the expired origin gave up its slot");
        assert_eq!(store.origins.len(), MAX_ORIGINS);
        Ok(())
    }

    #[test]
    fn updating_one_origin_keeps_other_origins_shared() -> Result<(), Box<dyn std::error::Error>> {
        let mut store = ProviderSessionCookieStore::default();
        let entry = Url::parse("https://entry.example/index.m3u8")?;
        let cdn = Url::parse("https://cdn.example/init.mp4")?;
        store.update(&entry, &response(&["sid=entry"]));
        store.update(&cdn, &response(&["sid=cdn"]));
        let snapshot = store.clone();
        store.update(&cdn, &response(&["sid=rotated"]));
        let key = origin_key(&entry).ok_or("origin key")?;
        assert!(Arc::ptr_eq(&store.origins[&key], &snapshot.origins[&key]), "untouched origins are not copied");
        assert_eq!(cookie_header(&snapshot, cdn.as_str()).as_deref(), Some("sid=cdn"), "snapshots keep their state");
        Ok(())
    }

    #[test]
    fn response_metadata_preserves_attributes_and_does_not_expose_set_cookie_as_request_header(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut headers = HeaderMap::new();
        headers.append(SET_COOKIE, "sid=x; Path=/video; Secure; Max-Age=30".parse()?);
        let response = ProviderSessionHeaders::from_response(
            &headers,
            HashMap::from([("cookie".to_string(), "sid=x".to_string())]),
        );
        assert_eq!(response.cookies, ["sid=x; Path=/video; Secure; Max-Age=30"]);
        assert!(!response.headers.contains_key("set-cookie"));
        Ok(())
    }
}
