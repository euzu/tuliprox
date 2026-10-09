#[cfg(any(test, feature = "test-support"))]
use super::CapacityRevision;
use super::{
    CacheCapacityPressure, HlsCacheCapacityError, HlsCacheCapacityReclaimOutcome, HlsCacheCapacityRevision,
    HlsCacheObjectLimitError,
};
#[cfg(any(test, feature = "test-support"))]
use std::sync::Arc;
use std::{fmt, io};

impl HlsCacheObjectLimitError {
    pub fn limit(&self) -> u64 { self.limit }
}

pub fn hls_cache_object_limit_from_io(error: &io::Error) -> Option<&HlsCacheObjectLimitError> {
    let mut source: &(dyn std::error::Error + 'static) = error.get_ref()?;
    loop {
        if let Some(limit_error) = source.downcast_ref::<HlsCacheObjectLimitError>() {
            return Some(limit_error);
        }
        source = source.source()?;
    }
}

pub(super) fn cache_object_limit_error(limit: u64) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, HlsCacheObjectLimitError { limit })
}

impl fmt::Debug for HlsCacheCapacityRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HlsCacheCapacityRevision(<opaque>)")
    }
}

impl HlsCacheCapacityRevision {
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test() -> Self { Self(Arc::new(CapacityRevision)) }
}

impl HlsCacheCapacityError {
    pub fn configured_session_bytes(&self) -> u64 { self.configured_session_bytes }

    pub fn configured_global_bytes(&self) -> u64 { self.configured_global_bytes }

    pub fn current_session_bytes(&self) -> u64 { self.current_session_bytes }

    pub fn current_global_bytes(&self) -> u64 { self.current_global_bytes }

    pub fn staged_bytes(&self) -> u64 { self.staged_bytes }

    pub fn required_session_bytes(&self) -> u64 { self.required_session_bytes }

    pub fn required_global_bytes(&self) -> u64 { self.required_global_bytes }

    pub fn revision(&self) -> &HlsCacheCapacityRevision { &self.revision }

    #[cfg(any(test, feature = "test-support"))]
    pub fn protected_working_set_bytes(&self) -> u64 { self.protected_working_set_bytes }

    #[cfg(any(test, feature = "test-support"))]
    pub fn reclaimable_bytes(&self) -> u64 { self.reclaimable_bytes }

    pub(super) fn pressure(&self) -> CacheCapacityPressure {
        CacheCapacityPressure {
            configured_session_bytes: self.configured_session_bytes,
            configured_global_bytes: self.configured_global_bytes,
            current_session_bytes: self.current_session_bytes,
            current_global_bytes: self.current_global_bytes,
            staged_bytes: self.staged_bytes,
            required_session_bytes: self.required_session_bytes,
            required_global_bytes: self.required_global_bytes,
        }
    }
}

pub fn hls_cache_capacity_from_io(error: &io::Error) -> Option<&HlsCacheCapacityError> {
    let mut source: &(dyn std::error::Error + 'static) = error.get_ref()?;
    loop {
        if let Some(capacity_error) = source.downcast_ref::<HlsCacheCapacityError>() {
            return Some(capacity_error);
        }
        source = source.source()?;
    }
}

pub(super) fn capacity_error(
    pressure: CacheCapacityPressure,
    reclaim: HlsCacheCapacityReclaimOutcome,
    revision: HlsCacheCapacityRevision,
) -> io::Error {
    io::Error::other(HlsCacheCapacityError {
        configured_session_bytes: pressure.configured_session_bytes,
        configured_global_bytes: pressure.configured_global_bytes,
        current_session_bytes: pressure.current_session_bytes,
        current_global_bytes: pressure.current_global_bytes,
        staged_bytes: pressure.staged_bytes,
        required_session_bytes: pressure.required_session_bytes,
        required_global_bytes: pressure.required_global_bytes,
        protected_working_set_bytes: reclaim.protected_working_set_bytes,
        reclaimable_bytes: reclaim.reclaimable_bytes,
        revision,
    })
}
