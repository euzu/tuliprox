use crate::utils::{get_default_web_root, is_blank_or_default_web_root};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigApiDto {
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "is_blank_or_default_web_root")]
    pub web_root: String,
}

impl ConfigApiDto {
    pub fn default_web_root() -> String { get_default_web_root() }

    pub fn prepare(&mut self) {
        if self.web_root.trim().is_empty() {
            self.web_root = Self::default_web_root();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigApiDto;

    #[test]
    fn serialization_skips_default_web_root() {
        let dto = ConfigApiDto { host: "0.0.0.0".to_string(), port: 8901, web_root: "./web".to_string() };

        let serialized = serde_json::to_string(&dto).expect("api dto serialization should succeed");
        assert!(!serialized.contains("\"web_root\""), "expected default web_root to be skipped, got: {serialized}");
    }

    #[test]
    fn serialization_keeps_non_default_web_root() {
        let dto = ConfigApiDto { host: "0.0.0.0".to_string(), port: 8901, web_root: "/srv/tuliprox/web".to_string() };

        let serialized = serde_json::to_string(&dto).expect("api dto serialization should succeed");
        assert!(
            serialized.contains("\"web_root\""),
            "expected non-default web_root to be persisted, got: {serialized}"
        );
    }

    #[test]
    fn prepare_sets_default_web_root_when_empty() {
        let mut dto = ConfigApiDto { host: "0.0.0.0".to_string(), port: 8901, web_root: String::new() };
        let expected_web_root = ConfigApiDto::default_web_root();
        dto.prepare();
        assert_eq!(dto.web_root, expected_web_root);
    }
}
