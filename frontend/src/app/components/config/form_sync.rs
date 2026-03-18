use crate::app::components::config::{ConfigForm, HasFormData};
use yew::prelude::*;

#[hook]
pub fn use_emit_mapped<Deps, Output, F>(deps: Deps, on_change: Callback<Output>, build_output: F)
where
    Deps: Clone + PartialEq + 'static,
    Output: 'static,
    F: Fn(Deps) -> Output + 'static,
{
    use_effect_with(deps, move |deps| {
        on_change.emit(build_output(deps.clone()));
        || ()
    });
}

#[hook]
pub fn use_emit_mapped_option<Deps, Output, F>(deps: Deps, on_change: Callback<Output>, build_output: F)
where
    Deps: Clone + PartialEq + 'static,
    Output: 'static,
    F: Fn(Deps) -> Option<Output> + 'static,
{
    use_effect_with(deps, move |deps| {
        if let Some(output) = build_output(deps.clone()) {
            on_change.emit(output);
        }
        || ()
    });
}

#[hook]
pub fn use_emit_reducer_state<State, Output, F>(
    state: &UseReducerHandle<State>,
    on_change: Callback<Output>,
    build_output: F,
) where
    State: Reducible + HasFormData + PartialEq + 'static,
    <State as HasFormData>::Data: Clone + PartialEq + 'static,
    Output: 'static,
    F: Fn(bool, <State as HasFormData>::Data) -> Output + 'static,
{
    let form = state.data().clone();
    let modified = state.modified();

    use_emit_mapped((form, modified), on_change, move |(form, modified)| build_output(modified, form));
}

#[hook]
pub fn use_emit_config_form<State, F>(
    state: &UseReducerHandle<State>,
    on_form_change: Callback<ConfigForm>,
    build_form: F,
) where
    State: Reducible + HasFormData + PartialEq + 'static,
    <State as HasFormData>::Data: Clone + PartialEq + 'static,
    F: Fn(bool, <State as HasFormData>::Data) -> ConfigForm + 'static,
{
    use_emit_reducer_state(state, on_form_change, build_form);
}
