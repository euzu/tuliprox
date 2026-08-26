use shared::model::ConfigApiDto;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HealthcheckConfig {
    pub api: ConfigApiDto,
}
