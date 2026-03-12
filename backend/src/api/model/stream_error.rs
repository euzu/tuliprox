use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

#[derive(Debug, Clone)]
pub enum StreamError {
    Reqwest(String),
    StdIo(String),
    // ReceiverClosed,
    ReceiverError(BroadcastStreamRecvError),
    LockError(String),
    Stream(String),
    MalformedPacket(String),
    InvalidTimestamp(String),
    SyncLoss(String),
}

impl StreamError {
    //     pub(crate) fn std_io(msg: String) -> Self {
    //       StreamError::StdIo(std::io::Error::new(std::io::ErrorKind::Other, msg))
    //     }
    pub fn reqwest(err: &reqwest::Error) -> Self { Self::Reqwest(err.to_string()) }
}

impl std::error::Error for StreamError {}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::Reqwest(e) => write!(f, "Reqwest error: {e}"),
            StreamError::StdIo(e) => write!(f, "IO error: {e}"),
            // StreamError::ReceiverClosed =>  write!(f, "Receiver closed"),
            StreamError::ReceiverError(e) => write!(f, "Receiver error {e}"),
            StreamError::Stream(e)
            | StreamError::LockError(e)
            | StreamError::MalformedPacket(e)
            | StreamError::InvalidTimestamp(e)
            | StreamError::SyncLoss(e) => write!(f, "{e}"),
        }
    }
}
