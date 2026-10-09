/// Reason a persisted entry cannot be converted back to its in-memory form
/// during the commit step. Surfaced to the caller so a corrupt persisted file
/// fails closed instead of silently dropping entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistedError {
    /// The persisted URL could not be parsed.
    InvalidUrl(String),
}

impl std::fmt::Display for PersistedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(s) => write!(f, "persisted url is invalid: {s}"),
        }
    }
}

impl std::error::Error for PersistedError {}

/// Typed error returned from the queue mutation boundary. Every
/// `mutate` closure that fails must return a known variant; the
/// `Other` variant is an escape hatch for dynamically-formatted
/// messages that have no stable wire code.
#[derive(Debug)]
pub enum QueueMutationError {
    UnknownRecording,
    StateNotEditable,
    Forbidden,
    InvalidInterval,
    InvalidQuotaPool,
    InvalidPath,
    PaddingLimitExceeded,
    QuotaExceeded,
    Duplicate,
    NotInTerminalState,
    DiskFull,
    MutationSkipped,
    /// The idempotency key was accepted before, for the same request.
    IdempotentReplay {
        recording_id: String,
    },
    /// The idempotency key was accepted before, for a different request.
    IdempotencyConflict,
    /// Escape hatch for dynamically-formatted validation messages
    /// that have no stable wire code. Prefer the typed variants.
    Other(String),
    Io(std::io::Error),
}

impl QueueMutationError {
    /// Escape-hatch constructor for messages that cannot be expressed
    /// as a typed variant. Prefer the typed `Self::X` constructors.
    pub fn new(message: impl Into<String>) -> Self { Self::Other(message.into()) }

    pub fn from_io(err: std::io::Error) -> Self { Self::Io(err) }

    /// Stable display message for logging and HTTP error rendering.
    pub fn message(&self) -> &'static str {
        match self {
            Self::UnknownRecording => "recording unknown",
            Self::StateNotEditable => "recording state not editable",
            Self::Forbidden => "recording forbidden",
            Self::InvalidInterval => "recording invalid interval",
            Self::InvalidQuotaPool => "recording invalid quota pool",
            Self::InvalidPath => "recording invalid path",
            Self::PaddingLimitExceeded => "recording_padding_limit_exceeded",
            Self::QuotaExceeded => "recording quota exceeded",
            Self::Duplicate => "recording duplicate",
            Self::NotInTerminalState => "recording not in terminal state",
            Self::DiskFull => "disk full",
            Self::MutationSkipped => "mutation unexpectedly skipped",
            Self::IdempotentReplay { .. } => "recording idempotent replay",
            Self::IdempotencyConflict => "recording idempotency conflict",
            Self::Other(_) => "queue mutation failed",
            Self::Io(_) => "queue mutation persistence failed",
        }
    }

    pub fn source_io(&self) -> Option<&std::io::Error> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl std::fmt::Display for QueueMutationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Other(s) => f.write_str(s),
            Self::Io(e) => std::fmt::Display::fmt(e, f),
            other => f.write_str(other.message()),
        }
    }
}

impl std::error::Error for QueueMutationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err as &(dyn std::error::Error + 'static)),
            _ => None,
        }
    }
}
