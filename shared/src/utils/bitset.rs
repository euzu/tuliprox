#[macro_export]
macro_rules! create_bitset {
    ($storage:ty, $enum_name:ident, $($variant:ident),*) => {
        paste::paste! {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[repr($storage)]
        pub enum $enum_name {
            $($variant),*
        }
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[repr(transparent)]
        #[serde(transparent)]
        pub struct [<$enum_name Set>](pub $storage);

        impl [<$enum_name Set>] {
            // COMPILE-TIME CHECK:
            // This ensures at compile time that the largest enum variant
            // doesn't exceed the storage capacity. Zero runtime cost.
            const _CAPACITY_CHECK: () = {
                let mut max_val = 0;
                $( if ($enum_name::$variant as usize) > max_val { max_val = $enum_name::$variant as usize; } )*
                if max_val >= <$storage>::BITS as usize {
                    panic!("BitSet storage too small for enum variants!");
                }
            };

            #[inline(always)]
            pub const fn new() -> Self {
                Self(0)
            }

           #[inline(always)]
            pub fn set(&mut self, variant: $enum_name) {
                // LLVM can optimize this to a single 'bts' or 'or' instruction
                self.0 |= 1 << (variant as $storage);
            }

            #[inline(always)]
            pub fn unset(&mut self, variant: $enum_name) {
                self.0 &= !(1 << (variant as $storage));
            }

            #[inline(always)]
            pub fn union(&mut self, other: Self) {
                self.0 |= other.0;
            }

            #[inline(always)]
            pub fn intersect(&mut self, other: Self)  {
                self.0 &= other.0;
            }

            #[inline(always)]
            pub fn contains(&self, variant: $enum_name) -> bool {
                // Becomes a single 'bt' or 'test' instruction
                (self.0 & (1 << (variant as $storage))) != 0
            }

            #[inline(always)]
            pub fn contains_all(&self, other: &Self) -> bool {
                (self.0 & other.0) == other.0
            }

            #[inline(always)]
            pub fn contains_any(&self, other: &Self) -> bool {
                (self.0 & other.0) != 0
            }

            #[inline(always)]
            pub fn is_empty(&self) -> bool {
                self.0 == 0
            }

            /// Returns true if every bit set in `self` is also set in `other`.
            #[inline(always)]
            pub fn is_subset_of(&self, other: &Self) -> bool {
                (self.0 & other.0) == self.0
            }

            #[inline]
            pub fn clear(&mut self) {
                self.0 = 0;
            }
        }

        impl std::fmt::Display for [<$enum_name Set>] {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                if self.0 == 0 { return Ok(()); }
                let mut first = true;
                $(
                    if self.contains($enum_name::$variant) {
                        if !first { write!(f, ",")?; }
                        write!(f, "{}", stringify!($variant).to_lowercase())?;
                        first = false;
                    }
                )*
                Ok(())
            }
        }

        // Implementation of BitOrAssign for better performance in loops
        impl std::ops::BitOrAssign for [<$enum_name Set>] {
            #[inline(always)]
            fn bitor_assign(&mut self, rhs: Self) {
                self.0 |= rhs.0;
            }
        }

        impl std::ops::BitOr for [<$enum_name Set>] {
            type Output = Self;
            #[inline(always)]
            fn bitor(mut self, rhs: Self) -> Self {
                self.0 |= rhs.0;
                self
            }
        }

        impl std::ops::BitAnd for [<$enum_name Set>] {
            type Output = Self;
            fn bitand(self, rhs: Self) -> Self {
                let mut new = self.clone();
                new.intersect(rhs);
                new
            }
        }

        // Permission::A | Permission::B → PermissionSet
        impl std::ops::BitOr for $enum_name {
            type Output = [<$enum_name Set>];
            #[inline(always)]
            fn bitor(self, rhs: Self) -> [<$enum_name Set>] {
                [<$enum_name Set>]((1 << (self as $storage)) | (1 << (rhs as $storage)))
            }
        }

        // PermissionSet | Permission → PermissionSet
        impl std::ops::BitOr<$enum_name> for [<$enum_name Set>] {
            type Output = Self;
            #[inline(always)]
            fn bitor(self, rhs: $enum_name) -> Self {
                Self(self.0 | (1 << (rhs as $storage)))
            }
        }

        // Permission → PermissionSet (single variant)
        impl From<$enum_name> for [<$enum_name Set>] {
            #[inline(always)]
            fn from(variant: $enum_name) -> Self {
                Self(1 << (variant as $storage))
            }
        }

    }
    };
}

#[macro_export]
macro_rules! apply_flags {
    ($dto:expr, $set:expr, $enum_type:path; $(($field:ident, $variant:ident)),* $(,)?) => {
        $(
            if $dto.$field {
                $set.set(<$enum_type>::$variant);
            }
        )*
    };
}
