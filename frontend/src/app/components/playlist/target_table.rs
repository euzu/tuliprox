use crate::app::components::menu_item::MenuItem;
use crate::app::components::popup_menu::PopupMenu;
use crate::app::components::{convert_bool_to_chip_style, AppIcon, Chip, FilterView, PlaylistMappings, PlaylistProcessing, RevealContent, Table, TableDefinition, TargetOptions, TargetOutput, TargetRename, TargetSort, TargetWatch, ToggleSwitch};
use crate::hooks::use_service_context;
use crate::model::DialogResult;
use crate::services::DialogService;
use shared::error::{info_err_res, TuliproxError};
use shared::model::{ConfigTargetDto, SortOrder};
use std::fmt::Display;
use std::rc::Rc;
use std::str::FromStr;
use yew::platform::spawn_local;
use yew::prelude::*;
use yew_i18n::use_translation;
use crate::html_if;

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

#[function_component]
pub fn TargetTable(props: &TargetTableProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
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

    let render_header_cell = {
        let translator = translate.clone();
        Callback::<usize, Html>::from(move |col| {
            html! {
                {
                    if col < HEADERS.len() {
                       translator.t(HEADERS[col])
                    } else {
                      String::new()
                    }
               }
            }
        })
    };

    let render_data_cell = {
        let translator = translate.clone();
        let popup_onclick = handle_popup_onclick.clone();
        Callback::<(usize, usize, Rc<ConfigTargetDto>), Html>::from(
            move |(row, col, dto): (usize, usize, Rc<ConfigTargetDto>)| {
                match col {
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
                    4 => html! { <TargetOptions target={Rc::clone(&dto)} /> },
                    5 => dto.sort.as_ref().map_or_else(|| html! {}, |_s| html! { <RevealContent><TargetSort target={Rc::clone(&dto)} /></RevealContent> }),
                    6 => dto.t_filter.as_ref().map_or_else(|| html! {}, |f| html! { <RevealContent preview={Some(html!{<FilterView inline={true} filter={f.clone()} />})}><FilterView pretty={true} filter={f.clone()} /></RevealContent> }),
                    7 => dto.rename.as_ref().map_or_else(|| html! {}, |_r| html! { <RevealContent><TargetRename target={Rc::clone(&dto)} /></RevealContent> }),
                    8 => {
                        let mapping_oneliner = dto.mapping.as_ref().map(|v| v.join(", ")).unwrap_or_default();
                        html_if!(!mapping_oneliner.is_empty(),
                            { <RevealContent preview={Some(html! { mapping_oneliner })}><PlaylistMappings mappings={dto.mapping.clone()} /></RevealContent> })
                    },
                    9 => html! { <PlaylistProcessing order={dto.processing_order} /> },
                    10 => html! { <TargetWatch  target={Rc::clone(&dto)} /> },
                    11 => html! { <ToggleSwitch value={dto.use_memory_cache} readonly={true} /> },
                    _ => html! {""},
                }
            })
    };

    let is_sortable = Callback::<usize, bool>::from(move |_col| {
        false
    });

    let on_sort = Callback::<Option<(usize, SortOrder)>, ()>::from(move |_args| {
    });

    let table_definition = {
        // first register for config update
        let render_header_cell_cb = render_header_cell.clone();
        let render_data_cell_cb = render_data_cell.clone();
        let is_sortable = is_sortable.clone();
        let on_sort = on_sort.clone();
        let num_cols = HEADERS.len();
        use_memo(props.targets.clone(), move |targets| {
            targets.as_ref().map(|list|
                Rc::new(TableDefinition::<ConfigTargetDto> {
                    items: if list.is_empty() {None} else {Some(Rc::new(list.clone()))},
                    num_cols,
                    is_sortable,
                    on_sort,
                    render_header_cell: render_header_cell_cb,
                    render_data_cell: render_data_cell_cb,
                }))
        })
    };


    let handle_menu_click = {
        let popup_is_open_state = popup_is_open.clone();
        let confirm = dialog.clone();
        let translate = translate.clone();
        let services_ctx = services.clone();
        let selected_dto = selected_dto.clone();
        Callback::from(move |(name, _): (String, _)| {
            if let Ok(action) = TargetTableAction::from_str(&name) {
                match action {
                    TargetTableAction::Edit => {}
                    TargetTableAction::Refresh => {
                        let translate = translate.clone();
                        let services_ctx = services_ctx.clone();
                        let dto_name = selected_dto.as_ref().map_or_else(String::new, |d| d.name.to_string());
                        spawn_local(async move {
                            let targets = vec![dto_name.as_str()];
                            match services_ctx.playlist.update_targets(&targets).await {
                                true => { services_ctx.toastr.success(translate.t("MESSAGES.PLAYLIST_UPDATE.SUCCESS")); }
                                false => { services_ctx.toastr.error(translate.t("MESSAGES.PLAYLIST_UPDATE.FAIL")); }
                            }
                        });
                    }
                    TargetTableAction::Delete => {
                        let confirm = confirm.clone();
                        let translator = translate.clone();
                        spawn_local(async move {
                            let result = confirm.confirm(&translator.t("MESSAGES.CONFIRM_DELETE")).await;
                            if result == DialogResult::Ok {
                                // TODO edit
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
                        <MenuItem icon="Edit" name={TargetTableAction::Edit.to_string()} label={translate.t("LABEL.EDIT")} onclick={&handle_menu_click}></MenuItem>
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


#[derive(Debug, Clone, Eq, PartialEq)]
enum TargetTableAction {
    Edit,
    Refresh,
    Delete,
}

impl Display for TargetTableAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            Self::Edit => "edit",
            Self::Refresh => "refresh",
            Self::Delete => "delete",
        })
    }
}

impl FromStr for TargetTableAction {
    type Err = TuliproxError;


    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s {
            "edit" => Ok(Self::Edit),
            "refresh" => Ok(Self::Refresh),
            "delete" => Ok(Self::Delete),
            _ => info_err_res!("Unknown Target Action: {}", s),
        }
    }
}