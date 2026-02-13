pub fn default_geoip_url() -> String {
    String::from("https://raw.githubusercontent.com/sapics/ip-location-db/refs/heads/main/asn-country/asn-country-ipv4.csv")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GeoIpConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_geoip_url")]
    pub url: String,
}

impl GeoIpConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled && self.url.trim().is_empty()
    }
}
