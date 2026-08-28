use super::{
    common::CommonInputForm, mutate_staged, staged_type_from_selection, staged_type_options, ConfigInputFormAction,
    ConfigInputFormState, LABEL_CLUSTER, LABEL_TYPE,
};
use crate::{
    app::components::{ClusterFlagsInput, ClusterFlagsInputMode, RadioButtonGroup},
    config_field_child,
    i18n::use_translation,
};
use shared::model::{ClusterFlags, StagedInputType};
use std::rc::Rc;
use yew::{component, html, Callback, Html, Properties, UseReducerHandle};

#[derive(Properties, Clone)]
pub(super) struct StagedInputFormProps {
    pub state: UseReducerHandle<ConfigInputFormState>,
    pub allow_write: bool,
}

impl PartialEq for StagedInputFormProps {
    fn eq(&self, _other: &Self) -> bool { false }
}

#[component]
pub(super) fn StagedInputForm(props: &StagedInputFormProps) -> Html {
    let translate = use_translation();
    let state = props.state.clone();
    let staged_type = state.form.staged_type;
    let clusters = state.form.staged.as_ref().map(|staged| staged.clusters);
    let extra = if props.allow_write {
        html! {
            <div class="tp__config-view__cols-2">
                { config_field_child!(translate.t(LABEL_TYPE), "INPUT_FORM.STAGED_TYPE", {
                    let state = state.clone();
                    html! { <RadioButtonGroup multi_select={false} none_allowed={false}
                        options={staged_type_options()} labels={Some(Rc::new(vec![translate.t("LABEL.M3U"), translate.t("LABEL.XTREAM")]))}
                        selected={Rc::new(vec![staged_type.to_string()])}
                        on_select={Callback::from(move |values: Rc<Vec<String>>| state.dispatch(ConfigInputFormAction::StagedType(staged_type_from_selection(&values))))} /> }
                })}
                { config_field_child!(translate.t(LABEL_CLUSTER), "INPUT_FORM.STAGED_CLUSTERS", {
                    let state = state.clone();
                    html! { <ClusterFlagsInput name="staged_clusters" value={clusters}
                        mode={ClusterFlagsInputMode::NoneIsAll}
                        on_change={Callback::from(move |(_, flags): (String, Option<ClusterFlags>)| {
                            let clusters = flags.filter(|flags| !flags.is_empty()).unwrap_or_else(ClusterFlags::all);
                            let staged = mutate_staged(&state.form.staged, |staged| staged.clusters = clusters);
                            state.dispatch(ConfigInputFormAction::Staged(staged));
                        })} /> }
                })}
            </div>
        }
    } else {
        html! {
            <div class="tp__config-view__cols-2">
                { crate::config_field_custom!(translate.t(LABEL_TYPE), staged_type.to_string()) }
                { crate::config_field_custom!(translate.t(LABEL_CLUSTER), clusters.map_or_else(String::new, |value| value.to_string())) }
            </div>
        }
    };
    html! {
        <CommonInputForm state={state} allow_write={props.allow_write} simple_url={true}
            credentials={staged_type == StagedInputType::Xtream} staged_persist={true}
            sequential_group={false} extra={extra} />
    }
}
