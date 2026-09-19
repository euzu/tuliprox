use crate::{
    app::{
        components::{
            convert_bool_to_chip_style, make_translated_header_callback, menu_item::MenuItem, popup_menu::PopupMenu,
            AppIcon, Chip, FilterView, PlaylistMappings, PlaylistProcessing, RevealContent, Table, TableDefinition,
            TargetOptions, TargetOutput, TargetRename, TargetSort, TargetWatch, ToggleSwitch,
        },
        ConfigContext,
    },
    hooks::use_service_context,
    html_if,
    i18n::use_translation,
    model::DialogResult,
    services::DialogService,
};
use shared::model::{ConfigTargetDto, SortOrder};
use std::{rc::Rc, str::FromStr};
use yew::{platform::spawn_local, prelude::*};

const HEADERS: [&str; 12] = [
    "LABEL.EMPTY",
    "LABEL.ENABLED",
    "LABEL.NAME",
    "LABEL.OUTPUT",
    "LABEL.OPTIONS",
    "LABEL.SORT",
    "LABEL.FILTER",
    "LABEL.RENAME",
    "LABEL.MAPPING",
    "LABEL.PROCESSING_ORDER",
    "LABEL.WATCH",
    "LABEL.USE_MEMORY_CACHE",
];

#[derive(Properties, PartialEq, Clone)]
pub struct TargetTableProps {
    pub targets: Option<Vec<Rc<ConfigTargetDto>>>,
}

