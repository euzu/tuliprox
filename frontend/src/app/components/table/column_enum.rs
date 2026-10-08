/// Static properties of a column, independent of a table's current data or permissions.
pub(crate) struct TableColumnSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub header_label: Option<&'static str>,
    pub can_hide: bool,
    pub content: bool,
    pub sortable: bool,
}

impl TableColumnSpec {
    pub const fn new(id: &'static str, label: &'static str) -> Self {
        Self { id, label, header_label: None, can_hide: true, content: true, sortable: false }
    }

    pub const fn header_label(&self) -> &'static str {
        match self.header_label {
            Some(label) => label,
            None => self.label,
        }
    }

    pub(crate) fn metadata(self) -> super::TableColumn {
        super::TableColumn {
            can_hide: self.can_hide,
            content: self.content,
            sortable: self.sortable,
            ..super::TableColumn::translated(self.id, self.label)
        }
    }
}

/// Declares ordered columns together with their stable IDs, labels and behavior.
macro_rules! define_table_columns {
    (enum $name:ident {
        $($variant:ident => ($id:literal, $label:literal)
            $({ $($property:ident: $value:expr),* $(,)? })?
        ),+ $(,)?
    }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(usize)]
        enum $name { $($variant),+ }

        impl $name {
            const ALL: &'static [Self] = &[$(Self::$variant),+];

            fn from_index(index: usize) -> Option<Self> { Self::ALL.get(index).copied() }

            const fn definition(self) -> $crate::app::components::TableColumnSpec {
                match self {
                    $(Self::$variant => $crate::app::components::TableColumnSpec {
                        $($($property: $value,)*)?
                        ..$crate::app::components::TableColumnSpec::new(self.id(), self.label())
                    }),+
                }
            }

            const fn id(self) -> &'static str { match self { $(Self::$variant => $id),+ } }
            const fn label(self) -> &'static str { match self { $(Self::$variant => $label),+ } }
            const fn header_label(self) -> &'static str { self.definition().header_label() }

            fn columns() -> Vec<$crate::app::components::TableColumn> {
                Self::ALL.iter().map(|column| column.definition().metadata()).collect()
            }
        }
    };
}

pub(crate) use define_table_columns;
