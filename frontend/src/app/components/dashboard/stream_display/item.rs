use super::{
    helpers::{build_technical_chips, is_background_transfer_stream, render_cluster},
    meter::{MeterDisplayKind, StreamMeterBadge},
};
use crate::{
    app::{
        components::{country::display_country_code, AppIcon, Chip, Country, RevealContent, ToggleSwitch},
        ConfigContext,
    },
    i18n::use_translation,
    utils::format_duration,
};
use shared::{
    model::StreamInfo,
    utils::{current_time_secs, strip_port},
};
use std::rc::Rc;
use web_sys::MouseEvent;
use yew::prelude::*;

#[derive(Properties, PartialEq, Clone)]
pub struct StreamDisplayItemProps {
    pub stream: Rc<StreamInfo>,
    pub metrics_enabled: bool,
    pub on_popup_click: Callback<(Rc<StreamInfo>, MouseEvent)>,
}

#[component]
pub fn StreamDisplayItem(props: &StreamDisplayItemProps) -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let stream = props.stream.clone();
    let is_background_transfer = is_background_transfer_stream(&stream);
    let chips = build_technical_chips(stream.channel.item_type, stream.channel.technical.as_ref());
    let client_ip = strip_port(&stream.client_ip).to_string();

    let handle_popup_click = {
        let stream = stream.clone();
        let on_popup_click = props.on_popup_click.clone();
        Callback::from(move |event: MouseEvent| on_popup_click.emit((stream.clone(), event)))
    };

    let get_user_comment = {
        let config_ctx = config_ctx.clone();
        Callback::from(move |username: &str| {
            if let Some(api_proxy) = config_ctx.api_proxy.as_ref() {
                for target_user in &api_proxy.user {
                    for credential in &target_user.credentials {
                        if credential.username == username {
                            return credential.comment.clone();
                        }
                    }
                }
            }
            None
        })
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
                                <span class="tp__stream-display__user-comment"> {get_user_comment.emit(stream.username.as_str()).map(|c| format!("({c})"))}</span>
                            </div>
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
