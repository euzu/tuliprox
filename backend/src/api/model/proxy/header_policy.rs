//! Shared hop-by-hop header policy for the HLS cache proxy and the MPEG-TS reverse-proxy.
//!
//! Both pipelines strip the same security-sensitive headers before forwarding to the
//! origin; before this module existed the HLS cache hard-coded its own list
//! (`should_remove_hls_origin_header`) that drifted from the `ReverseProxyDisabledHeaderConfig`
//! path used by the MPEG-TS reverse proxy. Adding a new hop-by-hop default required
//! editing two files; let a future maintainer forget one and the operator's
//! "disabled header" config silently leaks on the HLS path.
//!
//! The single source of truth here feeds both call sites via `HopByHopHeader::is_sensitive`.

use crate::model::ReverseProxyDisabledHeaderConfig;

/// Protocol family a header policy is being applied for.
///
/// Today only `Hls` ships with hard-coded defaults; `MpegTs` defers entirely to the
/// operator-configured `ReverseProxyDisabledHeaderConfig`. The enum exists so future
/// asymmetric rules (e.g. `Sec-Fetch-*` for HLS, `Cookie2` for legacy MPEG-TS providers)
/// have a single landing point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderProtocol {
    Hls,
    MpegTs,
}

/// Static, hard-coded list of headers whose presence is *always* a security problem on
/// outbound origin requests: transport-layer framing (`Connection`, `Transfer-Encoding`,
/// `Upgrade`), authentication credentials (`Authorization`, `Cookie`, `Proxy-Authorization`),
/// and Tuliprox internal markers (`x-tuliprox-*`).
///
/// The list does NOT depend on operator config; the config only adds *additional*
/// removals via `ReverseProxyDisabledHeaderConfig::should_remove`.
const ALWAYS_SENSITIVE: &[&str] = &[
    "authorization",
    "connection",
    "cookie",
    "cookie2",
    "host",
    "proxy-authorization",
    "set-cookie",
    "te",
    "trailer",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Hard-coded Tuliprox internal header prefix — never forwarded to origin regardless
/// of config. Matches `x-tuliprox-*`.
const TULIPROX_HEADER_PREFIX: &str = "x-tuliprox-";

/// Single hop-by-hop header policy shared between HLS and MPEG-TS proxy paths.
///
/// `is_sensitive` returns true when the header must be stripped before sending the
/// request to the origin. The operator-configured `ReverseProxyDisabledHeaderConfig`
/// is layered on top of the hard-coded defaults so adding a new "disabled header"
/// config applies to both protocols uniformly.
pub struct HopByHopHeader;

impl HopByHopHeader {
    /// True when a header must never be forwarded to the origin for the given protocol
    /// family. Combines:
    ///   1. hard-coded hop-by-hop + Tuliprox-internal headers (always sensitive)
    ///   2. operator-configured disabled-header list, when supplied
    ///
    /// The protocol-family parameter is reserved for future asymmetric rules. Today both
    /// `Hls` and `MpegTs` share the same set; switching on it costs nothing today and
    /// gives the next maintainer one obvious place to extend.
    pub fn is_sensitive(
        header_name: &str,
        protocol: HeaderProtocol,
        disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
    ) -> bool {
        let _ = protocol; // reserved for future per-protocol rules
        let header_lc = header_name.trim().to_ascii_lowercase();
        ALWAYS_SENSITIVE.contains(&header_lc.as_str())
            || header_lc.starts_with(TULIPROX_HEADER_PREFIX)
            || disabled_headers.is_some_and(|disabled| disabled.should_remove(header_lc.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disabled() -> ReverseProxyDisabledHeaderConfig {
        ReverseProxyDisabledHeaderConfig {
            referer_header: true,
            x_header: true,
            cloudflare_header: true,
            custom_header: vec!["X-Origin-Secret".to_string()],
        }
    }

    #[test]
    fn strips_hardcoded_hop_by_hop_for_both_protocols() {
        for header in ["Authorization", "Cookie", "Connection", "TE", "Trailer", "Transfer-Encoding",
                       "Upgrade", "Proxy-Authorization", "Host", "Set-Cookie"] {
            assert!(
                HopByHopHeader::is_sensitive(header, HeaderProtocol::Hls, None),
                "Hls should drop hard-coded {header}"
            );
            assert!(
                HopByHopHeader::is_sensitive(header, HeaderProtocol::MpegTs, None),
                "MpegTs should drop hard-coded {header}"
            );
        }
    }

    #[test]
    fn strips_tuliprox_internal_prefix() {
        assert!(HopByHopHeader::is_sensitive("X-Tuliprox-Main-Revision", HeaderProtocol::Hls, None));
        assert!(HopByHopHeader::is_sensitive("x-tuliprox-debug", HeaderProtocol::MpegTs, None));
    }

    #[test]
    fn operator_config_layers_on_top_of_hardcoded_list() {
        let d = disabled();
        assert!(HopByHopHeader::is_sensitive("Referer", HeaderProtocol::Hls, Some(&d)));
        assert!(HopByHopHeader::is_sensitive("X-Blocked", HeaderProtocol::Hls, Some(&d)));
        assert!(HopByHopHeader::is_sensitive("CF-Ray", HeaderProtocol::Hls, Some(&d)));
        assert!(HopByHopHeader::is_sensitive("x-origin-secret", HeaderProtocol::Hls, Some(&d)));
    }

    #[test]
    fn passthrough_headers_are_not_sensitive() {
        assert!(!HopByHopHeader::is_sensitive("Accept-Language", HeaderProtocol::Hls, None));
        assert!(!HopByHopHeader::is_sensitive("Accept-Encoding", HeaderProtocol::Hls, None));
        assert!(!HopByHopHeader::is_sensitive("Content-Type", HeaderProtocol::MpegTs, None));
    }

    #[test]
    fn mpegts_protocol_path_with_operator_config() {
        // Without operator config, MpegTs falls through to the same hard-coded defaults.
        assert!(HopByHopHeader::is_sensitive("Authorization", HeaderProtocol::MpegTs, None));
        assert!(!HopByHopHeader::is_sensitive("X-Blocked", HeaderProtocol::MpegTs, None));

        // With operator config, the operator's list is honored.
        let d = disabled();
        assert!(HopByHopHeader::is_sensitive("X-Blocked", HeaderProtocol::MpegTs, Some(&d)));
    }
}
