#[derive(Debug)]
pub(in crate::v3) enum PublishDatabaseError {
    NotPublished(io::Error),
    PublishedDurabilityUnknown(io::Error),
}

use std::io;

pub(super) fn invalid_data(message: &'static str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

pub(super) fn invalid_input(message: &'static str) -> io::Error { io::Error::new(io::ErrorKind::InvalidInput, message) }

impl PublishDatabaseError {
    pub(in crate::v3) const fn database_was_published(&self) -> bool {
        matches!(self, Self::PublishedDurabilityUnknown(_))
    }

    fn into_io(self) -> io::Error {
        match self {
            Self::NotPublished(error) | Self::PublishedDurabilityUnknown(error) => error,
        }
    }
}

impl From<PublishDatabaseError> for io::Error {
    fn from(error: PublishDatabaseError) -> Self { error.into_io() }
}
