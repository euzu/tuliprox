use crate::{
    app::{
        components::{
            playlist::explorer::{
                edit_panel::ExplorerEditPanel,
                identity::Id,
                sort::{cluster_label_key, is_edited, item_chno},
            },
            IconButton, RadioButtonGroup, TextButton,
        },
        context::PlaylistExplorerContext,
    },
    config_field_custom,
    hooks::use_service_context,
    html_if,
    i18n::{use_translation, YewI18n},
};
use shared::{
    model::{OverrideScopeDto, Permission, PlaylistRequest, UiPlaylistItem, XtreamCluster},
    utils::format_float_localized,
};
use std::rc::Rc;
use yew::prelude::*;

/// `Live · ID 123 · Input „Provider“` with translated parts.
#[must_use]
pub fn item_meta_label(translate: &YewI18n, item: &UiPlaylistItem) -> String {
    translate
        .t("LABEL.EXPLORER_ITEM_META")
        .replace("{cluster}", &translate.t(cluster_label_key(item.xtream_cluster)))
        .replace("{id}", &item.provider_id)
        .replace("{virtual_id}", &item.virtual_id.to_string())
        .replace("{input}", &item.input_name)
}

#[derive(Properties, PartialEq, Clone)]
pub struct ExplorerDetailPanelProps {
    /// `Id` keeps the prop comparison at a pointer check.
    pub item: Option<Id<UiPlaylistItem>>,
    /// Bumped whenever the catalog behind the panel was replaced.
    #[prop_or_default]
    pub playlist_revision: u32,
    pub on_close: Callback<()>,
    #[prop_or_default]
    pub on_item_updated: Option<Callback<Rc<UiPlaylistItem>>>,
    #[prop_or_default]
    pub on_open_series: Option<Callback<Rc<UiPlaylistItem>>>,
}

