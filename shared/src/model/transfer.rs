use super::recording::RecordingTaskDto;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TaskKindDto {
    Download,
    Recording,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RecordingTypeDto {
    Live,
    Vod,
    Series,
    #[default]
    LegacyDownload,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TaskPriorityDto {
    Background,
    Normal,
    High,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatusDto {
    Scheduled,
    Queued,
    WaitingForCapacity,
    RetryWaiting,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TransferTaskDto {
    pub id: String,
    pub title: String,
    pub kind: TaskKindDto,
    #[serde(default)]
    pub recording_type: RecordingTypeDto,
    pub priority: TaskPriorityDto,
    pub status: TransferStatusDto,
    pub retry_attempts: u8,
    pub downloaded_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_retry_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduled_start_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Present only when the task is a recording. Filters server-internal
    /// fields per the DVR design.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingTaskDto>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TransfersResponse {
    pub queue: Vec<TransferTaskDto>,
    pub finished: Vec<TransferTaskDto>,
    pub active: Vec<TransferTaskDto>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
// `TransfersDelta` is serialized over the WebSocket; the recording field
// inflates `TransferTaskDto` past clippy's `large_enum_variant` threshold
// even though `SnapshotReset` is bigger. Allow the lint here to keep the
// on-the-wire shape stable.
#[allow(clippy::large_enum_variant)]
#[serde(tag = "delta_type", rename_all = "snake_case")]
pub enum TransfersDelta {
    SnapshotReset(TransfersResponse),
    ActivePatched(TransferTaskDto),
    ActiveCleared,
    QueueReplaced { queue: Vec<TransferTaskDto> },
    FinishedReplaced { finished: Vec<TransferTaskDto> },
}

#[cfg(test)]
mod tests {
    use super::{
        RecordingTypeDto, TaskKindDto, TaskPriorityDto, TransferStatusDto, TransferTaskDto, TransfersDelta,
        TransfersResponse,
    };

    #[test]
    fn task_kind_serializes_as_snake_case() {
        let json = serde_json::to_string(&TaskKindDto::Recording).expect("serialize");
        assert_eq!(json, "\"recording\"");
    }

    #[test]
    fn task_priority_serializes_as_snake_case() {
        let json = serde_json::to_string(&TaskPriorityDto::High).expect("serialize");
        assert_eq!(json, "\"high\"");
    }

    #[test]
    fn transfer_status_serializes_as_snake_case() {
        let json = serde_json::to_string(&TransferStatusDto::WaitingForCapacity).expect("serialize");
        assert_eq!(json, "\"waiting_for_capacity\"");
    }

    fn make_test_task() -> TransferTaskDto {
        TransferTaskDto {
            id: "abc".to_string(),
            title: "Example".to_string(),
            kind: TaskKindDto::Download,
            recording_type: RecordingTypeDto::Vod,
            priority: TaskPriorityDto::Background,
            status: TransferStatusDto::Queued,
            retry_attempts: 0,
            downloaded_bytes: 123,
            total_bytes: Some(456),
            next_retry_at: None,
            scheduled_start_at: None,
            duration_secs: None,
            error: None,
            recording: None,
        }
    }

    #[test]
    fn transfer_task_round_trips() {
        let task = make_test_task();

        let json = serde_json::to_string(&task).expect("serialize");
        let decoded: TransferTaskDto = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, task);
    }

    #[test]
    fn legacy_transfer_without_recording_type_defaults_to_legacy_download() {
        let mut value = serde_json::to_value(make_test_task()).expect("serialize");
        value.as_object_mut().expect("object").remove("recording_type");
        let task: TransferTaskDto = serde_json::from_value(value).expect("deserialize");

        assert_eq!(task.recording_type, RecordingTypeDto::LegacyDownload);
    }

    #[test]
    fn transfers_response_round_trips() {
        let task = TransferTaskDto {
            kind: TaskKindDto::Recording,
            priority: TaskPriorityDto::Normal,
            status: TransferStatusDto::Scheduled,
            downloaded_bytes: 0,
            total_bytes: None,
            scheduled_start_at: Some(1_700_000_000),
            duration_secs: Some(5400),
            ..make_test_task()
        };
        let response = TransfersResponse { queue: vec![task.clone()], finished: Vec::new(), active: vec![task] };

        let json = serde_json::to_string(&response).expect("serialize");
        let decoded: TransfersResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, response);
    }

    #[test]
    fn transfers_delta_snapshot_reset_round_trips() {
        let response = TransfersResponse {
            queue: vec![TransferTaskDto {
                kind: TaskKindDto::Recording,
                priority: TaskPriorityDto::Normal,
                status: TransferStatusDto::Scheduled,
                downloaded_bytes: 0,
                total_bytes: None,
                scheduled_start_at: Some(1_700_000_000),
                duration_secs: Some(5400),
                ..make_test_task()
            }],
            finished: Vec::new(),
            active: Vec::new(),
        };

        let delta = TransfersDelta::SnapshotReset(response.clone());
        let json = serde_json::to_string(&delta).expect("serialize");
        let decoded: TransfersDelta = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, TransfersDelta::SnapshotReset(response));
    }

    #[test]
    fn transfers_delta_active_patched_round_trips() {
        let task = TransferTaskDto {
            status: TransferStatusDto::Running,
            retry_attempts: 2,
            next_retry_at: Some(1_700_000_100),
            error: Some("temporary".to_string()),
            ..make_test_task()
        };

        let delta = TransfersDelta::ActivePatched(task.clone());
        let json = serde_json::to_string(&delta).expect("serialize");
        let decoded: TransfersDelta = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, TransfersDelta::ActivePatched(task));
    }

    #[test]
    fn transfers_delta_queue_replaced_round_trips() {
        let task =
            TransferTaskDto { title: "Queued".to_string(), downloaded_bytes: 0, total_bytes: None, ..make_test_task() };

        let delta = TransfersDelta::QueueReplaced { queue: vec![task.clone()] };
        let json = serde_json::to_string(&delta).expect("serialize");
        let decoded: TransfersDelta = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, TransfersDelta::QueueReplaced { queue: vec![task] });
    }
}
