#[macro_export]
macro_rules! scalar_newtype {
    ($(
        $(#[$meta:meta])*
        $vis:vis struct $name:ident($prim:ty) $(=> $mode:ident)?;
    )+) => {$(
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis struct $name($prim);

        impl $name {
            $crate::scalar_ctor!($($mode)? , $prim);

            pub const fn get(self) -> $prim {
                self.0
            }
        }

        $crate::scalar_order!($($mode)? , $name);
    )+};
}

#[macro_export]
macro_rules! scalar_ctor {
    (, $prim:ty) => {
        pub const fn new(value: $prim) -> Self {
            Self(value)
        }
    };
    (ordered, $prim:ty) => {
        $crate::scalar_ctor!(, $prim);
    };
    (sealed, $prim:ty) => {
        pub(crate) const fn new(value: $prim) -> Self {
            Self(value)
        }
    };
}

#[macro_export]
macro_rules! scalar_order {
    (, $name:ident) => {};
    (sealed, $name:ident) => {};
    (ordered, $name:ident) => {
        impl ::core::cmp::Ord for $name {
            fn cmp(&self, other: &Self) -> ::core::cmp::Ordering {
                self.0.cmp(&other.0)
            }
        }

        impl ::core::cmp::PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<::core::cmp::Ordering> {
                Some(::core::cmp::Ord::cmp(self, other))
            }
        }
    };
}

#[macro_export]
macro_rules! text_mode {
    (verbatim, $value:expr) => {
        $value
    };
    (strip_control, $value:expr) => {
        match $value {
            value if value.contains(['\0', '\n']) => value.replace(['\0', '\n'], ""),
            value => value,
        }
    };
}

#[macro_export]
macro_rules! text_newtype {
    ($(
        $(#[$meta:meta])*
        $vis:vis struct $name:ident(String) => $mode:ident;
    )+) => {
        $crate::text_newtype! {$(
            $(#[$meta])*
            $vis struct $name(String) => $mode as new;
        )+}
    };
    ($(
        $(#[$meta:meta])*
        $vis:vis struct $name:ident(String) => $mode:ident as $ctor:ident;
    )+) => {$(
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        $vis struct $name(String);

        impl $name {
            pub fn $ctor(value: impl Into<String>) -> Self {
                Self($crate::text_mode!($mode, value.into()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.pad(&self.0)
            }
        }

        impl ::std::convert::AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl ::std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> Result<Self, D::Error> {
                <String as ::serde::Deserialize>::deserialize(deserializer).map(Self::$ctor)
            }
        }
    )+};
}

#[macro_export]
macro_rules! read_counter {
    ($name:ident, $record:ident, $read:ident, $reset:ident, $measure:ident) => {
        $crate::scalar_newtype! {
            pub struct $name(u64) => sealed;
        }

        fn count() -> &'static ::std::thread::LocalKey<::std::cell::Cell<u64>> {
            ::std::thread_local! {
                static COUNT: ::std::cell::Cell<u64> = const { ::std::cell::Cell::new(0) };
            }
            &COUNT
        }

        pub(crate) fn $record() {
            count().with(|cell| cell.set(cell.get().wrapping_add(1)));
        }

        pub fn $read() -> $name {
            $name::new(count().with(::std::cell::Cell::get))
        }

        pub fn $reset() {
            count().with(|cell| cell.set(0));
        }

        pub fn $measure<T>(work: impl FnOnce() -> T) -> (T, $name) {
            $reset();
            let value = work();
            (value, $read())
        }
    };
}

#[cfg(test)]
mod tests {
    crate::scalar_newtype! {
        pub struct Bytes(u64) => ordered;
        pub struct Tag(u64);
    }

    crate::text_newtype! {
        pub struct Header(String) => strip_control;
        pub struct Body(String) => verbatim;
    }

    crate::text_newtype! {
        pub struct Column(String) => strip_control as from_column;
    }

    fn sorted<T: Ord>(mut values: Vec<T>) -> Vec<T> {
        values.sort();
        values
    }

    #[test]
    fn only_a_newtype_that_declares_an_order_gets_one() {
        assert_eq!(Bytes::new(7).get(), 7);
        assert!(Bytes::new(7) < Bytes::new(8));
        assert_eq!(
            sorted(vec![Bytes::new(8), Bytes::new(7)]),
            vec![Bytes::new(7), Bytes::new(8)]
        );
        assert_ne!(Tag::new(7), Tag::new(8));
        assert_eq!(
            Tag::new(7).get().cmp(&Tag::new(8).get()),
            std::cmp::Ordering::Less,
            "a plain newtype still compares through the value it wraps, \
             so a caller that wants an ordering declares one instead of inheriting it"
        );
    }

    #[test]
    fn a_text_newtype_applies_its_mode_through_every_constructor_it_has() {
        assert_eq!(Header::new("nel\nolaren\0").as_str(), "nelolaren");
        assert_eq!(Body::new("nel\nolaren\0").as_str(), "nel\nolaren\0");
        assert_eq!(
            Column::from_column("nel\nolaren").as_str(),
            "nelolaren",
            "a type that names its constructor for where the value comes from \
             must still apply its mode"
        );

        let header: Header = serde_json::from_str("\"nel\\nolaren\"").unwrap();
        let body: Body = serde_json::from_str("\"nel\\nolaren\"").unwrap();
        let column: Column = serde_json::from_str("\"nel\\nolaren\"").unwrap();
        assert_eq!(
            (header.as_str(), body.as_str(), column.as_str()),
            ("nelolaren", "nel\nolaren", "nelolaren"),
            "deserialization goes through the constructor, whatever it is named"
        );
        assert_eq!(serde_json::to_string(&body).unwrap(), "\"nel\\nolaren\"");

        let map: std::collections::HashMap<Body, u8> =
            [(Body::new("kelp"), 1)].into_iter().collect();
        assert_eq!(map.get("kelp"), Some(&1), "a text newtype borrows as str");
    }
}
