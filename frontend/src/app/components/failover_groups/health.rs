//! Display helpers for the health columns of the member table.

const NO_VALUE: &str = "\u{2014}";

/// Bitrate as megabits with one decimal (`6.1M`), kilobits below one megabit.
pub fn bitrate_label(bitrate_kbps: Option<u32>) -> Option<String> {
    bitrate_kbps.filter(|kbps| *kbps > 0).map(|kbps| {
        if kbps >= 1000 {
            format!("{}.{}M", kbps / 1000, (kbps % 1000) / 100)
        } else {
            format!("{kbps}k")
        }
    })
}

/// `24h/7d` quality of service of a member.
pub fn qos_label(qos_24h: Option<u8>, qos_7d: Option<u8>) -> String {
    match (qos_24h, qos_7d) {
        (None, None) => NO_VALUE.to_string(),
        (day, week) => {
            let part = |value: Option<u8>| value.map_or_else(|| NO_VALUE.to_string(), |value| value.to_string());
            format!("{}/{}", part(day), part(week))
        }
    }
}

/// Connections in use over the connection limit of the member's input; no limit shows a dash.
pub fn slots_label(used: u32, max: u32) -> String {
    if max == 0 {
        format!("{used}/{NO_VALUE}")
    } else {
        format!("{used}/{max}")
    }
}

/// Dot colour class of a member: `open` when the member or its input is shut off, `degraded` while failures are
/// being counted, `ok` otherwise.
pub fn health_class(circuit: &str, input_circuit: &str, failure_streak: u32) -> &'static str {
    let is_open = |state: &str| matches!(state, "open" | "half_open");
    if is_open(circuit) || is_open(input_circuit) {
        "open"
    } else if circuit == "counting" || input_circuit == "counting" || failure_streak > 0 {
        "degraded"
    } else {
        "ok"
    }
}

pub fn circuit_label_key(circuit: &str) -> &'static str {
    match circuit {
        "counting" => "LABEL.FAILOVER_GROUPS_HEALTH_COUNTING",
        "open" => "LABEL.FAILOVER_GROUPS_HEALTH_OPEN",
        "half_open" => "LABEL.FAILOVER_GROUPS_HEALTH_HALF_OPEN",
        _ => "LABEL.FAILOVER_GROUPS_HEALTH_CLOSED",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_is_shown_in_megabits_with_one_decimal() {
        assert_eq!(bitrate_label(Some(6100)).as_deref(), Some("6.1M"));
        assert_eq!(bitrate_label(Some(3249)).as_deref(), Some("3.2M"));
        assert_eq!(bitrate_label(Some(800)).as_deref(), Some("800k"));
        assert_eq!(bitrate_label(Some(0)), None);
        assert_eq!(bitrate_label(None), None);
    }

    #[test]
    fn qos_label_shows_both_windows_or_a_dash() {
        assert_eq!(qos_label(Some(92), Some(80)), "92/80");
        assert_eq!(qos_label(Some(92), None), "92/\u{2014}");
        assert_eq!(qos_label(None, None), "\u{2014}");
    }

    #[test]
    fn slots_without_limit_show_a_dash() {
        assert_eq!(slots_label(2, 3), "2/3");
        assert_eq!(slots_label(1, 0), "1/\u{2014}");
    }

    #[test]
    fn health_class_ranks_open_above_degraded() {
        assert_eq!(health_class("closed", "closed", 0), "ok");
        assert_eq!(health_class("counting", "closed", 0), "degraded");
        assert_eq!(health_class("closed", "closed", 2), "degraded");
        assert_eq!(health_class("open", "closed", 0), "open");
        assert_eq!(health_class("closed", "half_open", 0), "open");
        assert_eq!(health_class("counting", "open", 3), "open");
    }

    #[test]
    fn circuit_labels_fall_back_to_closed() {
        assert_eq!(circuit_label_key("half_open"), "LABEL.FAILOVER_GROUPS_HEALTH_HALF_OPEN");
        assert_eq!(circuit_label_key("unknown"), "LABEL.FAILOVER_GROUPS_HEALTH_CLOSED");
    }
}
