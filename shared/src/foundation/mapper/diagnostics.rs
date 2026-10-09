#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MappingDiagnostic {
    pub statement: usize,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MappingOutcome {
    pub changed_fields: Vec<String>,
    pub emitted_items: usize,
    pub diagnostics: Vec<MappingDiagnostic>,
}
