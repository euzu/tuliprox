use super::{ItemField, PlaylistItemType};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CompiledRegex {
    pub restr: String,
    pub re: Arc<regex::Regex>,
}

impl PartialEq for CompiledRegex {
    fn eq(&self, other: &Self) -> bool { self.restr == other.restr }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnaryOperator {
    Not,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StringOperator {
    Eq,
    NotEq,
    Contains,
    StartsWith,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PresenceOperator {
    IsEmpty,
    IsNotEmpty,
}

impl std::fmt::Display for PresenceOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Self::IsEmpty => "IS EMPTY",
            Self::IsNotEmpty => "IS NOT EMPTY",
        })
    }
}

impl std::fmt::Display for StringOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match *self {
            Self::Eq => "=",
            Self::NotEq => "!=",
            Self::Contains => "CONTAINS",
            Self::StartsWith => "STARTSWITH",
        })
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NumericOperator {
    Eq,
    NotEq,
    Greater,
    GreaterOrEqual,
    Less,
    LessOrEqual,
}

impl std::fmt::Display for NumericOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match *self {
            Self::Eq => "=",
            Self::NotEq => "!=",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
        })
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BinaryOperator {
    And,
    Or,
}

impl BinaryOperator {
    pub(super) const OP_OR: &'static str = "OR";
    pub(super) const OP_AND: &'static str = "AND";
}

impl std::fmt::Display for BinaryOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                Self::Or => Self::OP_OR,
                Self::And => Self::OP_AND,
            }
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Group(Box<Filter>),
    FieldComparison(ItemField, CompiledRegex),
    TypeComparison(ItemField, PlaylistItemType),
    StringComparison(ItemField, StringOperator, String),
    PresenceComparison(ItemField, PresenceOperator),
    NumericComparison(ItemField, NumericOperator, u32),
    SetComparison(ItemField, Vec<String>),
    UnaryExpression(UnaryOperator, Box<Filter>),
    BinaryExpression(Box<Filter>, BinaryOperator, Box<Filter>),
}

impl Default for Filter {
    fn default() -> Self {
        Self::Group(Box::new(Filter::FieldComparison(
            ItemField::Group,
            CompiledRegex { restr: ".*".to_string(), re: crate::model::REGEX_CACHE.get_or_compile(".*").unwrap() },
        )))
    }
}

impl Filter {
    pub(super) const LIVE: &'static str = "live";
    pub(super) const VOD: &'static str = "vod";
    pub(super) const MOVIE: &'static str = "movie";
    pub(super) const SERIES: &'static str = "series";
    pub(super) const UNSUPPORTED: &'static str = "unsupported";
}

impl std::fmt::Display for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::FieldComparison(field, rewc) => {
                write!(f, "{} ~ \"{}\"", field, String::from(&rewc.restr))
            }
            Self::TypeComparison(field, item_type) => {
                write!(
                    f,
                    "{} = {}",
                    field,
                    match item_type {
                        PlaylistItemType::Live => Self::LIVE,
                        PlaylistItemType::Video | PlaylistItemType::LocalVideo => Self::MOVIE,
                        PlaylistItemType::Series
                        | PlaylistItemType::SeriesInfo
                        | PlaylistItemType::LocalSeries
                        | PlaylistItemType::LocalSeriesInfo => Self::SERIES, // yes series-info is handled as series in filter
                        _ => Self::UNSUPPORTED,
                    }
                )
            }
            Self::Group(stmt) => {
                write!(f, "({stmt})")
            }
            Self::StringComparison(field, op, value) => {
                write!(f, "{field} {op} \"{}\"", value.replace('"', "\\\""))
            }
            Self::PresenceComparison(field, op) => write!(f, "{field} {op}"),
            Self::NumericComparison(field, op, value) => {
                write!(f, "{field} {op} {value}")
            }
            Self::SetComparison(field, values) => {
                let rendered =
                    values.iter().map(|v| format!("\"{}\"", v.replace('"', "\\\""))).collect::<Vec<_>>().join(", ");
                write!(f, "{field} IN [{rendered}]")
            }
            Self::UnaryExpression(op, expr) => {
                let flt = match op {
                    UnaryOperator::Not => format!("NOT {expr}"),
                };
                write!(f, "{flt}")
            }
            Self::BinaryExpression(left, op, right) => {
                write!(f, "{left} {op} {right}")
            }
        }
    }
}
