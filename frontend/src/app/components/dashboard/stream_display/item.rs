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
    model::{
        EpgProgrammeDto, StreamEpgItemRequest, StreamEpgResponse, StreamInfo, StreamInfoConfigDto, StreamInfoFields,
        StreamInfoFieldsSet,
    },
    utils::{current_time_secs, strip_port},
};
use std::rc::Rc;
use wasm_bindgen_futures::spawn_local;
use web_sys::MouseEvent;
use yew::prelude::*;

fn only_time(ts: &str) -> String {
    match ts.split_once(' ') {
        Some((_date, time)) => time.to_string(),
        None => ts.to_string(),
    }
}

#[derive(Properties, PartialEq, Clone)]
pub struct StreamDisplayItemProps {
    pub stream: Rc<StreamInfo>,
    pub user_comment: Option<String>,
    pub metrics_enabled: bool,
    pub stream_info: Option<Rc<StreamInfoConfigDto>>,
    pub on_popup_click: Callback<(Rc<StreamInfo>, MouseEvent)>,
}

/// Holds the fetched EPG data and its fetch timestamp for staleness checking.
#[derive(Debug, Clone)]
struct EpgData {
    channel_id: std::sync::Arc<str>,
    target_id: Option<u16>,
    epg_reference_ts: Option<i64>,
    response: StreamEpgResponse,
    fetched_at_secs: u64,
}

impl EpgData {
    fn matches_request(
        &self,
        channel_id: &std::sync::Arc<str>,
        target_id: Option<u16>,
        epg_reference_ts: Option<i64>,
    ) -> bool {
        self.channel_id.as_ref() == channel_id.as_ref()
            && self.target_id == target_id
            && self.epg_reference_ts == epg_reference_ts
    }

    fn is_stale(&self, now_secs: u64) -> bool {
        let age = now_secs.saturating_sub(self.fetched_at_secs);
        age >= 7 * 3600
    }
}

type EpgIntervalEffectDeps = (Option<std::sync::Arc<str>>, Option<u16>, Option<u64>, Option<i64>, bool);
type EpgFetchEffectDeps = (Option<std::sync::Arc<str>>, Option<u16>, bool, Option<i64>, StreamInfoFieldsSet);

fn epg_interval_effect_deps(
    epg_channel_id: &Option<std::sync::Arc<str>>,
    epg_target_id: Option<u16>,
    epg_data: Option<&Rc<EpgData>>,
    epg_reference_ts: Option<i64>,
    hide_epg: bool,
) -> EpgIntervalEffectDeps {
    (epg_channel_id.clone(), epg_target_id, epg_data.map(|data| data.fetched_at_secs), epg_reference_ts, hide_epg)
}

fn epg_fetch_effect_deps(
    epg_channel_id: &Option<std::sync::Arc<str>>,
    epg_target_id: Option<u16>,
    needs_refetch: bool,
    epg_reference_ts: Option<i64>,
    hide_properties: StreamInfoFieldsSet,
) -> EpgFetchEffectDeps {
    (epg_channel_id.clone(), epg_target_id, needs_refetch, epg_reference_ts, hide_properties)
}

/// Computes current and next programme from a list of programmes.
fn compute_current_next(
    programmes: &[EpgProgrammeDto],
    now_secs: i64,
) -> (Option<EpgProgrammeDto>, Option<EpgProgrammeDto>) {
    let current = programmes.iter().find(|p| now_secs >= p.start_timestamp && now_secs < p.stop_timestamp).cloned();

    let next = current.as_ref().and_then(|cur| {
        programmes.iter().filter(|p| p.start_timestamp >= cur.stop_timestamp).min_by_key(|p| p.start_timestamp).cloned()
    });

    (current, next)
}

