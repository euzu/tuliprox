use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct TitledCardProps {
    pub title: AttrValue,
    #[prop_or_default]
    pub children: Children,
}

#[component(TitledCard)]
pub fn titled_card(props: &TitledCardProps) -> Html {
    html! {
        <div class="tp__titled-card">
            <span class="tp__titled-card__title">{ props.title.clone() }</span>
            <div class="tp__titled-card__content">
                { for props.children.iter() }
            </div>
        </div>
    }
}
