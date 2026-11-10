/// Answer text of a manual playlist update that finds the queue full.
pub(in crate::api) const QUEUE_BUSY_MESSAGE: &str = "Another playlist update is already queued; retry this request";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::api) enum ManualUpdateEnqueueError {
    Busy,
    Unavailable,
}
