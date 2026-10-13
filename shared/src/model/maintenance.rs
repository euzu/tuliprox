use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Path segment that addresses every known input at once.
pub const MAINTENANCE_WILDCARD: &str = "*";
/// Upper bound for a maintenance remark, in characters.
pub const MAINTENANCE_REASON_MAX_CHARS: usize = 500;

/// Why and since when an input is in maintenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputMaintenance {
    /// Unix timestamp in seconds; kept when the remark is updated.
    pub since: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Inputs in maintenance, keyed by canonical input name.
pub type MaintenanceEntries = BTreeMap<String, InputMaintenance>;

/// What the maintenance API returns after every call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceStateDto {
    pub entries: MaintenanceEntries,
    /// Canonical names of all configured inputs, for the selection in the dialog.
    pub inputs: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceRequestDto {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceReasonTooLong;

impl std::fmt::Display for MaintenanceReasonTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "maintenance reason exceeds {MAINTENANCE_REASON_MAX_CHARS} characters")
    }
}

impl std::error::Error for MaintenanceReasonTooLong {}

/// Trims the remark; an empty remark is no remark.
pub fn normalize_maintenance_reason(reason: Option<&str>) -> Result<Option<String>, MaintenanceReasonTooLong> {
    let Some(reason) = reason.map(str::trim).filter(|reason| !reason.is_empty()) else {
        return Ok(None);
    };
    if reason.chars().count() > MAINTENANCE_REASON_MAX_CHARS {
        return Err(MaintenanceReasonTooLong);
    }
    Ok(Some(reason.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_is_trimmed_and_empty_becomes_none() {
        assert_eq!(normalize_maintenance_reason(None), Ok(None));
        assert_eq!(normalize_maintenance_reason(Some("   ")), Ok(None));
        assert_eq!(normalize_maintenance_reason(Some("  down  ")), Ok(Some("down".to_string())));
    }

    #[test]
    fn reason_longer_than_limit_is_rejected() {
        let ok = "x".repeat(MAINTENANCE_REASON_MAX_CHARS);
        let too_long = "x".repeat(MAINTENANCE_REASON_MAX_CHARS + 1);
        assert_eq!(normalize_maintenance_reason(Some(&ok)), Ok(Some(ok.clone())));
        assert_eq!(normalize_maintenance_reason(Some(&too_long)), Err(MaintenanceReasonTooLong));
    }

    #[test]
    fn reason_limit_counts_characters_not_bytes() {
        let umlauts = "ä".repeat(MAINTENANCE_REASON_MAX_CHARS);
        assert!(normalize_maintenance_reason(Some(&umlauts)).is_ok());
    }

    #[test]
    fn state_dto_round_trips_as_json() {
        let mut entries = MaintenanceEntries::new();
        entries
            .insert("provider_a".to_string(), InputMaintenance { since: 1_760_350_000, reason: Some("down".into()) });
        let dto = MaintenanceStateDto { entries, inputs: vec!["provider_a".into(), "provider_b".into()] };
        let json = serde_json::to_string(&dto).unwrap();
        assert_eq!(
            json,
            r#"{"entries":{"provider_a":{"since":1760350000,"reason":"down"}},"inputs":["provider_a","provider_b"]}"#
        );
        assert_eq!(serde_json::from_str::<MaintenanceStateDto>(&json).unwrap(), dto);
    }

    #[test]
    fn missing_reason_deserializes_as_none() {
        let entry: InputMaintenance = serde_json::from_str(r#"{"since":1}"#).unwrap();
        assert_eq!(entry.reason, None);
    }
}
