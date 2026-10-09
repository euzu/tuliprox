use super::QuietHours;

impl QuietHours {
    /// Parse `HH:MM-HH:MM`. Returns `None` for anything malformed, which
    /// config validation turns into an error the operator sees.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let (start, end) = raw.trim().split_once('-')?;
        Some(Self { start_min: parse_hhmm(start)?, end_min: parse_hhmm(end)? })
    }

    /// Is `minutes_since_midnight` inside the window?
    ///
    /// Handles the wrapping case (`23:00-07:00`), which is the one people
    /// actually configure.
    #[must_use]
    pub fn contains(&self, minutes_since_midnight: u16) -> bool {
        if self.start_min == self.end_min {
            // A zero-width window silences nothing. Treating it as "always"
            // would mute a channel completely on a typo.
            return false;
        }
        if self.start_min < self.end_min {
            (self.start_min..self.end_min).contains(&minutes_since_midnight)
        } else {
            minutes_since_midnight >= self.start_min || minutes_since_midnight < self.end_min
        }
    }

    /// Minutes from `minutes_since_midnight` until the window ends.
    #[must_use]
    pub fn minutes_until_end(&self, minutes_since_midnight: u16) -> u16 {
        if !self.contains(minutes_since_midnight) {
            return 0;
        }
        if self.end_min > minutes_since_midnight {
            self.end_min - minutes_since_midnight
        } else {
            (24 * 60 - minutes_since_midnight) + self.end_min
        }
    }
}

fn parse_hhmm(raw: &str) -> Option<u16> {
    let (h, m) = raw.trim().split_once(':')?;
    let h: u16 = h.trim().parse().ok()?;
    let m: u16 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}
