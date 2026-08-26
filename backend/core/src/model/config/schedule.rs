use crate::model::macros;
use shared::model::{ScheduleConfigDto, ScheduleTaskType};

#[derive(Debug, Clone)]
pub struct ScheduleConfig {
    pub schedule: String,
    pub task_type: ScheduleTaskType,
    pub targets: Option<Vec<String>>,
}

macros::from_impl!(ScheduleConfig);
impl From<&ScheduleConfigDto> for ScheduleConfig {
    fn from(dto: &ScheduleConfigDto) -> Self {
        Self { schedule: dto.schedule.clone(), task_type: dto.task_type, targets: dto.targets.clone() }
    }
}
impl From<&ScheduleConfig> for ScheduleConfigDto {
    fn from(dto: &ScheduleConfig) -> Self {
        Self { schedule: dto.schedule.clone(), task_type: dto.task_type, targets: dto.targets.clone() }
    }
}
