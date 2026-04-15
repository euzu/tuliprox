use crate::{hooks::use_service_context, i18n::use_translation, utils::t_safe};
use yew::{classes, component, html, AttrValue, Html, Properties};

fn display_country_code(code: Option<&str>) -> Option<String> {
    let normalized = code?.trim().to_ascii_uppercase();
    let is_iso_country = normalized.len() == 2 && normalized.as_bytes().iter().all(|byte| byte.is_ascii_alphabetic());
    if is_iso_country {
        Some(normalized)
    } else {
        None
    }
}

#[derive(Properties, Clone, PartialEq)]
pub struct CountryProps {
    pub country_code: Option<String>,
    #[prop_or_default]
    pub classes: Option<String>,
}

#[component]
pub fn Country(props: &CountryProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();

    if let Some(code) = display_country_code(props.country_code.as_deref()).as_ref() {
        let country = t_safe(&translate, &format!("COUNTRY.{code}")).unwrap_or_else(|| code.clone());
        let flag_svg = services.flags.get_flag(code);
        return html! {
        <span class={classes!("tp__country", props.classes.as_ref())}>
            if let Some(svg) = flag_svg.as_ref() {
                <span class="tp__country__flag" aria-hidden="true">
                    // SAFETY: flags.dat is built offline by flags_builder from a trusted flag directory.
                    // If this source ever becomes user-controlled, replace this with sanitized SVG rendering.
                    {Html::from_html_unchecked(AttrValue::from(svg.clone()))}
                    </span>
            }
            <span class="tp__country__name">{&country}</span>
        </span>
        };
    }

    html! {}
}

#[cfg(test)]
mod tests {
    use super::display_country_code;

    #[test]
    fn display_country_code_filters_special_network_labels() {
        assert_eq!(display_country_code(Some("de")), Some("DE".to_string()));
        assert_eq!(display_country_code(Some("LOOPBACK")), None);
        assert_eq!(display_country_code(Some("LAN")), None);
        assert_eq!(display_country_code(Some("  ")), None);
    }
}
