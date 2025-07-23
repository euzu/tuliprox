
#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct IpCheckDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<String>,
}