use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const SETTINGS_MAX_FILE_BYTES: usize = 256 * 1024;
pub const SETTINGS_MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const SETTINGS_MAX_TABLES: usize = 64;
pub const SETTINGS_MAX_COLUMNS: usize = 256;
pub const SETTINGS_MAX_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TableLayoutPreferencesDto {
    pub column_order: Vec<String>,
    pub column_visibility: BTreeMap<String, bool>,
}

pub fn settings_id_valid(id: &str) -> bool {
    !id.is_empty() && id.len() <= SETTINGS_MAX_ID_BYTES && !id.chars().any(char::is_control)
}

impl TableLayoutPreferencesDto {
    pub fn validate(&self, reject_duplicates: bool) -> Result<(), &'static str> {
        if self.column_order.len() > SETTINGS_MAX_COLUMNS || self.column_visibility.len() > SETTINGS_MAX_COLUMNS {
            return Err("settings_limits_exceeded");
        }
        let mut ids = BTreeSet::new();
        for id in &self.column_order {
            if !ids.insert(id) && reject_duplicates {
                return Err("settings_payload_invalid");
            }
        }
        ids.extend(self.column_visibility.keys());
        if ids.len() > SETTINGS_MAX_COLUMNS {
            return Err("settings_limits_exceeded");
        }
        if ids.iter().any(|id| !settings_id_valid(id)) {
            return Err("settings_id_invalid");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebUiPreferencesDto {
    pub tables: BTreeMap<String, TableLayoutPreferencesDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserPreferencesDto {
    pub web_ui: WebUiPreferencesDto,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserSettingsDto {
    pub preferences: UserPreferencesDto,
    pub section_etags: BTreeMap<String, String>,
    pub shared: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_ids_accept_new_names_and_enforce_byte_and_control_limits() {
        for id in ["future.inventory", "future.csv.schema", "playlist.accounts_csv.schema"] {
            assert!(settings_id_valid(id));
        }
        assert!(settings_id_valid(&"x".repeat(SETTINGS_MAX_ID_BYTES)));
        assert!(settings_id_valid(&"é".repeat(SETTINGS_MAX_ID_BYTES / 2)));
        assert!(!settings_id_valid(&"x".repeat(SETTINGS_MAX_ID_BYTES + 1)));
        assert!(!settings_id_valid(&"é".repeat(SETTINGS_MAX_ID_BYTES / 2 + 1)));
        for id in ["", "table\nname", "table\0name", "table\u{7f}name"] {
            assert!(!settings_id_valid(id));
        }
    }

    #[test]
    fn column_limits_count_distinct_ids_across_order_and_visibility() {
        let mut layout = TableLayoutPreferencesDto {
            column_order: (0..SETTINGS_MAX_COLUMNS).map(|i| format!("column_{i}")).collect(),
            ..Default::default()
        };
        layout.column_visibility = layout.column_order.iter().map(|id| (id.clone(), false)).collect();
        assert_eq!(layout.validate(true), Ok(()));
        layout.column_visibility.remove("column_0");
        layout.column_visibility.insert("future".into(), false);
        assert_eq!(layout.validate(true), Err("settings_limits_exceeded"));
        layout.column_order = vec!["future".into(), "future".into()];
        assert_eq!(layout.validate(true), Err("settings_payload_invalid"));
        assert_eq!(layout.validate(false), Ok(()));
        layout.column_order = vec!["".into(), "".into()];
        assert_eq!(layout.validate(true), Err("settings_payload_invalid"));
        assert_eq!(layout.validate(false), Err("settings_limits_exceeded"));
        layout.column_visibility.clear();
        assert_eq!(layout.validate(false), Err("settings_id_invalid"));
    }

    #[test]
    fn layout_contract_defaults_and_validation() -> Result<(), Box<dyn std::error::Error>> {
        let mut layout: TableLayoutPreferencesDto = serde_json::from_str("{}")?;
        assert_eq!(layout, TableLayoutPreferencesDto::default());
        layout.column_order = vec!["future".into(), "future".into()];
        assert!(layout.validate(true).is_err());
        assert!(layout.validate(false).is_ok());
        assert!(serde_json::from_str::<TableLayoutPreferencesDto>(r#"{"other":true}"#).is_err());
        Ok(())
    }
}
