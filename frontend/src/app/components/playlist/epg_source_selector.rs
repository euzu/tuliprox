use crate::{
    app::components::{
        collect_provider_buttons,
        input::Input,
        playlist::source_selector_common::{build_source_type_options, source_selection_callback, submit_on_enter},
        Card, CollapsePanel, Panel, PlaylistContext, RadioButtonGroup, TextButton,
    },
    hooks::use_service_context,
    html_if,
    i18n::use_translation,
    model::ExplorerSourceType,
};
use shared::{model::PlaylistEpgRequest, utils::Internable};
use std::rc::Rc;
use web_sys::HtmlInputElement;
use yew::prelude::*;

#[derive(Properties, PartialEq, Clone)]
pub struct EpgSourceSelectorProps {
    #[prop_or_default]
    pub source_types: Option<Vec<ExplorerSourceType>>,
    #[prop_or_default]
    pub on_select: Callback<PlaylistEpgRequest>,
}

#[component]
pub fn EpgSourceSelector(props: &EpgSourceSelectorProps) -> Html {
    let translate = use_translation();
    let services_ctx = use_service_context();
    let playlist_ctx = use_context::<PlaylistContext>().expect("Playlist context not found");
    let active_source = use_state(|| ExplorerSourceType::Hosted);
    let url_ref = use_node_ref();
    let source_types = use_memo(props.source_types.clone(), |st| {
        build_source_type_options(
            st,
            &[ExplorerSourceType::Hosted, /*ExplorerSourceType::Provider,*/ ExplorerSourceType::Custom],
        )
    });

    let handle_source_select = source_selection_callback(active_source.clone());

    let handle_source_download = {
        let on_select = props.on_select.clone();
        Callback::from(move |request: PlaylistEpgRequest| on_select.emit(request))
    };

    let handle_custom_source = {
        let services = services_ctx.clone();
        let translate = translate.clone();
        let handle_source_download = handle_source_download.clone();
        let url_ref = url_ref.clone();
        Callback::from(move |_| {
            let url = if let Some(input) = url_ref.cast::<HtmlInputElement>() {
                input.value().trim().to_owned()
            } else {
                services.toastr.error(translate.t("MESSAGES.PLAYLIST_UPDATE.URL_MANDATORY"));
                return;
            };

            let mut valid = true;
            if url.is_empty() {
                services.toastr.error(translate.t("MESSAGES.PLAYLIST_UPDATE.URL_MANDATORY"));
                valid = false;
            }
            if valid {
                handle_source_download.emit(PlaylistEpgRequest::Custom(url));
            }
        })
    };

    let handle_key_down = submit_on_enter(handle_custom_source.clone(), "custom".to_owned());

    let render_hosted = {
        let playlist_ctx_clone = playlist_ctx.clone();
        let handle_defined_source = handle_source_download.clone();
        move || {
            html! {
            <>
            {
                if let Some(data) = playlist_ctx_clone.sources.as_ref() {
                    html! {
                        <div class="tp__playlist-source-selector__source-list">
                            { for data.iter().flat_map(|(_inputs, targets)| targets)
                                .map(Rc::clone)
                                .map(|target| {
                                    let handle_click = handle_defined_source.clone();
                                    html! {
                                    <TextButton name={target.name.clone()} title={target.name.clone()} icon={"Download"}
                                    onclick={move |_| handle_click.emit(PlaylistEpgRequest::Target(target.id))}/>
                                    }
                            })}
                        </div>
                    }
                } else {
                    html! {}
                }
            }
            </>
            }
        }
    };

    let render_provider = {
        let playlist_ctx_clone = playlist_ctx.clone();
        let handle_defined_source = handle_source_download.clone();
        move || {
            html! {
            <>
            {
                if let Some(data) = playlist_ctx_clone.sources.as_ref() {
                    html! {
                        <div class="tp__playlist-source-selector__source-list">
                            { for collect_provider_buttons(data.as_ref()).into_iter().map(|(name, id)| {
                                let handle_click = handle_defined_source.clone();
                                let input_name = name.to_string();
                                html! {
                                    <TextButton
                                        key={id}
                                        name={name.to_string()}
                                        title={name.to_string()}
                                        icon={"CloudDownload"}
                                        onclick={move |_| handle_click.emit(PlaylistEpgRequest::Input(input_name.clone()))}
                                    />
                                }
                            })}
                        </div>
                    }
                } else {
                    html! {}
                }
            }
            </>
            }
        }
    };

    let render_custom = {
        let translate = translate.clone();
        let handle_custom_source = handle_custom_source.clone();
        let url_ref = url_ref.clone();
        let handle_key_down = handle_key_down.clone();
        move || {
            html! {
                <div class="tp__playlist-source-selector__source-custom">
                  <div class="tp__playlist-source-selector__source-custom-body">
                     <Input
                         label={translate.t("LABEL.URL")}
                         field_id={Some("PLAYLIST_EPG_SOURCE_SELECTOR.URL".to_string())}
                         input_ref={url_ref}
                         name="url"
                         autocomplete={true}
                         onkeydown={handle_key_down}
                     />
                     <TextButton name={"custom"} title={translate.t("LABEL.DOWNLOAD")} icon={"CloudDownload"}
                       onclick={handle_custom_source}/>
                  </div>
                </div>
            }
        }
    };

    let active_source_interned = (*active_source).intern();
    html! {
      <div class="tp__playlist-source-selector tp__list-list">
        <div class="tp__playlist-source-selector__body tp__list-list__body">
            <CollapsePanel class="tp__playlist-source-selector__source-picker" expanded={true}
               title={translate.t("LABEL.SOURCE_PICKER")}>
               <Card>
                <div class="tp__playlist-source-selector__source-picker__header">
                    <RadioButtonGroup options={source_types.clone()}
                                  selected={Rc::new(vec![(*active_source).to_string()])}
                                  on_select={handle_source_select} />
                </div>
                <div class="tp__playlist-source-selector__source-picker__body">
                    { html_if!(source_types.contains(&ExplorerSourceType::Hosted.to_string()), {
                        <Panel value={ExplorerSourceType::Hosted.intern()} active={active_source_interned.clone()}>
                            { render_hosted() }
                        </Panel>
                    })}
                    { html_if!(source_types.contains(&ExplorerSourceType::Provider.to_string()), {
                        <Panel value={ExplorerSourceType::Provider.intern()} active={active_source_interned}>
                            { render_provider() }
                        </Panel>
                    })}
                    { html_if!(source_types.contains(&ExplorerSourceType::Custom.to_string()), {
                        <Panel value={ExplorerSourceType::Custom.intern()} active={active_source.intern()}>
                            { render_custom() }
                        </Panel>
                    })}
                </div>
              </Card>
            </CollapsePanel>
        </div>
      </div>
    }
}
