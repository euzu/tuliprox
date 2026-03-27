#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DialogResult {
    Ok,
    Cancel,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DialogAction {
    pub name: String,
    pub icon: Option<String>,
    pub label: String,
    pub style: Option<String>,
    pub result: DialogResult,
    pub focus: bool,
    pub disabled: bool,
}

impl DialogAction {
    pub(crate) fn new(
        name: &str,
        label: &str,
        result: DialogResult,
        icon: Option<String>,
        style: Option<String>,
    ) -> Self {
        Self { name: name.to_string(), label: label.to_string(), icon, style, result, focus: false, disabled: false }
    }
    pub(crate) fn new_focused(
        name: &str,
        label: &str,
        result: DialogResult,
        icon: Option<String>,
        style: Option<String>,
    ) -> Self {
        let mut result = Self::new(name, label, result, icon, style);
        result.focus = true;
        result
    }

    pub(crate) fn with_disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DialogActions {
    pub left: Option<Vec<DialogAction>>,
    pub right: Vec<DialogAction>,
}
