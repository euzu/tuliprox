use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TaskPriorityDto {
    Background,
    Normal,
    High,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatusDto {
    Scheduled,
    Queued,
    WaitingForCapacity,
    RetryWaiting,
    Running,
    Paused,
    /// Cancellation was requested and the worker has not finished tearing the
    /// transfer down yet. Not terminal: bytes may still be in flight.
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::{TaskPriorityDto, TransferStatusDto};

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
}