#[component]
pub fn TargetTable(props: &TargetTableProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let selected_dto = use_state(|| None::<Rc<ConfigTargetDto>>);

    let handle_popup_close = {
        let set_is_open = popup_is_open.clone();
        Callback::from(move |()| {
            set_is_open.set(false);
        })
    };

    let handle_popup_onclick = {
        let set_selected_dto = selected_dto.clone();
        let set_anchor_ref = popup_anchor_ref.clone();
        let set_is_open = popup_is_open.clone();
        Callback::from(move |(dto, event): (Rc<ConfigTargetDto>, MouseEvent)| {
            if let Some(target) = event.target_dyn_into::<web_sys::Element>() {
                set_selected_dto.set(Some(dto.clone()));
                set_anchor_ref.set(Some(target));
                set_is_open.set(true);
            }
        })
    };

    let render_header_cell = make_translated_header_callback(translate.clone(), &HEADERS);

    let render_data_cell = {
        let translator = translate.clone();
        let popup_onclick = handle_popup_onclick.clone();
        Callback::<(usize, usize, Rc<ConfigTargetDto>), Html>::from(
            move |(row, col, dto): (usize, usize, Rc<ConfigTargetDto>)| match col {
                0 => {
                    let popup_onclick = popup_onclick.clone();
                    html! {
                        <button class="tp__icon-button"
                            onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((dto.clone(), event)))}
                            data-row={row.to_string()}>
                            <AppIcon name="Popup"></AppIcon>
                        </button>
                    }
                }
                1 => html! { <Chip class={ convert_bool_to_chip_style(dto.enabled) }
                label={if dto.enabled {translator.t("LABEL.ACTIVE")} else { translator.t("LABEL.DISABLED")} }
                 /> },
                2 => html! { dto.name.as_str() },
                3 => html! { <TargetOutput target={Rc::clone(&dto)} /> },
                4 => {
                    html! { <RevealContent preview={ html!{translator.t("LABEL.SETTINGS")}}><TargetOptions target={Rc::clone(&dto)} /></RevealContent> }
                }
                5 => dto.sort.as_ref().map_or_else(
                    || html! {},
                    |_s| html! { <RevealContent><TargetSort target={Rc::clone(&dto)} /></RevealContent> },
                ),
                6 => {
                    let filters = [
                        (translator.t("LABEL.FILTER"), dto.filter.t_processing.as_ref()),
                        (translator.t("LABEL.PERSIST_FILTER"), dto.filter.t_persist.as_ref()),
                    ];
                    let rendered = filters.into_iter().filter_map(|(label, filter)| {
                        filter.map(|filter| {
                            html! {
                                <div>
                                    <strong>{label}</strong>
                                    <FilterView pretty={true} filter={filter.clone()} />
                                </div>
                            }
                        })
                    });
                    html! { <RevealContent>{ for rendered }</RevealContent> }
                }
                7 => dto.rename.as_ref().map_or_else(
                    || html! {},
                    |_r| html! { <RevealContent><TargetRename target={Rc::clone(&dto)} /></RevealContent> },
                ),
                8 => {
                    let mapping_oneliner = dto.mapping.as_ref().map(|v| v.join(", ")).unwrap_or_default();
                    html_if!(!mapping_oneliner.is_empty(),
                            { <RevealContent preview={Some(html! { mapping_oneliner })}><PlaylistMappings mappings={dto.mapping.clone()} /></RevealContent> })
                }
                9 => html! { <PlaylistProcessing order={dto.processing_order} /> },
                10 => html! { <TargetWatch  target={Rc::clone(&dto)} /> },
                11 => html! { <ToggleSwitch value={dto.use_memory_cache} readonly={true} /> },
                _ => html! {""},
            },
        )
    };

    let is_sortable = Callback::<usize, bool>::from(move |_col| false);

    let on_sort = Callback::<Option<(usize, SortOrder)>, ()>::from(move |_args| {});

    let table_definition = {
        // first register for config update
        let render_header_cell_cb = render_header_cell.clone();
        let render_data_cell_cb = render_data_cell.clone();
        let is_sortable = is_sortable.clone();
        let on_sort = on_sort.clone();
        let num_cols = HEADERS.len();
        use_memo(props.targets.clone(), move |targets| {
            targets.as_ref().map(|list| {
                Rc::new(TableDefinition::<ConfigTargetDto> {
                    items: if list.is_empty() { None } else { Some(Rc::new(list.clone())) },
                    num_cols,
                    is_sortable,
                    on_sort,
                    render_header_cell: render_header_cell_cb,
                    render_data_cell: render_data_cell_cb,
                })
            })
        })
    };

    let handle_menu_click = {
        let popup_is_open_state = popup_is_open.clone();
        let confirm = dialog.clone();
        let translate = translate.clone();
        let services_ctx = services.clone();
        let selected_dto = selected_dto.clone();
        let config_ctx = config_ctx.clone();
        Callback::from(move |(name, _): (String, _)| {
            if let Ok(action) = TargetTableAction::from_str(&name) {
                match action {
                    TargetTableAction::Refresh => {
                        let translate = translate.clone();
                        let services_ctx = services_ctx.clone();
                        let dto_name = selected_dto.as_ref().map_or_else(String::new, |d| d.name.clone());
                        spawn_local(async move {
                            let targets = vec![dto_name.as_str()];
                            if services_ctx.playlist.update_targets(&targets).await {
                                services_ctx.toastr.success(translate.t("MESSAGES.PLAYLIST_UPDATE.SUCCESS"));
                            } else {
                                services_ctx.toastr.error(translate.t("MESSAGES.PLAYLIST_UPDATE.FAIL"));
                            }
                        });
                    }
                    TargetTableAction::Delete => {
                        let confirm = confirm.clone();
                        let translator = translate.clone();
                        let services_ctx = services_ctx.clone();
                        let config_ctx = config_ctx.clone();
                        let target_name = selected_dto.as_ref().map_or_else(String::new, |d| d.name.clone());
                        spawn_local(async move {
                            let result = confirm.confirm(&translator.t("MESSAGES.CONFIRM_DELETE")).await;
                            if result != DialogResult::Ok {
                                return;
                            }
                            let Some(app_config) = config_ctx.config.as_ref() else {
                                return;
                            };
                            let mut sources = app_config.sources.clone();
                            for source in &mut sources.sources {
                                source.targets.retain(|t| t.name != target_name);
                            }
                            match services_ctx.config.save_sources(sources).await {
                                Ok(()) => {
                                    services_ctx.toastr.success(translator.t("MESSAGES.SAVE.SOURCES_CONFIG.SUCCESS"));
                                    let _ = services_ctx.config.get_server_config().await;
                                }
                                Err(err) => services_ctx.toastr.error(err.to_string()),
                            }
                        });
                    }
                }
            }
            popup_is_open_state.set(false);
        })
    };

    html! {
        <div class="tp__target-table">
          {
            if let Some(definition) = table_definition.as_ref() {
                html! {
                  <>
                   <Table::<ConfigTargetDto> definition={definition.clone()} />
                    <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
                        <MenuItem icon="Refresh" name={TargetTableAction::Refresh.to_string()} label={translate.t("LABEL.REFRESH")} onclick={&handle_menu_click} class="tp__update_action"></MenuItem>
                        <hr/>
                        <MenuItem icon="Delete" name={TargetTableAction::Delete.to_string()} label={translate.t("LABEL.DELETE")} onclick={&handle_menu_click} class="tp__delete_action"></MenuItem>
                    </PopupMenu>
                </>
                  }
            } else {
              html! {}
            }
          }
        </div>
    }
}

#[derive(Debug, Clone, Eq, PartialEq, strum_macros::Display, strum_macros::EnumString)]
#[strum(serialize_all = "snake_case")]
enum TargetTableAction {
    Refresh,
    Delete,
}
