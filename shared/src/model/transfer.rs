use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TaskKindDto {
    Download,
    Recording,
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
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TransfersResponse {
    pub queue: Vec<TransferTaskDto>,
    pub finished: Vec<TransferTaskDto>,
    pub active: Vec<TransferTaskDto>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
    use super::{TaskKindDto, TaskPriorityDto, TransferStatusDto, TransferTaskDto, TransfersDelta, TransfersResponse};

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

    #[test]
    fn transfer_task_round_trips() {
        let task = TransferTaskDto {
            id: "abc".to_string(),
            title: "Example".to_string(),
            kind: TaskKindDto::Download,
            priority: TaskPriorityDto::Background,
            status: TransferStatusDto::Queued,
            retry_attempts: 0,
            downloaded_bytes: 123,
            total_bytes: Some(456),
            next_retry_at: None,
            scheduled_start_at: None,
            duration_secs: None,
            error: None,
        };

        let json = serde_json::to_string(&task).expect("serialize");
        let decoded: TransferTaskDto = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, task);
    }

    #[test]
    fn transfers_response_round_trips() {
        let task = TransferTaskDto {
            id: "abc".to_string(),
            title: "Example".to_string(),
            kind: TaskKindDto::Recording,
            priority: TaskPriorityDto::Normal,
            status: TransferStatusDto::Scheduled,
            retry_attempts: 0,
            downloaded_bytes: 0,
            total_bytes: None,
            next_retry_at: None,
            scheduled_start_at: Some(1_700_000_000),
            duration_secs: Some(5400),
            error: None,
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
                id: "abc".to_string(),
                title: "Example".to_string(),
                kind: TaskKindDto::Recording,
                priority: TaskPriorityDto::Normal,
                status: TransferStatusDto::Scheduled,
                retry_attempts: 0,
                downloaded_bytes: 0,
                total_bytes: None,
                next_retry_at: None,
                scheduled_start_at: Some(1_700_000_000),
                duration_secs: Some(5400),
                error: None,
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
            id: "abc".to_string(),
            title: "Example".to_string(),
            kind: TaskKindDto::Download,
            priority: TaskPriorityDto::Background,
            status: TransferStatusDto::Running,
            retry_attempts: 2,
            downloaded_bytes: 123,
            total_bytes: Some(456),
            next_retry_at: Some(1_700_000_100),
            scheduled_start_at: None,
            duration_secs: None,
            error: Some("temporary".to_string()),
        };

        let delta = TransfersDelta::ActivePatched(task.clone());
        let json = serde_json::to_string(&delta).expect("serialize");
        let decoded: TransfersDelta = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, TransfersDelta::ActivePatched(task));
    }

    #[test]
    fn transfers_delta_queue_replaced_round_trips() {
        let task = TransferTaskDto {
            id: "abc".to_string(),
            title: "Queued".to_string(),
            kind: TaskKindDto::Download,
            priority: TaskPriorityDto::Background,
            status: TransferStatusDto::Queued,
            retry_attempts: 0,
            downloaded_bytes: 0,
            total_bytes: None,
            next_retry_at: None,
            scheduled_start_at: None,
            duration_secs: None,
            error: None,
        };

        let delta = TransfersDelta::QueueReplaced { queue: vec![task.clone()] };
        let json = serde_json::to_string(&delta).expect("serialize");
        let decoded: TransfersDelta = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, TransfersDelta::QueueReplaced { queue: vec![task] });
    }
}
