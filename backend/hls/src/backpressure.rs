use super::HlsSession;

/// Coarse pressure level for scheduling live HLS origin work.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsBackpressureState {
    Normal,
    Degraded,
    Saturated,
}

impl HlsBackpressureState {
    pub const fn allows_prefetch(self) -> bool { matches!(self, Self::Normal) }

    pub const fn allows_new_demand_fetch(self) -> bool { !matches!(self, Self::Saturated) }
}

pub fn classify_hls_backpressure(
    session: &HlsSession,
    global_available_permits: usize,
    max_session_segment_fetches: usize,
) -> HlsBackpressureState {
    if global_available_permits == 0 || session.active_segment_fetches >= max_session_segment_fetches {
        return HlsBackpressureState::Saturated;
    }
    if global_available_permits == 1 || session.active_segment_fetches > 0 {
        return HlsBackpressureState::Degraded;
    }
    HlsBackpressureState::Normal
}

#[cfg(test)]
mod tests {
    use super::{classify_hls_backpressure, HlsBackpressureState};
    use crate::{HlsSession, HlsSessionKey};

    fn session() -> HlsSession { HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0) }

    #[test]
    fn classifies_saturated_when_global_slots_are_exhausted() {
        let session = session();

        assert_eq!(classify_hls_backpressure(&session, 0, 2), HlsBackpressureState::Saturated);
    }

    #[test]
    fn classifies_saturated_when_session_slots_are_exhausted() {
        let mut session = session();
        session.active_segment_fetches = 2;

        assert_eq!(classify_hls_backpressure(&session, 3, 2), HlsBackpressureState::Saturated);
    }

    #[test]
    fn classifies_degraded_when_capacity_is_low() {
        let session = session();

        assert_eq!(classify_hls_backpressure(&session, 1, 2), HlsBackpressureState::Degraded);
    }
}
