use shared::model::TableLayoutPreferencesDto;
use std::rc::Rc;
use yew::AttrValue;

#[derive(Debug, Clone, PartialEq)]
pub enum TableColumnLabel {
    Text(AttrValue),
    TranslationKey(AttrValue),
}

impl TableColumnLabel {
    pub fn resolve(&self, translate: impl FnOnce(&str) -> String) -> AttrValue {
        match self {
            Self::Text(text) => text.clone(),
            Self::TranslationKey(key) => translate(key.as_str()).into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableColumn {
    pub id: AttrValue,
    pub label: TableColumnLabel,
    pub default_visible: bool,
    pub can_hide: bool,
    pub can_reorder: bool,
    pub available: bool,
    pub content: bool,
    pub sortable: bool,
}

impl TableColumn {
    /// Builds a column with a literal display label.
    pub fn new(id: impl Into<AttrValue>, label: impl Into<AttrValue>) -> Self {
        Self::with_label(id, TableColumnLabel::Text(label.into()))
    }

    pub fn translated(id: impl Into<AttrValue>, key: impl Into<AttrValue>) -> Self {
        Self::with_label(id, TableColumnLabel::TranslationKey(key.into()))
    }

    fn with_label(id: impl Into<AttrValue>, label: TableColumnLabel) -> Self {
        Self {
            id: id.into(),
            label,
            default_visible: true,
            can_hide: true,
            can_reorder: true,
            available: true,
            content: true,
            sortable: false,
        }
    }
}

pub fn column_order(columns: &[TableColumn], layout: &TableLayoutPreferencesDto) -> Vec<usize> {
    let mut order = Vec::with_capacity(columns.len());
    for id in &layout.column_order {
        if let Some(index) = columns.iter().position(|column| column.available && column.id.as_str() == id) {
            if !order.contains(&index) {
                order.push(index);
            }
        }
    }
    for (index, column) in columns.iter().enumerate() {
        if column.available && !order.contains(&index) {
            order.push(index);
        }
    }
    for (position, (index, column)) in columns.iter().enumerate().filter(|(_, c)| c.available).enumerate() {
        if !column.can_reorder {
            order.retain(|value| *value != index);
            order.insert(position.min(order.len()), index);
        }
    }
    order
}

pub fn visible_columns(columns: &[TableColumn], layout: &TableLayoutPreferencesDto) -> Vec<usize> {
    let order = column_order(columns, layout);
    let mut visible: Vec<_> = order
        .iter()
        .copied()
        .filter(|index| {
            let column = &columns[*index];
            !column.can_hide
                || layout.column_visibility.get(column.id.as_str()).copied().unwrap_or(column.default_visible)
        })
        .collect();
    if !visible.iter().any(|index| columns[*index].content) {
        if let Some(index) = order.iter().copied().find(|index| columns[*index].content) {
            let position = order.iter().position(|value| *value == index).unwrap_or(0);
            let insertion = visible
                .iter()
                .filter(|value| order.iter().position(|i| i == *value).is_some_and(|p| p < position))
                .count();
            visible.insert(insertion, index);
        }
    }
    visible
}

#[derive(Clone, PartialEq)]
pub struct TablePanelSpec {
    pub table_id: AttrValue,
    pub columns: Rc<Vec<TableColumn>>,
    pub supported: bool,
    pub anchor: Option<web_sys::Element>,
}

#[cfg(test)]
#[path = "layout.test.rs"]
mod tests;
