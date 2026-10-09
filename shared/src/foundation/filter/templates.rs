use super::{PatternTemplate, TemplateValue};
use crate::{
    error::TuliproxError,
    utils::{DirectedGraph, CONSTANTS},
};
use indexmap::IndexSet;
use log::error;
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

fn build_dependency_graph(templates: &Vec<PatternTemplate>) -> Result<DirectedGraph<String>, TuliproxError> {
    let mut graph = DirectedGraph::<String>::new();
    for template in templates {
        graph.add_node(&template.name);
        let mut handle_template_value = |value| {
            CONSTANTS
                .re_template_var
                .captures_iter(value)
                .filter(|caps| caps.len() > 1)
                .filter_map(|caps| caps.get(1))
                .map(|caps| String::from(caps.as_str()))
                .for_each(|e| {
                    graph.add_node(&e);
                    graph.add_edge(&template.name, &e);
                });
        };
        match &template.value {
            TemplateValue::Single(value) => handle_template_value(value),
            TemplateValue::Multi(values) => values.iter().for_each(|value| handle_template_value(value)),
        }
    }
    let cycles = graph.find_cycles();
    for cyclic in &cycles {
        error!("Cyclic template dependencies detected [{}]", cyclic.join(" <-> "));
    }
    if !cycles.is_empty() {
        return Err(TuliproxError::FilterParse("Cyclic dependencies in templates detected!".to_string()));
    }
    Ok(graph)
}

fn apply_dependency_template_value(
    template_value: TemplateValue,
    placeholder: &str,
    dependency_value: &TemplateValue,
) -> TemplateValue {
    match dependency_value {
        TemplateValue::Single(dep_val) => match template_value {
            TemplateValue::Single(templ_val) => {
                if templ_val.contains(placeholder) {
                    TemplateValue::Single(templ_val.replace(placeholder, dep_val.as_str()))
                } else {
                    TemplateValue::Single(templ_val)
                }
            }
            TemplateValue::Multi(templ_vals) => {
                let mut new_values = IndexSet::new();
                for templ_val in templ_vals {
                    if templ_val.contains(placeholder) {
                        new_values.insert(templ_val.replace(placeholder, dep_val.as_str()));
                    } else {
                        new_values.insert(templ_val);
                    }
                }
                TemplateValue::Multi(new_values.into_iter().collect())
            }
        },
        TemplateValue::Multi(dep_vals) => match template_value {
            TemplateValue::Single(templ_val) => {
                if templ_val.contains(placeholder) {
                    let mut new_values = IndexSet::new();
                    for dep_val in dep_vals {
                        new_values.insert(templ_val.replace(placeholder, dep_val.as_str()));
                    }
                    TemplateValue::Multi(new_values.into_iter().collect())
                } else {
                    TemplateValue::Single(templ_val)
                }
            }
            TemplateValue::Multi(templ_vals) => {
                let mut new_values = IndexSet::new();
                for templ_val in templ_vals {
                    if templ_val.contains(placeholder) {
                        for dep_val in dep_vals {
                            new_values.insert(templ_val.replace(placeholder, dep_val.as_str()));
                        }
                    } else {
                        new_values.insert(templ_val);
                    }
                }
                TemplateValue::Multi(new_values.into_iter().collect())
            }
        },
    }
}

pub fn prepare_templates(templates: &mut Vec<PatternTemplate>) -> Result<Vec<PatternTemplate>, TuliproxError> {
    let mut seen_template_names: HashSet<&str> = HashSet::with_capacity(templates.len());
    for template in templates.iter() {
        if !seen_template_names.insert(template.name.as_str()) {
            return Err(TuliproxError::FilterParse(format!("Duplicate template name found: {}", template.name)));
        }
    }

    let graph = build_dependency_graph(templates)?;
    let mut template_values = HashMap::new();
    let mut template_map = HashMap::with_capacity(templates.len());

    for item in templates.iter_mut() {
        item.prepare();
        template_values.insert(item.name.clone(), item.value.clone());
        template_map.insert(item.name.clone(), item);
    }

    if let Some(dependencies) = graph.get_dependencies() {
        if let Some(sorted) = graph.topological_sort() {
            for template_name in sorted {
                if let Some(depends_on) = dependencies.get(&template_name) {
                    let mut templ_value = template_values.get(&template_name).unwrap().clone();
                    for dep_templ_name in depends_on {
                        let dep_value = template_values.get(dep_templ_name).ok_or_else(|| {
                            TuliproxError::FilterParse(format!("Failed to load template {dep_templ_name}"))
                        })?;
                        let dep_templ = template_map.get_mut(dep_templ_name).unwrap();
                        templ_value = apply_dependency_template_value(templ_value, &dep_templ.placeholder, dep_value);
                    }
                    template_values.insert(template_name.clone(), templ_value);
                }
            }

            for (k, v) in template_values {
                let template = template_map.get_mut(&k).unwrap();
                template.value = v;
            }
        }
    }
    let result: Vec<PatternTemplate> = template_map.iter_mut().map(|(_, t)| t.clone()).collect();
    Ok(result)
}

pub fn apply_templates_to_pattern(
    pattern: &str,
    templates_list: Option<&[PatternTemplate]>,
    allow_multi: bool,
) -> Result<TemplateValue, TuliproxError> {
    let mut new_pattern = TemplateValue::Single(pattern.to_string());

    if let Some(templates) = templates_list {
        for template in templates {
            new_pattern = apply_dependency_template_value(new_pattern, &template.placeholder, &template.value);
        }
    }

    if !allow_multi {
        match &new_pattern {
            TemplateValue::Single(_) => {}
            TemplateValue::Multi(multi_vals) => match multi_vals.len().cmp(&1) {
                Ordering::Less => {
                    return Err(TuliproxError::FilterParse(format!(
                        "Empty multi value templates are not supported for pattern! {pattern}"
                    )));
                }
                Ordering::Equal => {
                    new_pattern = TemplateValue::Single(multi_vals.first().unwrap().to_owned());
                }
                Ordering::Greater => {
                    return Err(TuliproxError::FilterParse(format!(
                        "Multi value templates are not supported for pattern! {pattern}"
                    )));
                }
            },
        }
    }

    Ok(new_pattern)
}

pub fn apply_templates_to_pattern_single(
    pattern: &str,
    templates: Option<&[PatternTemplate]>,
) -> Result<String, TuliproxError> {
    match apply_templates_to_pattern(pattern, templates, false)? {
        TemplateValue::Single(value) => Ok(value),
        TemplateValue::Multi(_) => {
            Err(TuliproxError::FilterParse("Multi value templates are not supported for pattern!".to_string()))
        }
    }
}