#[component]
pub fn ExplorerDetailPanel(props: &ExplorerDetailPanelProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let can_write = services.auth.has_permission(Permission::PlaylistWrite);
    let explorer_ctx = use_context::<PlaylistExplorerContext>();
    let target_id = explorer_ctx.as_ref().and_then(|ctx| match ctx.playlist_request.as_ref() {
        Some(PlaylistRequest::Target(tid)) => Some(*tid),
        _ => None,
    });
    let is_target_view = target_id.is_some();

    let active_scope = use_state(|| if is_target_view { OverrideScopeDto::Target } else { OverrideScopeDto::Input });
    let is_editing = use_state(|| false);
    let current_item = use_state(|| props.item.clone());

    {
        let current_item = current_item.clone();
        use_effect_with(props.item.clone(), move |item| {
            current_item.set(item.clone());
            || ()
        });
    }

    let Some(ref item) = *current_item else {
        return html! {};
    };

    let on_close_edit = {
        let is_editing = is_editing.clone();
        Callback::from(move |()| is_editing.set(false))
    };

    let on_saved = {
        let is_editing = is_editing.clone();
        let current_item = current_item.clone();
        let on_item_updated = props.on_item_updated.clone();
        Callback::from(move |patched_item: UiPlaylistItem| {
            let rc = Rc::new(patched_item);
            current_item.set(Some(Id::new(rc.clone())));
            if let Some(ref cb) = on_item_updated {
                cb.emit(rc);
            }
            is_editing.set(false);
        })
    };

    if *is_editing && can_write {
        return html! {
            <ExplorerEditPanel
                item={Some(item.clone())}
                playlist_revision={props.playlist_revision}
                active_scope={active_scope.clone()}
                on_close={on_close_edit}
                on_saved={on_saved}
            />
        };
    }

    let edited = is_edited(item);
    let is_pending = item.override_state().is_some_and(|s| s.pending);
    let chno = item_chno(item);
    let epg_id = item.epg_channel_id.as_deref().unwrap_or("-");
    let meta_label = item_meta_label(&translate, item);

    let on_scope_select = {
        let active_scope = active_scope.clone();
        Callback::from(move |selection: Rc<Vec<String>>| {
            let scope = if selection.first().map(String::as_str) == Some("input") {
                OverrideScopeDto::Input
            } else {
                OverrideScopeDto::Target
            };
            active_scope.set(scope);
        })
    };

    let on_close = {
        let close_cb = props.on_close.clone();
        Callback::from(move |_: String| close_cb.emit(()))
    };

    let on_edit = {
        let is_editing = is_editing.clone();
        Callback::from(move |_: String| is_editing.set(true))
    };

    html! {
            <div class="det">
                <div class="hero">
                    <IconButton
                        name="close_detail"
                        icon="Back"
                        class="tp__explorer-panel-back"
                        aria_label={Some(translate.t("LABEL.BACK"))}
                        onclick={let on_close = props.on_close.clone(); Callback::from(move |(_name, _): (String, MouseEvent)| on_close.emit(()))}
                    />
                    <b>{ item.title.clone() }</b>
                </div>
                <div class="in">
                    <div class="meta">
                        { meta_label }
                    </div>
                    <div class="scope">
                        {
                            if is_target_view {
                                html! {
    <RadioButtonGroup
                                        options={Rc::new(vec!["input".to_string(), "target".to_string()])}
                                        labels={Some(Rc::new(vec![translate.t("LABEL.EXPLORER_SCOPE_INPUT"), translate.t("LABEL.EXPLORER_SCOPE_TARGET")]))}
                                        selected={Rc::new(vec![if *active_scope == OverrideScopeDto::Input { "input" } else { "target" }.to_string()])}
                                        on_select={on_scope_select}
                                    />
                                }
                            } else {
                                html! {
                                    <span class="on">
                                        { translate.t("LABEL.EXPLORER_SCOPE_INPUT") }
                                    </span>
                                }
                            }
                        }
                    </div>

                    {
                        if is_pending {
                            html! {
                                <div class="tp__explorer-pending-list">
                                    <span class="chip on">
                                        { "⏳ " }{ translate.t("LABEL.EXPLORER_PENDING_UNTIL_UPDATE") }
                                    </span>
                                </div>
                            }
                        } else {
                            html! {}
                        }
                    }

                    { html_if!(edited, {
                        <span class="chip on tp__explorer-edited-chip">{ translate.t("LABEL.EXPLORER_EDITED_BADGE") }</span>
                    })}
                    { config_field_custom!(translate.t("LABEL.NAME"), item.name.to_string(), Some("PLAYLIST_OVERRIDE.NAME".to_string())) }
                    { config_field_custom!(translate.t("LABEL.TITLE"), item.title.to_string(), Some("PLAYLIST_OVERRIDE.TITLE".to_string())) }
                    { config_field_custom!(translate.t("LABEL.GROUP"), item.group.to_string(), Some("PLAYLIST_OVERRIDE.GROUP".to_string())) }
                    { config_field_custom!(translate.t("LABEL.EXPLORER_EPG_ID_TMDB"), epg_id.to_string(), Some("PLAYLIST_OVERRIDE.EPG_CHANNEL_ID".to_string())) }
                    { config_field_custom!(translate.t("LABEL.EXPLORER_LOGO_URL"), html! { <span class="tp__explorer-url-value">{ item.logo.to_string() }</span> }, Some("PLAYLIST_OVERRIDE.LOGO".to_string())) }

                    {
                        if let Some(num) = chno {
                            html! {
                                { config_field_custom!(translate.t("LABEL.CHNO"), num.to_string(), Some("PLAYLIST_OVERRIDE.CHNO".to_string())) }
                            }
                        } else {
                            html! {}
                        }
                    }

                    {
                        if item.rating > 0.001 {
                            html! {
                                { config_field_custom!(translate.t("LABEL.RATING"), html! { <span class="det-rating">{ format!("★ {}", format_float_localized(item.rating, 1, false)) }</span> }, Some("PLAYLIST_OVERRIDE.RATING".to_string())) }
                            }
                        } else {
                            html! {}
                        }
                    }

                    {
                        if item.xtream_cluster == XtreamCluster::Series && props.on_open_series.is_some() {
                            let on_open = props.on_open_series.clone();
                            let item_rc = item.clone().into_inner();
                            let handle_show_episodes = Callback::from(move |_: String| {
                                if let Some(ref cb) = on_open {
                                    cb.emit(item_rc.clone());
                                }
                            });
                            html! {
                                <div class="tp__explorer-detail-series-action">
                                    <TextButton
                                        name="show_episodes"
                                        class="primary"
                                        icon="Playlist"
                                        title={translate.t("LABEL.EXPLORER_SHOW_EPISODES")}
                                        onclick={handle_show_episodes}
                                    />
                                </div>
                            }
                        } else {
                            html! {}
                        }
                    }

                    <div class="btns">
                        <TextButton name="close" class="secondary" icon="Close" title={translate.t("LABEL.CLOSE")} onclick={on_close} />
                        {
                            if can_write {
                                html! {
                                    <TextButton name="edit" class="primary" icon="Edit" title={translate.t("LABEL.EXPLORER_EDIT_STUB")} onclick={on_edit} />
                                }
                            } else {
                                html! {}
                            }
                        }
                    </div>
                </div>
            </div>
        }
}
