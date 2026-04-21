use super::{
    helpers::{build_technical_chips, is_background_transfer_stream, render_cluster},
    meter::{MeterDisplayKind, StreamMeterBadge},
};
use crate::{
    app::components::{country::display_country_code, AppIcon, Chip, Country, RevealContent, ToggleSwitch},
    i18n::use_translation,
    services::PlaylistService,
    utils::format_duration,
};
use shared::{
    model::{EpgProgrammeDto, StreamEpgItemRequest, StreamEpgResponse, StreamInfo},
    utils::{current_time_secs, strip_port},
};
use std::rc::Rc;
use wasm_bindgen_futures::spawn_local;
use web_sys::MouseEvent;
use yew::prelude::*;

#[derive(Properties, PartialEq, Clone)]
pub struct StreamDisplayItemProps {
    pub stream: Rc<StreamInfo>,
    pub user_comment: Option<String>,
    pub metrics_enabled: bool,
    pub on_popup_click: Callback<(Rc<StreamInfo>, MouseEvent)>,
}

/// Holds the fetched EPG data and its fetch timestamp for staleness checking.
#[derive(Debug, Clone)]
struct EpgData {
    response: StreamEpgResponse,
    fetched_at_secs: u64,
}

impl EpgData {
    fn is_valid(&self, now_secs: u64) -> bool {
        // Valid for 7 hours after fetch (window is 8h, refresh when < 1h remaining)
        let age = now_secs.saturating_sub(self.fetched_at_secs);
        age < 7 * 3600
    }

    fn is_near_expiry(&self, now_secs: u64) -> bool {
        let age = now_secs.saturating_sub(self.fetched_at_secs);
        age >= 7 * 3600
    }
}

/// Computes current and next programme from a list of programmes.
fn compute_current_next(
    programmes: &[EpgProgrammeDto],
    now_secs: i64,
) -> (Option<EpgProgrammeDto>, Option<EpgProgrammeDto>) {
    let current = programmes.iter().find(|p| now_secs >= p.start_timestamp && now_secs < p.stop_timestamp).cloned();

    let next =
        current.as_ref().and_then(|cur| programmes.iter().find(|p| p.start_timestamp > cur.start_timestamp).cloned());

    (current, next)
}

