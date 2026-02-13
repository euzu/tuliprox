#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigApiDto {
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub web_root: String,
}

impl ConfigApiDto {
    pub fn prepare(&mut self) {
        if self.web_root.is_empty() {
            self.web_root = String::from("./web");
        }
    }
}
