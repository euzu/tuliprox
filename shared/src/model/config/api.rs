#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ConfigApiDto {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub web_root: String,
}

impl ConfigApiDto {
    pub fn prepare(&mut self) {
        if self.web_root.is_empty() {
            self.web_root = String::from("./web");
        }
    }
}