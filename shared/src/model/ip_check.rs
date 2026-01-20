use crate::utils::is_blank_optional_string;

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct IpCheckDto {
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub ipv4: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub ipv6: Option<String>,
}