fn format_user_comment(user_comment: Option<String>) -> Option<String> {
    user_comment.and_then(|comment| {
        let trimmed = comment.trim();
        (!trimmed.is_empty()).then(|| format!("({trimmed})"))
    })
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
    let epg_reference_ts = stream.channel.epg_reference_ts;

    // EPG state: cached response + fetched timestamp
    let epg_data: UseStateHandle<Option<Rc<EpgData>>> = use_state(|| None);
    // Computed current/next for display
    let current_next: UseStateHandle<Option<(EpgProgrammeDto, Option<EpgProgrammeDto>)>> = use_state(|| None);
    // Refetch trigger: set to true when cache is stale, causes fetch effect to re-run
    let needs_refetch: UseStateHandle<bool> = use_state(|| false);

    // Fetch EPG when epg_channel_id is present or when needs_refetch is triggered
    let hide_properties = props.stream_info.as_ref().map_or_else(StreamInfoFieldsSet::new, |cfg| cfg.get_flags());
    // When EPG becomes hidden, clear stale UI state
    {
        let epg_data = epg_data.clone();
        let current_next = current_next.clone();
        use_effect_with(hide_properties, move |hide_props| {
            if hide_props.contains(StreamInfoFields::HideEpg) {
                epg_data.set(None);
                current_next.set(None);
            }
            || ()
        });
    }
    {
        let epg_channel_id = epg_channel_id.clone();
        let epg_data = epg_data.clone();
        let current_next = current_next.clone();
        let needs_refetch = needs_refetch.clone();
        let fetch_deps = epg_fetch_effect_deps(
            &epg_channel_id,
            Some(epg_target_id),
            *needs_refetch,
            epg_reference_ts,
            hide_properties,
        );
        use_effect_with(
            fetch_deps,
            move |(epg_channel_id, effect_target_id, force_refetch, effect_reference_ts, _hide_props)| {
                let Some(epg_channel_id) = epg_channel_id.clone() else {
                    return;
                };
                if hide_properties.contains(StreamInfoFields::HideEpg) {
                    return;
                }
                let force_refetch = *force_refetch;
                let effect_target_id = *effect_target_id;
                let effect_reference_ts = *effect_reference_ts;

                let epg_channel_id = epg_channel_id.clone();
                let epg_data = epg_data.clone();
                let current_next = current_next.clone();
                let needs_refetch = needs_refetch.clone();

                spawn_local(async move {
                    let now_secs = current_time_secs();
                    let now_i64 = effect_reference_ts.unwrap_or(now_secs as i64);
                    let reset_state = || {
                        current_next.set(None);
                        epg_data.set(None);
                        if force_refetch {
                            needs_refetch.set(false);
                        }
                    };

                    // Check if cache is valid before fetching
                    if let Some(ref data) = *epg_data {
                        if !data.matches_request(&epg_channel_id, effect_target_id, effect_reference_ts) {
                            current_next.set(None);
                            epg_data.set(None);
                        } else if !force_refetch && !data.is_stale(now_secs) {
                            return;
                        }
                    }

                    // Fetch fresh data
                    let service = PlaylistService::new();
                    if let Some(response) = service
                        .get_stream_epg(vec![StreamEpgItemRequest {
                            epg_channel_id: epg_channel_id.to_string(),
                            target_id: effect_target_id,
                            reference_ts: effect_reference_ts,
                        }])
                        .await
                    {
                        if let Some(entry) = response.entries.first() {
                            let (current, next) = compute_current_next(&entry.programmes, now_i64);
                            let new_data = Rc::new(EpgData {
                                channel_id: epg_channel_id.clone(),
                                target_id: effect_target_id,
                                epg_reference_ts: effect_reference_ts,
                                response,
                                fetched_at_secs: now_secs,
                            });
                            current_next.set(current.map(|cur| (cur, next)));
                            epg_data.set(Some(new_data));
                        } else {
                            reset_state();
                            return;
                        }
                    } else {
                        reset_state();
                        return;
                    }

                    if force_refetch {
                        needs_refetch.set(false);
                    }
                });
            },
        );
    }

    // Local tick: recompute current/next and detect staleness every 30 seconds
    {
        let interval_deps = epg_interval_effect_deps(
            &epg_channel_id,
            Some(epg_target_id),
            (*epg_data).as_ref(),
            epg_reference_ts,
            hide_properties.contains(StreamInfoFields::HideEpg),
        );
        let epg_data = epg_data.clone();
        let current_next = current_next.clone();
        let needs_refetch = needs_refetch.clone();
        use_effect_with(interval_deps, move |(epg_channel_id, effect_target_id, _, effect_reference_ts, hide_epg)| {
            if *hide_epg {
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            }
            let effect_target_id = *effect_target_id;
            let effect_reference_ts = *effect_reference_ts;
            epg_channel_id.as_ref().map_or_else(
                || Box::new(|| ()) as Box<dyn FnOnce()>,
                |channel_id| {
                    let epg_data = epg_data.clone();
                    let current_next = current_next.clone();
                    let needs_refetch = needs_refetch.clone();
                    let channel_id = channel_id.clone();

                    let interval = gloo_timers::callback::Interval::new(30_000, move || {
                        let epg_data = epg_data.clone();
                        let current_next = current_next.clone();
                        let needs_refetch = needs_refetch.clone();

                        let now_secs = current_time_secs();

                        if let Some(data) = (*epg_data).as_ref() {
                            if !data.matches_request(&channel_id, effect_target_id, effect_reference_ts) {
                                current_next.set(None);
                                epg_data.set(None);
                                needs_refetch.set(true);
                                return;
                            }
                        }

                        // Check if cache is stale and needs refetch
                        if let Some(data) = (*epg_data).as_ref() {
                            if data.is_stale(now_secs) {
                                needs_refetch.set(true);
                                return;
                            }
                        }

                        // Recompute current/next from cached data
                        if let Some(data) = (*epg_data).as_ref() {
                            if let Some(entry) = data.response.entries.first() {
                                let now_i64 = effect_reference_ts.unwrap_or(now_secs as i64);
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
                                if !hide_properties.contains(StreamInfoFields::HideUserComment) {
                                    <span class="tp__stream-display__user-comment"> {format_user_comment(props.user_comment.clone())}</span>
                                }
                            </div>
                            if !hide_properties.contains(StreamInfoFields::HideEpg) {
                            if let Some((current, next_opt)) = current_next.as_ref() {
                                if !current.title.is_empty() {
                                    <div class="tp__stream-display__epg">
                                        <span class="tp__stream-display__epg-now">
                                            <span class="tp__stream-display__epg-time">{only_time(&current.start)}</span>
                                            {" "}
                                            <span class="tp__stream-display__epg-title">{&current.title}</span>
                                        </span>
                                        if let Some(next) = next_opt.as_ref() {
                                            <span class="tp__stream-display__epg-next">
                                                <span class="tp__stream-display__epg-separator">{" ↦ "}</span>
                                                <span class="tp__stream-display__epg-time">{only_time(&next.start)}</span>
                                                {" "}
                                                <span class="tp__stream-display__epg-title">{&next.title}</span>
                                            </span>
                                        }
                                    </div>
                                }
                            }
                            }
                        </div>
                    </div>
                </div>
           </div>
           <div class="tp__stream-display__row">
            <div class="tp__stream-display__stats">
                    if !hide_properties.contains(StreamInfoFields::HideGroup) {
                    <div class="tp__stream-display__stat tp__stream-display__stat--category">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.GROUP")}</span>
                        <span class="tp__stream-display__stat-value">{stream.channel.group.to_string()}</span>
                    </div>
                    }
                    if !hide_properties.contains(StreamInfoFields::HideIp) {
                    <div class="tp__stream-display__stat tp__stream-display__stat--client">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.CLIENT_IP")}</span>
                        <span class="tp__stream-display__stat-value tp__stream-display__stat-value--ip">{client_ip.clone()}</span>
                    </div>
                    }
                    if !hide_properties.contains(StreamInfoFields::HideCountry) && display_country_code(stream.country_code.as_deref()).is_some() {
                        <div class="tp__stream-display__stat tp__stream-display__stat--country">
                            <span class="tp__stream-display__stat-label">{translate.t("LABEL.COUNTRY")}</span>
                            <span class="tp__stream-display__stat-value">
                                <Country country_code={stream.country_code.clone()}/>
                            </span>
                        </div>
                    }
                    if !hide_properties.contains(StreamInfoFields::HideShared) {
                    <div class="tp__stream-display__stat">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.SHARED")}</span>
                        <span class="tp__stream-display__stat-value"><ToggleSwitch value={stream.channel.shared} readonly={true} compact={true}/></span>
                    </div>
                    }
                    if !hide_properties.contains(StreamInfoFields::HideDuration) {
                    <div class="tp__stream-display__stat tp__stream-display__stat--duration">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.DURATION")}</span>
                        <span class="tp__stream-display__stat-value tp__stream-display__duration" data-ts={{let s = if stream.started_at == 0 { stream.ts } else { stream.started_at }; s.to_string()}}>
                            {format_duration(current_time_secs().saturating_sub(if stream.started_at == 0 { stream.ts } else { stream.started_at }))}
                        </span>
                    </div>
                    }
                    if  props.metrics_enabled && stream.meter_uid != 0 && !hide_properties.contains(StreamInfoFields::HideBandwidth) {
                        <div class="tp__stream-display__stat">
                            <span class="tp__stream-display__stat-label">{translate.t("LABEL.BANDWIDTH")}</span>
                            <span class="tp__stream-display__stat-value">
                                <StreamMeterBadge uid={stream.uid} meter_uid={stream.meter_uid} kind={MeterDisplayKind::Bandwidth} />
                            </span>
                        </div>
                    }
                    if props.metrics_enabled && stream.meter_uid != 0 && !hide_properties.contains(StreamInfoFields::HideTransferred) {
                        <div class="tp__stream-display__stat">
                            <span class="tp__stream-display__stat-label">{translate.t("LABEL.TRANSFERRED")}</span>
                            <span class="tp__stream-display__stat-value">
                                <StreamMeterBadge uid={stream.uid} meter_uid={stream.meter_uid} kind={MeterDisplayKind::Transferred} />
                            </span>
                        </div>
                    }
                    if !hide_properties.contains(StreamInfoFields::HidePlayer) {
                    <div class="tp__stream-display__stat tp__stream-display__detail">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.PLAYER")}</span>
                        <span class="tp__stream-display__stat-value">
                            <RevealContent preview={Some(html! { &stream.user_agent })}>{&stream.user_agent}</RevealContent>
                        </span>
                    </div>
                    }
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
    fn test_compute_current_next_picks_earliest_future_programme_when_unsorted() {
        let programmes = vec![
            make_programme("Late Show", 400, 500),
            make_programme("Current Show", 100, 200),
            make_programme("Next Show", 200, 300),
        ];

        let (current, next) = compute_current_next(&programmes, 150);
        assert_eq!(current.as_ref().map(|p| p.title.as_str()), Some("Current Show"));
        assert_eq!(next.as_ref().map(|p| p.title.as_str()), Some("Next Show"));
    }

    #[test]
    fn test_epg_data_is_stale() {
        let data = EpgData {
            channel_id: std::sync::Arc::<str>::from("channel-1"),
            target_id: Some(7),
            epg_reference_ts: None,
            response: StreamEpgResponse { entries: vec![] },
            fetched_at_secs: 100,
        };
        assert!(!data.is_stale(100));
        assert!(!data.is_stale(3600));
        assert!(data.is_stale(25300));
    }

    #[test]
    fn test_epg_data_matches_request() {
        let data = EpgData {
            channel_id: std::sync::Arc::<str>::from("channel-1"),
            target_id: Some(7),
            epg_reference_ts: Some(1_700_000_000),
            response: StreamEpgResponse { entries: vec![] },
            fetched_at_secs: 100,
        };

        assert!(data.matches_request(&std::sync::Arc::<str>::from("channel-1"), Some(7), Some(1_700_000_000)));
        assert!(!data.matches_request(&std::sync::Arc::<str>::from("channel-1"), Some(8), Some(1_700_000_000)));
        assert!(!data.matches_request(&std::sync::Arc::<str>::from("channel-1"), Some(7), Some(1_700_000_600)));
        assert!(!data.matches_request(&std::sync::Arc::<str>::from("channel-2"), Some(7), Some(1_700_000_000)));
    }

    #[test]
    fn test_format_user_comment_trims_and_skips_empty_values() {
        assert_eq!(format_user_comment(None), None);
        assert_eq!(format_user_comment(Some("   ".to_string())), None);
        assert_eq!(format_user_comment(Some(" comment ".to_string())), Some("(comment)".to_string()));
    }

    #[test]
    fn test_epg_interval_effect_deps_change_when_epg_data_changes() {
        let channel_id = Some(std::sync::Arc::<str>::from("channel-1"));
        let first = Rc::new(EpgData {
            channel_id: std::sync::Arc::<str>::from("channel-1"),
            target_id: Some(7),
            epg_reference_ts: None,
            response: StreamEpgResponse { entries: vec![] },
            fetched_at_secs: 100,
        });
        let second = Rc::new(EpgData {
            channel_id: std::sync::Arc::<str>::from("channel-1"),
            target_id: Some(7),
            epg_reference_ts: None,
            response: StreamEpgResponse { entries: vec![] },
            fetched_at_secs: 200,
        });

        let first_deps = epg_interval_effect_deps(&channel_id, Some(7), Some(&first), None, false);
        let second_deps = epg_interval_effect_deps(&channel_id, Some(7), Some(&second), None, false);

        assert_ne!(first_deps, second_deps);
        assert_eq!(first_deps.1, Some(7));
        assert_eq!(second_deps.1, Some(7));
        assert_eq!(first_deps.2, Some(100));
        assert_eq!(second_deps.2, Some(200));
        assert_eq!(first_deps.3, None);
        assert_eq!(second_deps.3, None);
        assert!(!first_deps.4);
        assert!(!second_deps.4);
    }

    #[test]
    fn test_epg_fetch_effect_deps_change_when_reference_changes() {
        let channel_id = Some(std::sync::Arc::<str>::from("channel-1"));

        let first_deps =
            epg_fetch_effect_deps(&channel_id, Some(7), false, Some(1_700_000_000), StreamInfoFieldsSet::new());
        let second_deps =
            epg_fetch_effect_deps(&channel_id, Some(7), false, Some(1_700_000_600), StreamInfoFieldsSet::new());

        assert_ne!(first_deps, second_deps);
        assert_eq!(first_deps.1, Some(7));
        assert_eq!(second_deps.1, Some(7));
        assert_eq!(first_deps.3, Some(1_700_000_000));
        assert_eq!(second_deps.3, Some(1_700_000_600));
    }

    #[test]
    fn test_epg_interval_effect_deps_change_when_reference_changes() {
        let channel_id = Some(std::sync::Arc::<str>::from("channel-1"));
        let data = Rc::new(EpgData {
            channel_id: std::sync::Arc::<str>::from("channel-1"),
            target_id: Some(7),
            epg_reference_ts: Some(1_700_000_000),
            response: StreamEpgResponse { entries: vec![] },
            fetched_at_secs: 100,
        });

        let first_deps = epg_interval_effect_deps(&channel_id, Some(7), Some(&data), Some(1_700_000_000), false);
        let second_deps = epg_interval_effect_deps(&channel_id, Some(7), Some(&data), Some(1_700_000_600), false);

        assert_ne!(first_deps, second_deps);
        assert_eq!(first_deps.1, Some(7));
        assert_eq!(second_deps.1, Some(7));
        assert_eq!(first_deps.3, Some(1_700_000_000));
        assert_eq!(second_deps.3, Some(1_700_000_600));
    }

    #[test]
    fn test_epg_effect_deps_change_when_target_changes() {
        let channel_id = Some(std::sync::Arc::<str>::from("channel-1"));
        let data = Rc::new(EpgData {
            channel_id: std::sync::Arc::<str>::from("channel-1"),
            target_id: Some(7),
            epg_reference_ts: Some(1_700_000_000),
            response: StreamEpgResponse { entries: vec![] },
            fetched_at_secs: 100,
        });

        let first_fetch =
            epg_fetch_effect_deps(&channel_id, Some(7), false, Some(1_700_000_000), StreamInfoFieldsSet::new());
        let second_fetch =
            epg_fetch_effect_deps(&channel_id, Some(8), false, Some(1_700_000_000), StreamInfoFieldsSet::new());
        let first_interval = epg_interval_effect_deps(&channel_id, Some(7), Some(&data), Some(1_700_000_000), false);
        let second_interval = epg_interval_effect_deps(&channel_id, Some(8), Some(&data), Some(1_700_000_000), false);

        assert_ne!(first_fetch, second_fetch);
        assert_ne!(first_interval, second_interval);
    }
}
