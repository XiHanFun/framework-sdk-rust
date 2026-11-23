/// 生成按规范名字编码的枚举：`ALL`、`name`、`as_str`、`parse`、`Display`、`FromStr` 与按名字的 serde 实现。
///
/// 名字区分大小写；`FromStr` 与读入失败的消息是 `'<名字>' is not a <what>.`，错误码 `PRIMITIVES_VALIDATION_001`。名字后面可以用 `|`
/// 再列读入时同样接受的写法（如协议里表示缺省的空串），写出的总是名字本身。枚举前的属性原样带上：会随协议增长的枚举在调用处标
/// `#[non_exhaustive]`。标准库的类型都写全路径，调用处可能引入了同名的别名（如 `primitives::Result`）。
macro_rules! named_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident ($what:literal) {
            $( $(#[$variant_meta:meta])* $variant:ident = $value:literal => $text:literal $(| $alias:literal)* ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(u8)]
        $vis enum $name {
            $( $(#[$variant_meta])* $variant = $value ),+
        }

        impl $name {
            /// 按数值顺序列出全部取值。
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// 规范里的名字。
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// 规范里的名字，同 [`Self::name`]。
            pub const fn as_str(self) -> &'static str {
                self.name()
            }

            /// 按名字解析，区分大小写；未知名字返回 `None`。
            pub fn parse(name: &str) -> ::std::option::Option<Self> {
                match name {
                    $($text $(| $alias)* => ::std::option::Option::Some(Self::$variant),)+
                    _ => ::std::option::Option::None,
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                formatter.write_str(self.name())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::primitives::FrameworkError;

            fn from_str(name: &str) -> ::std::result::Result<Self, Self::Err> {
                Self::parse(name).ok_or_else(|| {
                    $crate::primitives::FrameworkError::new($crate::primitives::ERROR_CODE_INVALID_ARGUMENT, format!(concat!("'{}' is not a ", $what, "."), name))
                })
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> ::std::result::Result<S::Ok, S::Error> {
                serializer.serialize_str(self.name())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> ::std::result::Result<Self, D::Error> {
                let name = <::std::string::String as ::serde::Deserialize>::deserialize(deserializer)?;
                Self::parse(&name).ok_or_else(|| <D::Error as ::serde::de::Error>::custom(format!(concat!("'{}' is not a ", $what, "."), name)))
            }
        }
    };
}

pub(crate) use named_enum;
