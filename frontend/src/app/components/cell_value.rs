use shared::model::{ProxyType, ProxyUserStatus};

pub enum CellValue<'a> {
    Empty,
    Bool(bool),
    Status(ProxyUserStatus),
    Proxy(ProxyType),
    Text(&'a str),
    Date(i64),
}

impl<'a> PartialOrd for CellValue<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a> Ord for CellValue<'a> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (CellValue::Empty, CellValue::Empty) => std::cmp::Ordering::Equal,
            (CellValue::Bool(a), CellValue::Bool(b)) => a.cmp(b),
            (CellValue::Status(a), CellValue::Status(b)) => a.cmp(b),
            (CellValue::Proxy(a), CellValue::Proxy(b)) => a.cmp(b),
            (CellValue::Text(a), CellValue::Text(b)) => a.cmp(b),
            (CellValue::Date(a), CellValue::Date(b)) => a.cmp(b),

            (CellValue::Empty, _) => std::cmp::Ordering::Less,
            (CellValue::Bool(_), CellValue::Empty) => std::cmp::Ordering::Greater,
            (CellValue::Bool(_), _) => std::cmp::Ordering::Less,
            (CellValue::Status(_), CellValue::Empty | CellValue::Bool(_)) => {
                std::cmp::Ordering::Greater
            }
            (CellValue::Status(_), _) => std::cmp::Ordering::Less,
            (CellValue::Proxy(_), CellValue::Empty | CellValue::Bool(_) | CellValue::Status(_)) => {
                std::cmp::Ordering::Greater
            }
            (CellValue::Proxy(_), _) => std::cmp::Ordering::Less,
            (CellValue::Text(_), CellValue::Date(_)) => std::cmp::Ordering::Less,
            (CellValue::Text(_), _) => std::cmp::Ordering::Greater,
            (CellValue::Date(_), _) => std::cmp::Ordering::Greater,
        }
    }
}

impl<'a> PartialEq for CellValue<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl<'a> Eq for CellValue<'a> {}
