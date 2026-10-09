use crate::error::TuliproxError;
use std::{fmt::Display, str::FromStr};

#[derive(Debug, Clone, PartialEq)]
pub enum BuiltInFunction {
    Concat,
    Uppercase,
    Lowercase,
    Capitalize,
    Split,
    Trim,
    Print,
    ToNumber,
    First,
    Template,
    Replace,
    Pad,
    Format,
    AddFavourite,
}

impl FromStr for BuiltInFunction {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "concat" => Ok(Self::Concat),
            "capitalize" => Ok(Self::Capitalize),
            "lowercase" => Ok(Self::Lowercase),
            "uppercase" => Ok(Self::Uppercase),
            "split" => Ok(Self::Split),
            "trim" => Ok(Self::Trim),
            "print" => Ok(Self::Print),
            "number" => Ok(Self::ToNumber),
            "first" => Ok(Self::First),
            "template" => Ok(Self::Template),
            "replace" => Ok(Self::Replace),
            "pad" => Ok(Self::Pad),
            "format" => Ok(Self::Format),
            "add_favourite" => Ok(Self::AddFavourite),
            _ => Err(TuliproxError::Mapper(format!("Unknown function {s}"))),
        }
    }
}

impl Display for BuiltInFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Concat => "concat",
            Self::Capitalize => "capitalize",
            Self::Lowercase => "lowercase",
            Self::Uppercase => "uppercase",
            Self::Split => "split",
            Self::Trim => "trim",
            Self::Print => "print",
            Self::ToNumber => "number",
            Self::First => "first",
            Self::Template => "template",
            Self::Replace => "replace",
            Self::Pad => "pad",
            Self::Format => "format",
            Self::AddFavourite => "add_favourite",
        })
    }
}
