use super::{
    helpers::{build_technical_chips, format_duration, render_cluster},
    meter::{MeterDisplayKind, StreamMeterBadge},
};
use crate::{
    app::components::{AppIcon, Chip, RevealContent, ToggleSwitch},
    i18n::use_translation,
    utils::t_safe,
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
    let stream = props.stream.clone();
    let chips = build_technical_chips(stream.channel.item_type, stream.channel.technical.as_ref());
    let country = stream
        .country
        .as_ref()
        .map_or_else(String::new, |c| t_safe(&translate, &format!("COUNTRY.{c}")).unwrap_or_else(|| c.to_string()));
    let client_ip = strip_port(&stream.client_ip).to_string();

    let handle_popup_click = {
        let stream = stream.clone();
        let on_popup_click = props.on_popup_click.clone();
        Callback::from(move |event: MouseEvent| on_popup_click.emit((stream.clone(), event)))
    };

    html! {
        <article class="tp__stream-display__item">
            <button class="tp__stream-display__menu" onclick={handle_popup_click}>
                <AppIcon name="Handle" />
            </button>
        <div class="tp__stream-display__item-content">
            <div class="tp__stream-display__item-head">
                <div class="tp__stream-display__identity">
                    <div class="tp__stream-display__title-row">
                        <div class="tp__stream-display__title-block">
                            <div class="tp__stream-display__title">{stream.channel.title.to_string()}</div>
                            <div class="tp__stream-display__subtitle">
                                <span class="tp__stream-display__username"> {stream.username.clone()}</span>
                                {" • "}
                                <span class="tp__stream-display__channel_type">{render_cluster(&stream.channel)}</span>
                                {" • "}
                                <span class="tp__stream-display__provider">{stream.provider.clone()}</span>
                                if !country.is_empty() {
                                    <>
                                        {" • "}
                                        {country}
                                    </>
                                }
                            </div>
                        </div>
                    </div>
                </div>
                <div class="tp__stream-display__stats">
                    <div class="tp__stream-display__stat">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.CLIENT_IP")}</span>
                        <span class="tp__stream-display__stat-value">{client_ip.clone()}</span>
                    </div>
                    <div class="tp__stream-display__stat">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.SHARED")}</span>
                        <span class="tp__stream-display__stat-value"><ToggleSwitch value={stream.channel.shared} readonly={true} /></span>
                    </div>
                    <div class="tp__stream-display__stat">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.DURATION")}</span>
                        <span class="tp__stream-display__stat-value tp__stream-display__duration" data-ts={stream.ts.to_string()}>
                            {format_duration(current_time_secs().saturating_sub(stream.ts))}
                        </span>
                    </div>
                    if props.metrics_enabled {
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
                <div class="tp__stream-display__details">
                    <div class="tp__stream-display__detail">
                        <span class="tp__stream-display__detail-label">{translate.t("LABEL.GROUP")}</span>
                        <span class="tp__stream-display__detail-value">{stream.channel.group.to_string()}</span>
                    </div>
                    <div class="tp__stream-display__detail tp__stream-display__detail">
                        <span class="tp__stream-display__detail-label">{translate.t("LABEL.PLAYER")}</span>
                        <span class="tp__stream-display__detail-value">
                            <RevealContent preview={Some(html! { &stream.user_agent })}>{&stream.user_agent}</RevealContent>
                        </span>
                    </div>
                </div>
            </div>
        </div>
        </article>
    }
}
