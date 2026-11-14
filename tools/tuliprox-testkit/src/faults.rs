use crate::TestkitError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Category of a parameterised origin fault. Used by the control API body and by the
/// scenario `origin_fault` step so both surfaces name the same kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginFaultKind {
    HlsManifestStatus,
    HlsSegmentStatus,
    SlowSegmentBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginFault {
    CloseBeforeFirstByte,
    CloseAfterBytes(u64),
    StallAfterBytes(u64),
    CorruptNextFrame,
    UnexpectedMarker(u32),
    /// Answer every `index.m3u8` request of `account` with `status`.
    HlsManifestStatus {
        account: String,
        status: u16,
    },
    /// Answer every `.ts` segment request of `account` with `status`; manifests stay healthy.
    HlsSegmentStatus {
        account: String,
        status: u16,
    },
    /// Deliver segment bodies of `account` at `bytes_per_second`.
    SlowSegmentBody {
        account: String,
        bytes_per_second: u64,
    },
}

impl OriginFault {
    /// Build a parameterised fault from its control-plane or YAML parts, rejecting invalid
    /// parameters before they reach the origin handlers.
    pub fn from_parts(
        kind: OriginFaultKind,
        account: String,
        status: Option<u16>,
        bytes_per_second: Option<u64>,
    ) -> Result<Self, TestkitError> {
        if account.is_empty() {
            return Err(TestkitError::Configuration("origin fault account cannot be empty".to_owned()));
        }
        match kind {
            OriginFaultKind::HlsManifestStatus => {
                Ok(Self::HlsManifestStatus { account, status: require_status(status)? })
            }
            OriginFaultKind::HlsSegmentStatus => {
                Ok(Self::HlsSegmentStatus { account, status: require_status(status)? })
            }
            OriginFaultKind::SlowSegmentBody => {
                let bytes_per_second = bytes_per_second.ok_or_else(|| {
                    TestkitError::Configuration("slow segment body fault requires bytes_per_second".to_owned())
                })?;
                if bytes_per_second == 0 {
                    return Err(TestkitError::Configuration(
                        "slow segment body fault rate must be positive".to_owned(),
                    ));
                }
                Ok(Self::SlowSegmentBody { account, bytes_per_second })
            }
        }
    }
}

fn require_status(status: Option<u16>) -> Result<u16, TestkitError> {
    let status = status.ok_or_else(|| TestkitError::Configuration("origin fault status is required".to_owned()))?;
    if !(100..=599).contains(&status) {
        return Err(TestkitError::Configuration(format!("origin fault status {status} is not a valid HTTP status")));
    }
    Ok(status)
}

#[derive(Debug, Default)]
pub struct FaultSchedule {
    faults: HashMap<String, OriginFault>,
}

impl FaultSchedule {
    pub fn insert(&mut self, fault_id: String, fault: OriginFault) -> Result<(), TestkitError> {
        if self.faults.contains_key(&fault_id) {
            return Err(TestkitError::Configuration(format!("fault ID {fault_id} already exists")));
        }
        self.faults.insert(fault_id, fault);
        Ok(())
    }

    /// Consumes a one-shot fault. Parameterised per-account faults are not consumed by
    /// matching and must be removed with [`Self::clear`].
    pub fn take(&mut self, fault_id: &str) -> Option<OriginFault> { self.faults.remove(fault_id) }

    pub fn clear(&mut self, fault_id: &str) -> Result<(), TestkitError> {
        self.faults
            .remove(fault_id)
            .map(|_| ())
            .ok_or_else(|| TestkitError::Configuration(format!("fault ID {fault_id} does not exist")))
    }

    pub fn clear_all(&mut self) { self.faults.clear(); }

    /// Status applied to `index.m3u8` requests of `account`, if any persistent fault matches.
    pub fn manifest_status_for(&self, account: &str) -> Option<u16> {
        self.faults.values().find_map(|fault| match fault {
            OriginFault::HlsManifestStatus { account: fault_account, status } if fault_account == account => {
                Some(*status)
            }
            _ => None,
        })
    }

    /// Status applied to `.ts` segment requests of `account`, if any persistent fault matches.
    pub fn segment_status_for(&self, account: &str) -> Option<u16> {
        self.faults.values().find_map(|fault| match fault {
            OriginFault::HlsSegmentStatus { account: fault_account, status } if fault_account == account => {
                Some(*status)
            }
            _ => None,
        })
    }

