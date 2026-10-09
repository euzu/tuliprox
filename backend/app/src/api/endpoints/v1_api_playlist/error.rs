#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManualUpdateEnqueueError {
    Busy,
    Unavailable,
}
