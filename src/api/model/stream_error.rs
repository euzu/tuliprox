use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

#[derive(Debug, Clone)]
pub enum StreamError {
    Reqwest(String),
    // StdIo(std::io::Error),
    // ReceiverClosed,
    ReceiverError(BroadcastStreamRecvError),
    LockError(String),
    Custom(String),
}

impl StreamError {
//     pub(crate) fn std_io(msg: String) -> Self {
//       StreamError::StdIo(std::io::Error::new(std::io::ErrorKind::Other, msg))
//     }
    pub fn reqwest(err: &reqwest::Error) -> Self {
        Self::Reqwest(err.to_string())
    }
}

impl std::error::Error for StreamError {}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::Reqwest(e) => write!(f, "Reqwest error: {e}"),
            // StreamError::StdIo(e) => write!(f, "IO error: {e}"),
            // StreamError::ReceiverClosed =>  write!(f, "Receiver closed"),
            StreamError::ReceiverError(e) =>  write!(f, "Receiver error {e}"),
            StreamError::LockError(e) =>  write!(f, "{e}"),
            StreamError::Custom(e) => write!(f, "Custom stream error: {e}"),
        }
    }
}