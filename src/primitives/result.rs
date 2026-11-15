use std::any::TypeId;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::error::Error;
use super::error_code::ERROR_CODE_INVALID_ARGUMENT;

/// 成功或失败（RFC-0001 Result / Result&lt;T&gt;）：成功带值，失败带 [`Error`]。
///
/// 就是标准库的 `Result`，`?`、`map`、`and_then`、`unwrap_or` 照常使用；不带值的成功是 `Result<()>`。
/// 线上编码用 [`WireResult`] 或 [`wire_result`]。
pub type Result<T> = std::result::Result<T, Error>;

/// 可能没有的值（RFC-0001 Maybe&lt;T&gt;）：就是标准库的 `Option`。
///
/// 线上编码与 serde 对 `Option` 的处理相同：有值为值本身，无值为 `null`；`ok_or(error)` 即 Go 的 `ToResult`。
pub type Maybe<T> = Option<T>;

/// [`Result`] 的线上编码包装：`{"ok":true,"value":…}` 或 `{"ok":false,"error":{…}}`。
///
/// 不带值的成功（`Result<()>`）编码为 `{"ok":true}`，与 .NET 非泛型 `Result` 一致；读入时忽略它的 `value`。
/// 读入时未知字段忽略；`ok` 为假而没有 `error`、或 `ok` 为真而缺少 `value`（`Option` 值除外）都是错误。
#[derive(Clone, Debug, PartialEq)]
pub struct WireResult<T>(pub Result<T>);

impl<T> WireResult<T> {
    /// 取出里面的结果。
    pub fn into_inner(self) -> Result<T> {
        self.0
    }
}

impl<T> From<Result<T>> for WireResult<T> {
    fn from(result: Result<T>) -> Self {
        Self(result)
    }
}

impl<T> From<WireResult<T>> for Result<T> {
    fn from(wire: WireResult<T>) -> Self {
        wire.0
    }
}

/// 判断类型是不是 `()`：不带值的成功省略 `value`。
fn is_unit<T: 'static>() -> bool {
    TypeId::of::<T>() == TypeId::of::<()>()
}

/// 按线上编码写出一个结果。
fn serialize_result<T, S>(result: &Result<T>, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    T: Serialize + 'static,
    S: Serializer,
{
    let mut map = serializer.serialize_map(None)?;
    match result {
        Ok(value) => {
            map.serialize_entry("ok", &true)?;
            if !is_unit::<T>() {
                map.serialize_entry("value", value)?;
            }
        }
        Err(error) => {
            map.serialize_entry("ok", &false)?;
            map.serialize_entry("error", error)?;
        }
    }
    map.end()
}

impl<T: Serialize + 'static> Serialize for WireResult<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serialize_result(&self.0, serializer)
    }
}

/// 逐字段读入线上编码。
struct ResultVisitor<T>(PhantomData<T>);

impl<'de, T> Visitor<'de> for ResultVisitor<T>
where
    T: Deserialize<'de> + 'static,
{
    type Value = Result<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(r#"a result object {"ok":…}"#)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Result<T>, A::Error> {
        let (mut ok, mut value, mut error) = (None, None, None::<Option<Error>>);
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "ok" if ok.is_some() => return Err(de::Error::duplicate_field("ok")),
                "ok" => ok = Some(map.next_value::<bool>()?),
                "value" if value.is_some() => return Err(de::Error::duplicate_field("value")),
                // 不带值的成功不看 value：Go 写成 {}，.NET 省略，都接受
                "value" if is_unit::<T>() => {
                    map.next_value::<IgnoredAny>()?;
                    value = Some(None);
                }
                "value" => value = Some(Some(map.next_value::<T>()?)),
                "error" if error.is_some() => return Err(de::Error::duplicate_field("error")),
                "error" => error = Some(map.next_value::<Option<Error>>()?),
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        match ok {
            None => Err(de::Error::missing_field("ok")),
            Some(false) => match error.flatten() {
                Some(error) => Ok(Err(error)),
                None => Err(de::Error::custom(format!("{ERROR_CODE_INVALID_ARGUMENT}: A failed result on the wire needs an error."))),
            },
            Some(true) => match value.flatten() {
                Some(value) => Ok(Ok(value)),
                // 缺少 value：`()` 与 `Option` 可以从「无」得到值，其他类型即缺字段
                None => T::deserialize(de::value::UnitDeserializer::<A::Error>::new()).map(Ok).map_err(|_: A::Error| de::Error::missing_field("value")),
            },
        }
    }
}

impl<'de, T: Deserialize<'de> + 'static> Deserialize<'de> for WireResult<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        deserializer.deserialize_map(ResultVisitor(PhantomData)).map(WireResult)
    }
}

/// 供 `#[serde(with = "…")]` 使用的线上编码：字段类型是 [`Result`] 时按 [`WireResult`] 的形态读写。
///
/// ```
/// use serde::{Deserialize, Serialize};
/// use xihan_framework::primitives::{self, Result};
///
/// #[derive(Serialize, Deserialize)]
/// struct Envelope {
///     #[serde(with = "primitives::wire_result")]
///     outcome: Result<u32>,
/// }
///
/// let text = serde_json::to_string(&Envelope { outcome: Ok(7) }).unwrap();
/// assert_eq!(text, r#"{"outcome":{"ok":true,"value":7}}"#);
/// ```
pub mod wire_result {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{Result, WireResult};

    /// 按线上编码写出。
    pub fn serialize<T, S>(result: &Result<T>, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        T: Serialize + 'static,
        S: Serializer,
    {
        super::serialize_result(result, serializer)
    }

    /// 按线上编码读入。
    pub fn deserialize<'de, T, D>(deserializer: D) -> std::result::Result<Result<T>, D::Error>
    where
        T: Deserialize<'de> + 'static,
        D: Deserializer<'de>,
    {
        WireResult::deserialize(deserializer).map(WireResult::into_inner)
    }
}
