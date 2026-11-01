use crate::{
    app::{
        components::playlist::explorer::{
            edit_panel::ExplorerEditPanel,
            identity::Id,
            sort::{cluster_label_key, extract_channel_no, is_edited},
        },
        context::PlaylistExplorerContext,
    },
    i18n::{use_translation, YewI18n},
};
use shared::model::{OverrideScopeDto, PlaylistRequest, UiPlaylistItem};
use std::rc::Rc;
use yew::prelude::*;

/// `Live · ID 123 · Input „Provider“` with translated parts.
#[must_use]
pub fn item_meta_label(translate: &YewI18n, item: &UiPlaylistItem) -> String {
    translate
        .t("LABEL.EXPLORER_ITEM_META")
        .replace("{cluster}", &translate.t(cluster_label_key(item.xtream_cluster)))
        .replace("{id}", &item.provider_id)
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
}

#[component]
pub fn ExplorerDetailPanel(props: &ExplorerDetailPanelProps) -> Html {
    let translate = use_translation();
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

    if *is_editing {
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
    let chno = extract_channel_no(&item.name, &item.title);
    let epg_id = item.epg_channel_id.as_deref().unwrap_or("-");
    let meta_label = item_meta_label(&translate, item);

    let toggle_input_scope = {
        let active_scope = active_scope.clone();
        Callback::from(move |_: MouseEvent| active_scope.set(OverrideScopeDto::Input))
    };

    let toggle_target_scope = {
        let active_scope = active_scope.clone();
        Callback::from(move |_: MouseEvent| active_scope.set(OverrideScopeDto::Target))
    };

    let on_close = {
        let close_cb = props.on_close.clone();
        Callback::from(move |_: MouseEvent| close_cb.emit(()))
    };

    let on_edit = {
        let is_editing = is_editing.clone();
        Callback::from(move |_: MouseEvent| is_editing.set(true))
    };

    html! {
        <div class="det">
            <div class="hero">
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
                                <>
                                    <button
                                        type="button"
                                        class={if *active_scope == OverrideScopeDto::Input { "on" } else { "" }}
                                        aria-pressed={(*active_scope == OverrideScopeDto::Input).to_string()}
                                        onclick={toggle_input_scope}
                                    >
                                        { translate.t("LABEL.EXPLORER_SCOPE_INPUT") }
                                    </button>
                                    <button
                                        type="button"
                                        class={if *active_scope == OverrideScopeDto::Target { "on" } else { "" }}
                                        aria-pressed={(*active_scope == OverrideScopeDto::Target).to_string()}
                                        onclick={toggle_target_scope}
                                    >
                                        { translate.t("LABEL.EXPLORER_SCOPE_TARGET") }
                                    </button>
                                </>
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

                <div class={format!("f {}", if edited { "chg" } else { "" })}>
                    <label>
                        { translate.t("LABEL.NAME") }
                        { if edited { html! { <span class="o">{ " · " }{ translate.t("LABEL.EXPLORER_EDITED_BADGE") }</span> } } else { html! {} } }
                    </label>
                    <div class="v">
                        <span>{ item.name.clone() }</span>
                    </div>
                </div>

                <div class="f">
                    <label>{ translate.t("LABEL.TITLE") }</label>
                    <div class="v"><span>{ item.title.clone() }</span></div>
                </div>

                <div class="f">
                    <label>{ translate.t("LABEL.GROUP") }</label>
                    <div class="v"><span>{ item.group.clone() }</span></div>
                </div>

                <div class="f">
                    <label>{ translate.t("LABEL.EXPLORER_EPG_ID_TMDB") }</label>
                    <div class="v"><span>{ epg_id }</span></div>
                </div>

                <div class="f">
                    <label>{ translate.t("LABEL.EXPLORER_LOGO_URL") }</label>
                    <div class="v"><span>{ item.logo.clone() }</span></div>
                </div>

                {
                    if let Some(num) = chno {
                        html! {
                            <div class="f">
                                <label>{ translate.t("LABEL.CHNO") }</label>
                                <div class="v"><span>{ num }</span></div>
                            </div>
                        }
                    } else {
                        html! {}
                    }
                }

                <div class="btns">
                    <button type="button" class="btn" onclick={on_close}>{ translate.t("LABEL.CANCEL") }</button>
                    <button type="button" class="btn p" onclick={on_edit}>{ translate.t("LABEL.EXPLORER_EDIT_STUB") }</button>
                </div>
            </div>
        </div>
    }
}
