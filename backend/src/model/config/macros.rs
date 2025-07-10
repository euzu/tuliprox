#[macro_export]
macro_rules! from_impl {
    ($struct_name:ident) => {
        paste::paste! {
            impl From<[<$struct_name Dto>]> for $struct_name {
                fn from(dto: [<$struct_name Dto>]) -> Self {
                    $struct_name::from(&dto)
                }
            }
        }
    }
}

pub use from_impl;

#[macro_export]
macro_rules! try_from_impl {
    ($struct_name:ident) => {
        paste::paste! {
            impl TryFrom<[<$struct_name Dto>]> for $struct_name {
                type Error = shared::error::TuliproxError;
                fn try_from(dto: [<$struct_name Dto>]) -> Result<Self, shared::error::TuliproxError> {
                    $struct_name::try_from(&dto)
                }
            }
        }
    }
}

pub use try_from_impl;