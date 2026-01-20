use crate::app::components::input::Input;
use crate::app::components::{Card, CollapsePanel, InputRow, Panel, PlaylistContext, RadioButtonGroup, TextButton};
use crate::hooks::use_service_context;
use crate::model::ExplorerSourceType;
use shared::model::PlaylistEpgRequest;
use std::rc::Rc;
use std::str::FromStr;
use web_sys::HtmlInputElement;
use yew::prelude::*;
use yew_i18n::use_translation;
use crate::html_if;

#[derive(Properties, PartialEq, Clone)]
pub struct EpgSourceSelectorProps {
    #[prop_or_default]
    pub source_types: Option<Vec<ExplorerSourceType>>,
    #[prop_or_default]
    pub on_select: Callback<PlaylistEpgRequest>,
}

#[function_component]
pub fn EpgSourceSelector(props: &EpgSourceSelectorProps) -> Html {
    let translate = use_translation();
    let services_ctx = use_service_context();
    let playlist_ctx = use_context::<PlaylistContext>().expect("Playlist context not found");
    let active_source = use_state(|| ExplorerSourceType::Hosted);
    let url_ref = use_node_ref();
    let source_types = use_memo(props.source_types.clone(), |st| {
        let st = st.as_ref().map(|v| v.as_slice()).unwrap_or(&[
            ExplorerSourceType::Hosted, /*ExplorerSourceType::Provider,*/ ExplorerSourceType::Custom,
        ]);
        st.iter().map(ToString::to_string).collect::<Vec<String>>()
    });

    let handle_source_select = {
        let active_source_clone = active_source.clone();
        Callback::from(move |source_selection: Rc<Vec<String>>| {
            if let Some(source_type_str) = source_selection.first() {
                if let Ok(source_type) = ExplorerSourceType::from_str(source_type_str) {
                    active_source_clone.set(source_type)
                }
            }
        })
    };

    let handle_source_download = {
        let on_select = props.on_select.clone();
        Callback::from(move |request: PlaylistEpgRequest| {
            on_select.emit(request)
        })
    };

    let handle_custom_source = {
        let services = services_ctx.clone();
        let translate = translate.clone();
        let handle_source_download = handle_source_download.clone();
        let url_ref = url_ref.clone();
        Callback::from(move |_| {
            let url = match url_ref.cast::<HtmlInputElement>() {
                Some(input) => input.value().trim().to_owned(),
                None => {
                    services.toastr.error(translate.t("MESSAGES.PLAYLIST_UPDATE.URL_MANDATORY"));
                    return;
                }
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

    let handle_key_down = {
        let handle_custom_source = handle_custom_source.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                handle_custom_source.emit("custom".to_owned());
            }
        })
    };

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
                        { for data.iter().flat_map(|(inputs, _targets)| inputs)
                            .map(Rc::clone)
                            .map(|provider| {
                                let handle_click = handle_defined_source.clone();
                                let result = match &*provider {
                                    InputRow::Input(input) => {
                                       Some((input.name.clone(), input.id))
                                    },
                                    InputRow::Alias(alias, _input) => {
                                        Some((alias.name.clone(), alias.id))
                                    }
                                };
                                if let Some((name, id)) = result {
                                    html! {
                                    <TextButton name={name.to_string()} title={name.to_string()} icon={"CloudDownload"}
                                    onclick={move |_| handle_click.emit(PlaylistEpgRequest::Input(id))}/>
                                    }
                                } else {
                                    html!{}
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
                     <Input label={translate.t("LABEL.URL")} input_ref={url_ref} name="url" autocomplete={true} onkeydown={handle_key_down} />
                     <TextButton name={"custom"} title={translate.t("LABEL.DOWNLOAD")} icon={"CloudDownload"}
                       onclick={handle_custom_source}/>
                  </div>
                </div>
            }
        }
    };

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
                        <Panel value={ExplorerSourceType::Hosted.to_string()} active={active_source.to_string()}>
                            { render_hosted() }
                        </Panel>
                    })}
                    { html_if!(source_types.contains(&ExplorerSourceType::Provider.to_string()), {
                        <Panel value={ExplorerSourceType::Provider.to_string()} active={active_source.to_string()}>
                            { render_provider() }
                        </Panel>
                    })}
                    { html_if!(source_types.contains(&ExplorerSourceType::Custom.to_string()), {
                        <Panel value={ExplorerSourceType::Custom.to_string()} active={active_source.to_string()}>
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