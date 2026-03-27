use super::helpers::{format_bandwidth, format_transferred};
use crate::{hooks::use_service_context, model::EventMessage};
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
    } else {
        next.transferred_total_kb = next.transferred_total_kb.saturating_add(entry.total_kb);
    }

    next
}

#[component]
pub fn StreamMeterBadge(props: &StreamMeterBadgeProps) -> Html {
    let services = use_service_context();
    let meter_state = use_state(StreamMeterBadgeState::default);

    {
        let meter_state = meter_state.clone();
        let reset_key = props.uid;
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

    let label = match props.kind {
        MeterDisplayKind::Bandwidth => format_bandwidth(meter_state.rate_kbps),
        MeterDisplayKind::Transferred => {
            let total_kb = meter_state.transferred_total_kb.saturating_add(meter_state.current_meter_total_kb);
            format_transferred(total_kb)
        }
    };
    html! { <span>{label}</span> }
}
