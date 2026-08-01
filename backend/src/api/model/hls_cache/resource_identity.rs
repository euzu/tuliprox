use crate::processing::parser::hls::origin_manifest::ParsedByteRange;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use url::Url;

const RESOURCE_IDENTITY_DOMAIN: &[u8] = b"tuliprox/hls/media-resource/v1\0";
const STABLE_NAMESPACE_DOMAIN: &[u8] = b"tuliprox/hls/media-resource-stable-namespace/v1\0";
const MAX_PUBLISHED_RESOURCE_HISTORY: usize = 1_024;

/// Query-independent media identity used by both manifest acceptance and the proxy timeline.
///
/// `exact_path` preserves the complete URL path. `stable_namespace` is present only for the
/// known `<namespace>/<volatile-id>/<stable-file-name>` layout and permits a CDN directory
/// token to rotate without degrading identity to a basename-only comparison.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct HlsMediaResourceIdentity {
    exact_path: [u8; 32],
    stable_namespace: Option<[u8; 32]>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub(crate) struct HlsMediaResourceSemanticKey([u8; 32]);

impl HlsMediaResourceIdentity {
    pub(crate) fn from_url(url: &str, byte_range: Option<ParsedByteRange>) -> Self {
        let path = resource_path(url);
        Self {
            exact_path: hash_identity(RESOURCE_IDENTITY_DOMAIN, path.as_bytes(), byte_range),
            stable_namespace: stable_namespace_path(&path)
                .map(|namespace| hash_identity(STABLE_NAMESPACE_DOMAIN, namespace.as_bytes(), byte_range)),
        }
    }

    pub(crate) fn matches(self, other: Self) -> bool {
        self.exact_path == other.exact_path
            || self
                .stable_namespace
                .zip(other.stable_namespace)
                .is_some_and(|(left, right)| left == right)
    }

    pub(crate) const fn exact_path_hash(self) -> [u8; 32] { self.exact_path }

    pub(crate) const fn semantic_key(self) -> HlsMediaResourceSemanticKey {
        HlsMediaResourceSemanticKey(match self.stable_namespace {
            Some(stable_namespace) => stable_namespace,
            None => self.exact_path,
        })
    }

    #[cfg(test)]
    pub(crate) const fn for_test(marker: u8) -> Self {
        Self { exact_path: [marker; 32], stable_namespace: None }
    }
}

impl HlsMediaResourceSemanticKey {
    pub(crate) const fn bytes(self) -> [u8; 32] { self.0 }

    pub(crate) fn diagnostic_token(self) -> [u8; 8] {
        let mut token = [0_u8; 8];
        token.copy_from_slice(&self.0[..8]);
        token
    }

    #[cfg(test)]
    pub(crate) const fn for_test(bytes: [u8; 32]) -> Self { Self(bytes) }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PublishedResource {
    identity: HlsMediaResourceIdentity,
    proxy_seq: u64,
}

/// Bounded history of media resources that were actually exposed by this HLS session.
///
/// It deliberately outlives individual `SegmentEntry` values so cache/timeline GC cannot
/// make previously published origin media look new again. Dropping the session drops the
/// history and its stream-local namespace.
#[derive(Clone, Debug, Default)]
pub(crate) struct HlsPublishedResourceHistory {
    resources: VecDeque<PublishedResource>,
    generation: u64,
}

impl HlsPublishedResourceHistory {
    pub(crate) fn is_empty(&self) -> bool { self.resources.is_empty() }

    pub(crate) const fn generation(&self) -> u64 { self.generation }

    pub(crate) fn proxy_seq_for(&self, identity: HlsMediaResourceIdentity) -> Option<u64> {
        self.resources
            .iter()
            .rev()
            .find(|published| published.identity.matches(identity))
            .map(|published| published.proxy_seq)
    }

    pub(crate) fn record(&mut self, identity: HlsMediaResourceIdentity, proxy_seq: u64) {
        if let Some(published) =
            self.resources.iter_mut().rev().find(|published| published.identity.matches(identity))
        {
            published.proxy_seq = proxy_seq;
            return;
        }
        if self.resources.len() == MAX_PUBLISHED_RESOURCE_HISTORY {
            self.resources.pop_front();
        }
        self.resources.push_back(PublishedResource { identity, proxy_seq });
        self.generation = self.generation.saturating_add(1);
    }