#[component]
pub fn StreamDisplayItem(props: &StreamDisplayItemProps) -> Html {
    let translate = use_translation();
    let stream = props.stream.clone();
    let is_background_transfer = is_background_transfer_stream(&stream);
    let chips = build_technical_chips(stream.channel.item_type, stream.channel.technical.as_ref());
    let client_ip = strip_port(&stream.client_ip).to_string();

    let epg_channel_id = stream.channel.epg_channel_id.clone();
    let epg_target_id = stream.channel.target_id;

    // EPG state: cached response + fetched timestamp
    let epg_data: UseStateHandle<Option<Rc<EpgData>>> = use_state(|| None);
    // Computed current/next for display
    let current_next: UseStateHandle<Option<(EpgProgrammeDto, Option<EpgProgrammeDto>)>> = use_state(|| None);
    // Refetch trigger: set to true when cache is stale, causes fetch effect to re-run
    let needs_refetch: UseStateHandle<bool> = use_state(|| false);

    // Fetch EPG when epg_channel_id is present or when needs_refetch is triggered
    {
        let epg_channel_id = epg_channel_id.clone();
        let epg_data = epg_data.clone();
        let current_next = current_next.clone();
        let needs_refetch = needs_refetch.clone();
        use_effect_with((epg_channel_id.clone(), *needs_refetch), move |(epg_channel_id, force_refetch)| {
            let Some(epg_channel_id) = epg_channel_id.clone() else {
                return;
            };
            let force_refetch = *force_refetch;

            let epg_channel_id = epg_channel_id.clone();
            let epg_data = epg_data.clone();
            let current_next = current_next.clone();
            let needs_refetch = needs_refetch.clone();

            spawn_local(async move {
                let now_secs = current_time_secs();

                // Check if cache is valid before fetching
                if let Some(ref data) = *epg_data {
                    if !force_refetch && data.is_valid(now_secs) {
                        return;
                    }
                }

                // Fetch fresh data
                let service = PlaylistService::new();
                if let Some(response) = service
                    .get_stream_epg(vec![StreamEpgItemRequest {
                        epg_channel_id: epg_channel_id.to_string(),
                        target_id: Some(epg_target_id),
                    }])
                    .await
                {
                    let new_data = Rc::new(EpgData { response, fetched_at_secs: current_time_secs() });

                    // Compute current/next from the fetched data
                    let now_i64 = current_time_secs() as i64;
                    if let Some(entry) = new_data.response.entries.first() {
                        let (current, next) = compute_current_next(&entry.programmes, now_i64);
                        // Only show current programme if one is active; clears stale state otherwise
                        current_next.set(current.map(|cur| (cur, next)));
                    } else {
                        current_next.set(None);
                    }

                    epg_data.set(Some(new_data));
                    if force_refetch {
                        needs_refetch.set(false);
                    }
                }
            });
        });
    }

    // Local tick: recompute current/next and detect staleness every 30 seconds
    {
        let epg_channel_id = epg_channel_id.clone();
        let epg_data = epg_data.clone();
        let current_next = current_next.clone();
        let needs_refetch = needs_refetch.clone();
        use_effect_with(epg_channel_id.clone(), move |epg_channel_id| {
            epg_channel_id.as_ref().map_or_else(
                || Box::new(|| ()) as Box<dyn FnOnce()>,
                |_| {
                    let epg_data = epg_data.clone();
                    let current_next = current_next.clone();
                    let needs_refetch = needs_refetch.clone();

                    let interval = gloo_timers::callback::Interval::new(30_000, move || {
                        let epg_data = epg_data.clone();
                        let current_next = current_next.clone();
                        let needs_refetch = needs_refetch.clone();

                        let now_secs = current_time_secs();

                        // Check if cache is stale and needs refetch
                        if let Some(data) = (*epg_data).as_ref() {
                            if data.is_near_expiry(now_secs) {
                                needs_refetch.set(true);
                                return;
                            }
                        }

                        // Recompute current/next from cached data
                        if let Some(data) = (*epg_data).as_ref() {
                            if let Some(entry) = data.response.entries.first() {
                                let now_i64 = now_secs as i64;
                                let (current, next) = compute_current_next(&entry.programmes, now_i64);
                                // Only show current if one is active; clears stale state otherwise
                                current_next.set(current.map(|cur| (cur, next)));
                            } else {
                                current_next.set(None);
                            }
                        }
                    });
                    Box::new(move || drop(interval)) as Box<dyn FnOnce()>
                },
            )
        });
    }

    let handle_popup_click = {
        let stream = stream.clone();
        let on_popup_click = props.on_popup_click.clone();
        Callback::from(move |event: MouseEvent| on_popup_click.emit((stream.clone(), event)))
    };

    html! {
        <article class="tp__stream-display__item">
            if !is_background_transfer {
                <button class="tp__stream-display__menu" onclick={handle_popup_click}>
                    <AppIcon name="Handle" />
                </button>
            }
        <div class="tp__stream-display__item-content">
            <div class="tp__stream-display__item-head">
                <div class="tp__stream-display__identity">
                    <div class="tp__stream-display__title-row">
                        <div class="tp__stream-display__title-block">
                            <div class="tp__stream-display__title">{stream.channel.title.to_string()}</div>
                            <div class="tp__stream-display__subtitle">
                                <span class="tp__stream-display__channel_type">{render_cluster(&stream.channel)}</span>
                                {" • "}
                                <span class="tp__stream-display__provider">{stream.provider.clone()}</span>
                                {" • "}
                                <span class="tp__stream-display__username"> {stream.username.clone()}</span>
                                <span class="tp__stream-display__user-comment"> {props.user_comment.clone().map(|c| format!("({c})"))}</span>
                            </div>
                            if let Some((current, _)) = current_next.as_ref() {
                                if !current.title.is_empty() {
                                    <div class="tp__stream-display__epg">
                                        <span class="tp__stream-display__epg-now">
                                            <span class="tp__stream-display__epg-time">{&current.start}</span>
                                            {" "}
                                            <span class="tp__stream-display__epg-title">{&current.title}</span>
                                        </span>
                                        if let Some(next) = current_next.as_ref().and_then(|(_, n)| n.as_ref()) {
                                            <span class="tp__stream-display__epg-next">
                                                {" → "}
                                                <span class="tp__stream-display__epg-time">{&next.start}</span>
                                                {" "}
                                                <span class="tp__stream-display__epg-title">{&next.title}</span>
                                            </span>
                                        }
                                    </div>
                                }
                            }
                        </div>
                    </div>
                </div>
           </div>
           <div class="tp__stream-display__row">
            <div class="tp__stream-display__stats">
                    <div class="tp__stream-display__stat tp__stream-display__stat--category">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.GROUP")}</span>
                        <span class="tp__stream-display__stat-value">{stream.channel.group.to_string()}</span>
                    </div>
                    <div class="tp__stream-display__stat tp__stream-display__stat--client">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.CLIENT_IP")}</span>
                        <span class="tp__stream-display__stat-value tp__stream-display__stat-value--ip">{client_ip.clone()}</span>
                    </div>
                    if display_country_code(stream.country_code.as_deref()).is_some() {
                        <div class="tp__stream-display__stat tp__stream-display__stat--country">
                            <span class="tp__stream-display__stat-label">{translate.t("LABEL.COUNTRY")}</span>
                            <span class="tp__stream-display__stat-value">
                                <Country country_code={stream.country_code.clone()}/>
                            </span>
                        </div>
                    }
                    <div class="tp__stream-display__stat">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.SHARED")}</span>
                        <span class="tp__stream-display__stat-value"><ToggleSwitch value={stream.channel.shared} readonly={true} compact={true}/></span>
                    </div>
                    <div class="tp__stream-display__stat tp__stream-display__stat--duration">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.DURATION")}</span>
                        <span class="tp__stream-display__stat-value tp__stream-display__duration" data-ts={{let s = if stream.started_at == 0 { stream.ts } else { stream.started_at }; s.to_string()}}>
                            {format_duration(current_time_secs().saturating_sub(if stream.started_at == 0 { stream.ts } else { stream.started_at }))}
                        </span>
                    </div>
                    if props.metrics_enabled && stream.meter_uid != 0 {
                        <div class="tp__stream-display__stat">
                            <span class="tp__stream-display__stat-label">{translate.t("LABEL.BANDWIDTH")}</span>
                            <span class="tp__stream-display__stat-value">
                                <StreamMeterBadge uid={stream.uid} meter_uid={stream.meter_uid} kind={MeterDisplayKind::Bandwidth} />
                            </span>
                        </div>
                        <div class="tp__stream-display__stat">
                            <span class="tp__stream-display__stat-label">{translate.t("LABEL.TRANSFERRED")}</span>
                            <span class="tp__stream-display__stat-value">
                                <StreamMeterBadge uid={stream.uid} meter_uid={stream.meter_uid} kind={MeterDisplayKind::Transferred} />
                            </span>
                        </div>
                    }
                    <div class="tp__stream-display__stat tp__stream-display__detail">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.PLAYER")}</span>
                        <span class="tp__stream-display__stat-value">
                            <RevealContent preview={Some(html! { &stream.user_agent })}>{&stream.user_agent}</RevealContent>
                        </span>
                    </div>
                </div>
           </div>
           <div class="tp__stream-display__row">
                if !chips.is_empty() {
                    <div class="tp__stream-display__chips">
                        { for chips.into_iter().map(|(label, chip_class)| html! {
                            <Chip label={label} class={Some(format!("tp__stream-display__chip {chip_class}"))} />
                        })}
                    </div>
                }
            </div>
        </div>
        </article>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_programme(title: &str, start: i64, stop: i64) -> EpgProgrammeDto {
        EpgProgrammeDto {
            start_timestamp: start,
            stop_timestamp: stop,
            start: format!("{:02}:{:02}", start / 3600, (start % 3600) / 60),
            stop: format!("{:02}:{:02}", stop / 3600, (stop % 3600) / 60),
            title: title.to_string(),
        }
    }

    #[test]
    fn test_compute_current_next_finds_current_and_next() {
        let programmes = vec![
            make_programme("Show A", 100, 200),
            make_programme("Show B", 200, 300),
            make_programme("Show C", 300, 400),
        ];

        let (current, next) = compute_current_next(&programmes, 150);
        assert_eq!(current.as_ref().map(|p| p.title.as_str()), Some("Show A"));
        assert_eq!(next.as_ref().map(|p| p.title.as_str()), Some("Show B"));
    }

    #[test]
    fn test_compute_current_next_at_boundary_becomes_next() {
        let programmes = vec![make_programme("Show A", 100, 200), make_programme("Show B", 200, 300)];

        let (current, next) = compute_current_next(&programmes, 200);
        assert_eq!(current.as_ref().map(|p| p.title.as_str()), Some("Show B"));
        assert!(next.is_none());
    }

    #[test]
    fn test_compute_current_next_empty_programmes() {
        let (current, next) = compute_current_next(&[], 100);
        assert!(current.is_none());
        assert!(next.is_none());
    }

    #[test]
    fn test_compute_current_next_no_programme_running() {
        let programmes = vec![make_programme("Show A", 100, 200)];
        let (current, next) = compute_current_next(&programmes, 50);
        assert!(current.is_none());
        assert!(next.is_none());
    }

    #[test]
    fn test_compute_current_next_last_programme_no_next() {
        let programmes = vec![make_programme("Show A", 100, 200), make_programme("Show B", 200, 300)];

        let (current, next) = compute_current_next(&programmes, 250);
        assert_eq!(current.as_ref().map(|p| p.title.as_str()), Some("Show B"));
        assert!(next.is_none());
    }

    #[test]
    fn test_epg_data_is_valid_fresh() {
        let data = EpgData { response: StreamEpgResponse { entries: vec![] }, fetched_at_secs: 100 };
        assert!(data.is_valid(100));
        assert!(data.is_valid(3600));
        assert!(!data.is_valid(25300));
    }

    #[test]
    fn test_epg_data_is_near_expiry() {
        let data = EpgData { response: StreamEpgResponse { entries: vec![] }, fetched_at_secs: 100 };
        // At t=25300, age=25200 (exactly 7h), is_near_expiry = true
        assert!(data.is_near_expiry(25300));
        // At t=25200, age=25100 (just under 7h), is_near_expiry = false
        assert!(!data.is_near_expiry(25200));
        // At t=100, age=0, is_near_expiry = false
        assert!(!data.is_near_expiry(100));
    }
}
