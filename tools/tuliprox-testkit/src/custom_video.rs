use crate::TestkitError;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

const PREFIX: &[u8] = b"TPX-CVS-";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomVideoKind {
    ChannelUnavailable,
    UserConnectionsExhausted,
    ProviderConnectionsExhausted,
    LowPriorityPreempted,
    UserAccountExpired,
    PanelApiProvisioning,
    HlsSessionOrLeaseExpired,
}

impl CustomVideoKind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "CHANNEL_UNAVAILABLE" => Some(Self::ChannelUnavailable),
            "USER_CONNECTIONS_EXHAUSTED" => Some(Self::UserConnectionsExhausted),
            "PROVIDER_CONNECTIONS_EXHAUSTED" => Some(Self::ProviderConnectionsExhausted),
            "LOW_PRIORITY_PREEMPTED" => Some(Self::LowPriorityPreempted),
            "USER_ACCOUNT_EXPIRED" => Some(Self::UserAccountExpired),
            "PANEL_API_PROVISIONING" => Some(Self::PanelApiProvisioning),
            "HLS_SESSION_OR_LEASE_EXPIRED" => Some(Self::HlsSessionOrLeaseExpired),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct WireMarkerScanner {
    retained: Vec<u8>,
}

impl WireMarkerScanner {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Option<CustomVideoKind>, TestkitError> {
        self.retained.extend_from_slice(bytes);
        if let Some(position) = self.retained.windows(PREFIX.len()).position(|window| window == PREFIX) {
            let value_start = position + PREFIX.len();
            let Some(value_end) =
                self.retained[value_start..].iter().position(|byte| !matches!(byte, b'A'..=b'Z' | b'_'))
            else {
                self.limit_buffer();
                return Ok(None);
            };
            let marker = std::str::from_utf8(&self.retained[value_start..value_start + value_end])
                .map_err(|error| TestkitError::Protocol(format!("invalid custom-video marker: {error}")))?;
            let kind = CustomVideoKind::parse(marker)
                .ok_or_else(|| TestkitError::Protocol(format!("unknown custom-video marker {marker}")))?;
            self.retained.clear();
            return Ok(Some(kind));
        }
        self.limit_buffer();
        Ok(None)
    }

    fn limit_buffer(&mut self) {
        const MAX_RETAINED: usize = 128;
        if self.retained.len() > MAX_RETAINED {
            let start = self.retained.len() - MAX_RETAINED;
            self.retained.drain(..start);
        }
    }
}

#[must_use]
pub fn fixture_digest(bytes: &[u8]) -> String { blake3::hash(bytes).to_hex().to_string() }

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureManifest {
    pub schema_version: u16,
    pub fixtures: BTreeMap<String, String>,
}

/// Verifies fixture installation bytes only.  It must never be used to classify
/// bytes observed after Tuliprox has processed a transport stream.
pub fn verify_fixture_manifest(root: &Path, manifest: &FixtureManifest) -> Result<(), TestkitError> {
    if manifest.schema_version != 1 {
        return Err(TestkitError::Configuration("unsupported custom-video fixture manifest schema".to_owned()));
    }
    for (relative_path, expected_digest) in &manifest.fixtures {
        let path = Path::new(relative_path);
        if path.is_absolute() || path.components().any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(TestkitError::Configuration(format!("unsafe fixture path {relative_path}")));
        }
        let bytes = std::fs::read(root.join(path))?;
        let actual_digest = fixture_digest(&bytes);
        if actual_digest != *expected_digest {
            return Err(TestkitError::Protocol(format!("fixture digest mismatch for {relative_path}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_marker_split_across_transport_chunks() {
        let mut scanner = WireMarkerScanner::default();
        assert!(scanner.push(b"ignored TPX-CVS-USER_").unwrap().is_none());
        assert_eq!(scanner.push(b"CONNECTIONS_EXHAUSTED;").unwrap(), Some(CustomVideoKind::UserConnectionsExhausted));
    }

    #[test]
    fn fixture_digest_is_deterministic() {
        assert_eq!(fixture_digest(b"fixture"), fixture_digest(b"fixture"));
    }

    #[test]
    fn checked_in_fixture_manifest_matches_installation_bytes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test/fixtures");
        let manifest_path = root.join("testkit/custom-video-fixtures.json");
        let manifest: FixtureManifest = serde_json::from_slice(&std::fs::read(manifest_path).unwrap()).unwrap();
        assert!(verify_fixture_manifest(&root, &manifest).is_ok());
    }
}
