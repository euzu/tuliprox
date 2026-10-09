/// Terminal failure while admitting a playback request. Carried to the response layer so a
/// rejected request surfaces as a non-success HTTP response instead of an empty provider body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamAdmissionError {
    CleanupReceiverClosed,
    CleanupAdmissionTimeout,
    RegistrationRejected,
}

use tuliprox_session::ConnectionRejectionReason;

impl From<ConnectionRejectionReason> for StreamAdmissionError {
    fn from(reason: ConnectionRejectionReason) -> Self {
        match reason {
            ConnectionRejectionReason::CleanupReceiverClosed => Self::CleanupReceiverClosed,
            ConnectionRejectionReason::CleanupAdmissionTimeout => Self::CleanupAdmissionTimeout,
            ConnectionRejectionReason::RegistrationFailed => Self::RegistrationRejected,
        }
    }
}
