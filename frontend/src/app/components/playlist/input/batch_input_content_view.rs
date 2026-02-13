use crate::app::components::{CsvTable, NoContent};
use crate::hooks::use_service_context;
use shared::model::ConfigInputDto;
use std::rc::Rc;
use yew::platform::spawn_local;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct BatchInputContentViewProps {
    pub input: Rc<ConfigInputDto>,
}

#[function_component]
pub fn BatchInputContentView(props: &BatchInputContentViewProps) -> Html {
    let services = use_service_context();

    let batch_content = use_state(|| Option::<String>::None);

    {
        let batch_content = batch_content.clone();
        let services_clone = services.clone();
        let input_clone = props.input.clone();
        use_effect_with((services.clone(), props.input.clone()), move |_| {
            let batch_content = batch_content.clone();
            let services = services_clone.clone();
            let input = input_clone.clone();
            spawn_local(async move {
                let content = services.config.get_batch_input_content(&input).await;
                batch_content.set(content);
            });
            || ()
        });
    }

    html! {
        <div class="tp__batch-input-content">
        {
            if let Some(csv) = (*batch_content).as_ref() {
                html! { <CsvTable content={csv.clone()} separator={';'} first_row_is_header={true} /> }
            } else {
                html! { <NoContent /> }
            }
        }
        </div>
    }
}
