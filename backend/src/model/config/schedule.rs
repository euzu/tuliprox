use shared::model::ScheduleConfigDto;
use crate::model::macros;

#[derive(Debug, Clone)]
pub struct ScheduleConfig {
    pub schedule: String,
    pub targets: Option<Vec<String>>,
}

macros::from_impl!(ScheduleConfig);
impl From<&ScheduleConfigDto> for ScheduleConfig {
    fn from(dto: &ScheduleConfigDto) -> Self {
        Self {
            schedule: dto.schedule.to_string(),
            targets: dto.targets.clone(),
        }
    }
}
impl From<&ScheduleConfig> for ScheduleConfigDto {
    fn from(dto: &ScheduleConfig) -> Self {
        Self {
            schedule: dto.schedule.to_string(),
            targets: dto.targets.clone(),
        }
    }
}
