#[macro_export]
macro_rules! create_bit_set {
    ($storage:ty, $enum_name:ident, $($variant:ident),*) => {
        paste::paste! {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr($storage)]
        pub enum $enum_name {
            $($variant),*
        }
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
        pub struct [<$enum_name Set>](pub $storage);

        impl [<$enum_name Set>] {

            #[inline]
            pub fn new() -> Self {
                Self(0)
            }

            // New: Constructor for multiple variants
            #[inline]
            pub fn from_variants(variants: &[$enum_name]) -> Self {
                let mut set = Self::new();
                for &v in variants {
                    set.add(v);
                }
                set
            }

            #[inline]
            pub fn add(&mut self, variant: $enum_name) {
                self.0 |= 1 << (variant as $storage);
            }

            #[inline]
            pub fn remove(&mut self, variant: $enum_name) {
                self.0 &= !(1 << (variant as $storage));
            }

            #[inline]
            pub fn union(&mut self, other: Self) {
                self.0 |= other.0;
            }

            #[inline]
            pub fn intersect(&mut self, other: Self)  {
                self.0 &= other.0;
            }

            #[inline]
            pub fn contains(&self, variant: $enum_name) -> bool {
                (self.0 & (1 << (variant as $storage))) != 0
            }

            #[inline]
            pub fn is_empty(&self) -> bool {
                self.0 == 0
            }

            #[inline]
            pub fn clear(&mut self) {
                self.0 = 0;
            }
        }

        impl std::fmt::Display for [<$enum_name Set>] {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

        // set1 | set2
        impl std::ops::BitOr for [<$enum_name Set>] {
            type Output = Self;
            fn bitor(self, rhs: Self) -> Self {
                let mut new = self.clone();
                new.union(rhs);
                new

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

    }
    };
}