use crate::app::components::{Card, CollapsePanel, InputRow, Panel, PlaylistContext, RadioButtonGroup, TextButton};
use crate::app::context::PlaylistExplorerContext;
use crate::hooks::use_service_context;
use crate::html_if;
use crate::model::{BusyStatus, EventMessage, ExplorerSourceType};
use shared::model::{InputType, PlaylistRequest, PlaylistRequestM3u, PlaylistRequestXtream};
use std::rc::Rc;
use std::str::FromStr;
use web_sys::HtmlInputElement;
use yew::platform::spawn_local;
use yew::prelude::*;
use yew_i18n::use_translation;
use crate::app::components::input::Input;

#[derive(Properties, PartialEq, Clone)]
pub struct PlaylistSourceSelectorProps {
    #[prop_or_default]
    pub hide_title: bool,
    #[prop_or_default]
    pub source_types: Option<Vec<ExplorerSourceType>>,
    #[prop_or_default]
    pub on_select: Option<Callback<PlaylistRequest>>,
}

#[function_component]
pub fn PlaylistSourceSelector(props: &PlaylistSourceSelectorProps) -> Html {
    let translate = use_translation();
    let services_ctx = use_service_context();
    let playlist_ctx = use_context::<PlaylistContext>().expect("Playlist context not found");
    let playlist_explorer_ctx = use_context::<PlaylistExplorerContext>();
    let active_source = use_state(|| ExplorerSourceType::Hosted);
    let loading = use_state(|| false);
    let custom_provider = use_state(|| InputType::Xtream);
    let username_ref = use_node_ref();
    let password_ref = use_node_ref();
    let url_ref = use_node_ref();
    let source_types = use_memo(props.source_types.clone(), |st| {
        let st = st.as_ref().map(|v| v.as_slice()).unwrap_or(&[
            ExplorerSourceType::Hosted, ExplorerSourceType::Provider, ExplorerSourceType::Custom,
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
        let services = services_ctx.clone();
        let set_loading = loading.clone();
        if let Some(on_select) = &props.on_select {
            let on_select = on_select.clone();
            Callback::from(move |request: PlaylistRequest| {
                on_select.emit(request)
            })
        } else {
            let playlist_explorer_ctx_clone = playlist_explorer_ctx.expect("PlaylistExplorer context not found").clone();
            Callback::from(move |request: PlaylistRequest| {
                if !*set_loading {
                    let services = services.clone();
                    let playlist_explorer_ctx_clone = playlist_explorer_ctx_clone.clone();
                    set_loading.set(true);
                    services.event.broadcast(EventMessage::Busy(BusyStatus::Show));
                    let set_loading = set_loading.clone();
                    let req = request;
                    spawn_local(async move {
                        let playlist = services.playlist.get_playlist_categories(&req).await;
                        playlist_explorer_ctx_clone.playlist.set(playlist);
                        playlist_explorer_ctx_clone.playlist_request.set(Some(req));
                        set_loading.set(false);
                        services.event.broadcast(EventMessage::Busy(BusyStatus::Hide));
                    });
                }
            })
        }
    };

    let handle_custom_source = {
        let services = services_ctx.clone();
        let translate = translate.clone();
        let set_custom_provider = custom_provider.clone();
        let handle_source_download = handle_source_download.clone();
        let u_ref = username_ref.clone();
        let p_ref = password_ref.clone();
        let url_ref = url_ref.clone();
        Callback::from(move |_| {
            let is_xtream = matches!(*set_custom_provider, InputType::Xtream);
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
            let (username, password) = if is_xtream {
                let (username, password) = match (u_ref.cast::<HtmlInputElement>(), p_ref.cast::<HtmlInputElement>()) {
                     (Some(u), Some(p)) => (u.value().trim().to_owned(), p.value().trim().to_owned()),
                     _ => (String::new(), String::new())
                 };

                if username.is_empty() || password.is_empty() {
                    services.toastr.error(translate.t("MESSAGES.PLAYLIST_UPDATE.USERNAME_PASSWORD_MANDATORY"));
                    valid = false;
                }
                (Some(username), Some(password))
            } else {
                (None, None)
            };

            if valid {
                let request = if is_xtream {
                    PlaylistRequest::CustomXtream(PlaylistRequestXtream {
                        username: username.unwrap_or_default(),
                        password: password.unwrap_or_default(),
                        url,
                    })
                } else {
                    PlaylistRequest::CustomM3u(PlaylistRequestM3u {
                        url,
                    })
                };
                handle_source_download.emit(request);
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
                                onclick={move |_| handle_click.emit(PlaylistRequest::Target(target.id))}/>
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
                                        if matches!(input.input_type, InputType::M3uBatch | InputType::XtreamBatch) {
                                            None
                                        } else {
                                            Some((input.name.clone(), input.id))
                                        }
                                    },
                                    InputRow::Alias(alias, _input) => {
                                        Some((alias.name.clone(), alias.id))
                                    }
                                };
                                if let Some((name, id)) = result {
                                    html! {
                                    <TextButton name={name.to_string()} title={name.to_string()} icon={"CloudDownload"}
                                    onclick={move |_| handle_click.emit(PlaylistRequest::Input(id))}/>
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
        let username_ref = username_ref.clone();
        let password_ref = password_ref.clone();
        let url_ref = url_ref.clone();
        let set_custom_provider = custom_provider.clone();
        let handle_key_down = handle_key_down.clone();
        move || {
            html! {
                <div class="tp__playlist-source-selector__source-custom">
                  <div class="tp__playlist-source-selector__source-custom-body">
                  {
                    html_if!(matches!(*set_custom_provider, InputType::Xtream), {
                       <>
                        <Input label={translate.t("LABEL.USERNAME")} input_ref={username_ref} name="username" autocomplete={true} />
                        <Input label={translate.t("LABEL.PASSWORD")} input_ref={password_ref} name="password" hidden={true} autocomplete={false} onkeydown={handle_key_down.clone()}/>
                       </>
                      })
                  }
                    <Input label={translate.t("LABEL.URL")} input_ref={url_ref} name="url" autocomplete={true} onkeydown={handle_key_down} />
                    <TextButton name={"custom"} title={translate.t("LABEL.DOWNLOAD")} icon={"CloudDownload"}
                       onclick={handle_custom_source}/>
                  </div>
                </div>
            }
        }
    };

    let set_custom_provider_1 = custom_provider.clone();
    let set_custom_provider_2 = custom_provider.clone();

    html! {
      <div class="tp__playlist-source-selector tp__list-list">
        { html_if!(!props.hide_title, {
            <div class="tp__playlist-source-selector__header tp__list-list__header">
              <h1>{ translate.t("LABEL.SOURCES")}</h1>
            </div>
        })}
        <div class="tp__playlist-source-selector__body tp__list-list__body">
            <CollapsePanel class="tp__playlist-source-selector__source-picker" expanded={true}
               title={translate.t("LABEL.SOURCE_PICKER")}>
               <Card>
                <div class="tp__playlist-source-selector__source-picker__header">
                    <RadioButtonGroup options={source_types.clone()}
                                  selected={Rc::new(vec![(*active_source).to_string()])}
                                  on_select={handle_source_select} />
                    {
                        html_if! {
                        *active_source == ExplorerSourceType::Custom,
                        {
                            <div class="tp__playlist-source-selector__source-custom-options">
                               <TextButton class={if matches!(*custom_provider, InputType::Xtream) {"active"} else {""} }
                                        name={"xtream_source"} title={translate.t("LABEL.XTREAM")} icon={"Playlist"}
                                        onclick={move |_| set_custom_provider_1.set(InputType::Xtream)}/>
                               <TextButton class={if matches!(*custom_provider, InputType::M3u) {"active"} else {""} }
                                        name={"m3u_source"} title={translate.t("LABEL.M3U")} icon={"Playlist"}
                                        onclick={move |_| set_custom_provider_2.set(InputType::M3u)}/>
                            </div>
                        }
                      }
                    }
                </div>
                <div class="tp__playlist-source-selector__source-picker__body">
                    <Panel value={ExplorerSourceType::Hosted.to_string()} active={active_source.to_string()}>
                        { render_hosted() }
                    </Panel>
                    <Panel value={ExplorerSourceType::Provider.to_string()} active={active_source.to_string()}>
                        { render_provider() }
                    </Panel>
                    <Panel value={ExplorerSourceType::Custom.to_string()} active={active_source.to_string()}>
                        { render_custom() }
                    </Panel>
                </div>
              </Card>
            </CollapsePanel>
        </div>
      </div>
    }
}