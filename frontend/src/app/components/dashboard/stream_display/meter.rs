use crate::{
    hooks::use_service_context,
    model::EventMessage,
    utils::{format_bandwidth, format_transferred},
};
use shared::model::StreamMeterEntry;
use yew::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MeterDisplayKind {
    Bandwidth,
    Transferred,
}

#[derive(Properties, PartialEq)]
pub struct StreamMeterBadgeProps {
    pub uid: u32,
    pub meter_uid: u32,
    pub kind: MeterDisplayKind,
}

#[derive(Clone, PartialEq, Eq, Default)]
struct StreamMeterBadgeState {
    rate_kbps: u32,
    transferred_total_kb: u32,
    current_meter_uid: u32,
    current_meter_total_kb: u32,
    has_sample: bool,
}

fn apply_stream_meter_entry(
    state: &StreamMeterBadgeState,
    current_meter_uid: u32,
    entry: &StreamMeterEntry,
) -> StreamMeterBadgeState {
    let mut next = state.clone();
    if entry.meter_uid == current_meter_uid {
        if next.current_meter_uid != current_meter_uid {
            next.transferred_total_kb = next.transferred_total_kb.saturating_add(next.current_meter_total_kb);
            next.current_meter_uid = current_meter_uid;
            next.current_meter_total_kb = 0;
        }
        next.current_meter_total_kb = next.current_meter_total_kb.max(entry.total_kb);
        next.rate_kbps = entry.rate_kbps;
        next.has_sample = true;
    } else {
        next.transferred_total_kb = next.transferred_total_kb.saturating_add(entry.total_kb);
    }

    next
}

fn format_meter_label(kind: MeterDisplayKind, state: &StreamMeterBadgeState) -> String {
    if !state.has_sample {
        return "-".to_string();
    }
    match kind {
        MeterDisplayKind::Bandwidth if state.rate_kbps == 0 => "0 KB/s".to_string(),
        MeterDisplayKind::Bandwidth => format_bandwidth(state.rate_kbps),
        MeterDisplayKind::Transferred => {
            let total_kb = state.transferred_total_kb.saturating_add(state.current_meter_total_kb);
            if total_kb == 0 {
                "0 KB".to_string()
            } else {
                format_transferred(total_kb)
            }
        }
    }
}

#[component]
pub fn StreamMeterBadge(props: &StreamMeterBadgeProps) -> Html {
    let services = use_service_context();
    let meter_state = use_state(StreamMeterBadgeState::default);

    {
        let meter_state = meter_state.clone();
        let reset_key = props.meter_uid;
        use_effect_with(reset_key, move |_| {
            meter_state.set(StreamMeterBadgeState::default());
            || ()
        });
    }

    {
        let services = services.clone();
        let meter_state = meter_state.clone();
        let listen_key = (props.uid, props.meter_uid);
        use_effect_with(listen_key, move |(uid, meter_uid)| {
            let uid = *uid;
            let meter_uid = *meter_uid;
            let subid = services.event.subscribe(move |msg| {
                if let EventMessage::StreamMeterBatch(entries) = msg {
                    let mut next_state = (*meter_state).clone();
                    let mut changed = false;
                    for entry in entries.iter().filter(|entry| entry.uids.contains(&uid)) {
                        let updated = apply_stream_meter_entry(&next_state, meter_uid, entry);
                        if updated != next_state {
                            next_state = updated;
                            changed = true;
                        }
                    }
                    if changed {
                        meter_state.set(next_state);
                    }
                }
            });
            move || services.event.unsubscribe(subid)
        });
    }

    let label = format_meter_label(props.kind, &meter_state);
    html! { <span>{label}</span> }
}

#[cfg(test)]
mod tests {
    use super::{apply_stream_meter_entry, format_meter_label, MeterDisplayKind, StreamMeterBadgeState};
    use shared::model::StreamMeterEntry;

    fn entry(meter_uid: u32, rate_kbps: u32, total_kb: u32, uids: Vec<u32>) -> StreamMeterEntry {
        StreamMeterEntry { meter_uid, rate_kbps, total_kb, uids }
    }

    #[test]
    fn meter_label_shows_dash_before_first_sample() {
        let state = StreamMeterBadgeState::default();

        assert_eq!(format_meter_label(MeterDisplayKind::Bandwidth, &state), "-");
        assert_eq!(format_meter_label(MeterDisplayKind::Transferred, &state), "-");
    }

    #[test]
    fn meter_label_shows_zero_after_first_empty_sample() {
        let state = apply_stream_meter_entry(&StreamMeterBadgeState::default(), 7, &entry(7, 0, 0, vec![1]));

        assert_eq!(format_meter_label(MeterDisplayKind::Bandwidth, &state), "0 KB/s");
        assert_eq!(format_meter_label(MeterDisplayKind::Transferred, &state), "0 KB");
    }

    #[test]
    fn same_meter_keeps_transferred_total_across_stream_uid_changes() {
        let state = apply_stream_meter_entry(&StreamMeterBadgeState::default(), 7, &entry(7, 120, 256, vec![1]));
        let state = apply_stream_meter_entry(&state, 7, &entry(7, 0, 256, vec![2]));

        assert_eq!(state.current_meter_uid, 7);
        assert_eq!(state.current_meter_total_kb, 256);
        assert_eq!(state.transferred_total_kb, 0);
        assert_eq!(format_meter_label(MeterDisplayKind::Transferred, &state), "256 KB");
    }
}