    pub(crate) fn recent_entries(
        &self,
        limit: usize,
    ) -> impl Iterator<Item = (HlsMediaResourceIdentity, u64)> + '_ {
        self.resources
            .iter()
            .rev()
            .take(limit)
            .map(|published| (published.identity, published.proxy_seq))
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize { self.resources.len() }
}

fn resource_path(url: &str) -> String {
    Url::parse(url)
        .ok()
        .map_or_else(|| url.split(['?', '#']).next().unwrap_or_default().to_string(), |parsed| parsed.path().to_string())
}

fn stable_namespace_path(path: &str) -> Option<String> {
    let mut components = path.split('/').collect::<Vec<_>>();
    // Require a namespace before the volatile parent. Otherwise `/token/file`
    // would collapse to the basename and collide across unrelated streams.
    if components.len() < 4 {
        return None;
    }
    let volatile_index = components.len().saturating_sub(2);
    if !is_volatile_path_component(components.get(volatile_index).copied().unwrap_or_default()) {
        return None;
    }
    components.remove(volatile_index);
    Some(components.join("/"))
}

fn is_volatile_path_component(component: &str) -> bool {
    let compact = component.chars().filter(|character| *character != '-').collect::<String>();
    compact.len() >= 12 && compact.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn hash_identity(domain: &[u8], path: &[u8], byte_range: Option<ParsedByteRange>) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(path);
    hasher.update([0]);
    match byte_range {
        Some(byte_range) => {
            hasher.update([1]);
            hasher.update(byte_range.offset.to_be_bytes());
            hasher.update(byte_range.length.to_be_bytes());
        }
        None => hasher.update([0]),
    }
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::{
        HlsMediaResourceIdentity, HlsMediaResourceSemanticKey, HlsPublishedResourceHistory,
        MAX_PUBLISHED_RESOURCE_HISTORY,
    };
    use crate::processing::parser::hls::origin_manifest::ParsedByteRange;

    #[test]
    fn identity_ignores_query_and_fragment_but_preserves_path_and_range() {
        let first = HlsMediaResourceIdentity::from_url("https://a.example/live/segment.ts?token=one#x", None);
        let second = HlsMediaResourceIdentity::from_url("https://b.example/live/segment.ts?token=two#y", None);
        let other_path = HlsMediaResourceIdentity::from_url("https://b.example/other/segment.ts", None);
        let ranged = HlsMediaResourceIdentity::from_url(
            "https://b.example/live/segment.ts",
            Some(ParsedByteRange { length: 10, offset: 20 }),
        );

        assert!(first.matches(second));
        assert!(!first.matches(other_path));
        assert!(!first.matches(ranged));
    }

    #[test]
    fn known_volatile_parent_uses_a_stable_non_basename_namespace() {
        let first = HlsMediaResourceIdentity::from_url(
            "https://a.example/channel-one/0123456789abcdef/1745180_474.ts?token=one",
            None,
        );
        let rotated = HlsMediaResourceIdentity::from_url(
            "https://b.example/channel-one/fedcba9876543210/1745180_474.ts?token=two",
            None,
        );
        let other_stream = HlsMediaResourceIdentity::from_url(
            "https://b.example/channel-two/fedcba9876543210/1745180_474.ts",
            None,
        );

        assert!(first.matches(rotated));
        assert!(!first.matches(other_stream));
        assert_ne!(first, rotated, "ordinary identity equality remains exact");
        assert_eq!(first.semantic_key(), rotated.semantic_key());
        assert_ne!(first.semantic_key(), other_stream.semantic_key());
    }

    #[test]
    fn root_level_volatile_parent_does_not_collapse_identity_to_basename() {
        let first =
            HlsMediaResourceIdentity::from_url("https://a.example/0123456789abcdef/segment.ts", None);
        let second =
            HlsMediaResourceIdentity::from_url("https://b.example/fedcba9876543210/segment.ts", None);

        assert!(!first.matches(second));
        assert_ne!(first.semantic_key(), second.semantic_key());
    }

    #[test]
    fn semantic_log_token_is_never_identity_evidence() {
        let mut first_bytes = [0x11; 32];
        let mut second_bytes = first_bytes;
        first_bytes[31] = 0x22;
        second_bytes[31] = 0x33;
        let first = HlsMediaResourceSemanticKey::for_test(first_bytes);
        let second = HlsMediaResourceSemanticKey::for_test(second_bytes);

        assert_eq!(first.diagnostic_token(), second.diagnostic_token());
        assert_ne!(first, second);
        assert_eq!(first.diagnostic_token(), first.bytes()[..8]);
    }

    #[test]
    fn published_history_is_bounded_and_matches_rotated_namespace() {
        let mut history = HlsPublishedResourceHistory::default();
        for sequence in 0..=MAX_PUBLISHED_RESOURCE_HISTORY {
            history.record(
                HlsMediaResourceIdentity::from_url(&format!("https://origin.example/live/{sequence}.ts"), None),
                u64::try_from(sequence).unwrap_or(u64::MAX),
            );
        }
        assert_eq!(history.len(), MAX_PUBLISHED_RESOURCE_HISTORY);
        assert_eq!(history.generation(), 1_025);
        assert_eq!(
            history.proxy_seq_for(HlsMediaResourceIdentity::from_url(
                "https://other.example/live/1024.ts?changed=true",
                None,
            )),
            Some(1_024)
        );
        history.record(HlsMediaResourceIdentity::from_url("https://other.example/live/1024.ts", None), 1_024);
        assert_eq!(history.generation(), 1_025, "re-recording an identity must not advance evidence");
    }

    #[test]
    fn published_history_updates_the_same_newest_match_that_lookup_returns() {
        let stable_namespace = [0x7a; 32];
        let oldest = HlsMediaResourceIdentity {
            exact_path: [0x01; 32],
            stable_namespace: Some(stable_namespace),
        };
        let newest = HlsMediaResourceIdentity {
            exact_path: [0x02; 32],
            stable_namespace: Some(stable_namespace),
        };
        let next = HlsMediaResourceIdentity {
            exact_path: [0x03; 32],
            stable_namespace: Some(stable_namespace),
        };
        let mut history = HlsPublishedResourceHistory {
            resources: std::collections::VecDeque::from([
                super::PublishedResource { identity: oldest, proxy_seq: 10 },
                super::PublishedResource { identity: newest, proxy_seq: 20 },
            ]),
            generation: 2,
        };

        history.record(next, 30);

        assert_eq!(history.proxy_seq_for(next), Some(30));
        assert_eq!(history.resources.front().map(|resource| resource.proxy_seq), Some(10));
        assert_eq!(history.resources.back().map(|resource| resource.proxy_seq), Some(30));
        assert_eq!(history.generation(), 2);
    }
}