    /// Delivery rate applied to segment bodies of `account`, if any persistent fault matches.
    pub fn segment_rate_for(&self, account: &str) -> Option<u64> {
        self.faults.values().find_map(|fault| match fault {
            OriginFault::SlowSegmentBody { account: fault_account, bytes_per_second } if fault_account == account => {
                Some(*bytes_per_second)
            }
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fault_is_consumed_only_once() {
        let mut schedule = FaultSchedule::default();
        schedule.insert("close".to_owned(), OriginFault::CloseBeforeFirstByte).unwrap();
        assert_eq!(schedule.take("close"), Some(OriginFault::CloseBeforeFirstByte));
        assert_eq!(schedule.take("close"), None);
    }

    #[test]
    fn parameterised_faults_persist_until_cleared() {
        let mut schedule = FaultSchedule::default();
        schedule
            .insert(
                "a-manifest".to_owned(),
                OriginFault::HlsManifestStatus { account: "input-a".to_owned(), status: 404 },
            )
            .unwrap();

        assert_eq!(schedule.manifest_status_for("input-a"), Some(404));
        assert_eq!(schedule.manifest_status_for("input-a"), Some(404));
        assert_eq!(
            schedule.take("a-manifest"),
            Some(OriginFault::HlsManifestStatus { account: "input-a".to_owned(), status: 404 })
        );
    }

    #[test]
    fn per_account_matching_does_not_leak_across_accounts() {
        let mut schedule = FaultSchedule::default();
        schedule
            .insert(
                "a-manifest".to_owned(),
                OriginFault::HlsManifestStatus { account: "input-a".to_owned(), status: 404 },
            )
            .unwrap();
        schedule
            .insert(
                "a-segment".to_owned(),
                OriginFault::HlsSegmentStatus { account: "input-a".to_owned(), status: 503 },
            )
            .unwrap();
        schedule
            .insert(
                "a-slow".to_owned(),
                OriginFault::SlowSegmentBody { account: "input-a".to_owned(), bytes_per_second: 8 },
            )
            .unwrap();

        assert_eq!(schedule.manifest_status_for("input-b"), None);
        assert_eq!(schedule.segment_status_for("input-b"), None);
        assert_eq!(schedule.segment_rate_for("input-b"), None);
        assert_eq!(schedule.manifest_status_for("input-a"), Some(404));
        assert_eq!(schedule.segment_status_for("input-a"), Some(503));
        assert_eq!(schedule.segment_rate_for("input-a"), Some(8));
    }

    #[test]
    fn clearing_a_fault_removes_its_matches() {
        let mut schedule = FaultSchedule::default();
        schedule
            .insert(
                "a-manifest".to_owned(),
                OriginFault::HlsManifestStatus { account: "input-a".to_owned(), status: 404 },
            )
            .unwrap();
        schedule.clear("a-manifest").unwrap();
        assert_eq!(schedule.manifest_status_for("input-a"), None);
        assert!(schedule.clear("a-manifest").is_err());
    }

    #[test]
    fn from_parts_validates_parameters() {
        assert!(matches!(
            OriginFault::from_parts(OriginFaultKind::HlsManifestStatus, "input-a".to_owned(), Some(404), None),
            Ok(OriginFault::HlsManifestStatus { status: 404, .. })
        ));
        assert!(matches!(
            OriginFault::from_parts(OriginFaultKind::HlsSegmentStatus, "input-a".to_owned(), Some(503), None),
            Ok(OriginFault::HlsSegmentStatus { status: 503, .. })
        ));
        assert!(matches!(
            OriginFault::from_parts(OriginFaultKind::SlowSegmentBody, "input-a".to_owned(), None, Some(8)),
            Ok(OriginFault::SlowSegmentBody { bytes_per_second: 8, .. })
        ));

        assert!(OriginFault::from_parts(OriginFaultKind::HlsManifestStatus, String::new(), Some(404), None).is_err());
        assert!(OriginFault::from_parts(OriginFaultKind::HlsManifestStatus, "input-a".to_owned(), None, None).is_err());
        assert!(
            OriginFault::from_parts(OriginFaultKind::HlsManifestStatus, "input-a".to_owned(), Some(99), None).is_err()
        );
        assert!(
            OriginFault::from_parts(OriginFaultKind::HlsManifestStatus, "input-a".to_owned(), Some(600), None).is_err()
        );
        assert!(OriginFault::from_parts(OriginFaultKind::SlowSegmentBody, "input-a".to_owned(), None, None).is_err());
        assert!(OriginFault::from_parts(OriginFaultKind::SlowSegmentBody, "input-a".to_owned(), None, Some(0)).is_err());
    }
}
