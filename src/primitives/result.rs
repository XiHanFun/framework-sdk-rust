use std::any::TypeId;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, Visitor};
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
/// 读写规则见 RFC-0001「Result」与向量 `wire-encoding.json`：
///
/// - 不带值的成功（`Result<()>`）只写 `{"ok":true}`；读入时带了 `value` 即不合法，`{}` 与 `null` 也一样。
/// - 有值成功的 `value` 必填；值类型是 `Maybe`（`Option`）时无值写 `null`，其他类型的 `value` 不能是 `null`。
/// - 失败的 `error` 必填且不能是 `null`；失败不能带 `value`，成功不能带 `error`。
/// - 只收 JSON 对象；未知成员忽略。
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
        let (mut ok, mut value, mut error) = (None, None, None::<Error>);
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "ok" if ok.is_some() => return Err(de::Error::duplicate_field("ok")),
                "ok" => ok = Some(map.next_value::<bool>()?),
                // 无值成功只写 {"ok":true}：带了 value 即不合法，{} 与 null 也一样
                "value" if is_unit::<T>() => return Err(invalid("A result without a value must not carry 'value'.")),
                "value" if value.is_some() => return Err(de::Error::duplicate_field("value")),
                "value" => value = Some(map.next_value_seed(PresentValue::<T>(PhantomData))?),
                "error" if error.is_some() => return Err(de::Error::duplicate_field("error")),
                // null 读不成 Error：失败的 error 写成 null 不合法
                "error" => error = Some(map.next_value::<Error>()?),
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        match (ok, value, error) {
            (None, ..) => Err(de::Error::missing_field("ok")),
            (Some(false), Some(_), _) => Err(invalid("A failed result must not carry 'value'.")),
            (Some(false), None, Some(error)) => Ok(Err(error)),
            (Some(false), None, None) => Err(invalid("A failed result on the wire needs an error.")),
            (Some(true), _, Some(_)) => Err(invalid("A successful result must not carry 'error'.")),
            (Some(true), Some(value), None) => Ok(Ok(value)),
            // 无值成功的值从「单元」得到
            (Some(true), None, None) if is_unit::<T>() => T::deserialize(de::value::UnitDeserializer::<A::Error>::new()).map(Ok),
            (Some(true), None, None) => Err(de::Error::missing_field("value")),
        }
    }
}

/// 线上编码的读入错误：`PRIMITIVES_VALIDATION_001` 加英文说明。
fn invalid<E: de::Error>(message: &str) -> E {
    E::custom(format!("{ERROR_CODE_INVALID_ARGUMENT}: {message}"))
}

/// 读入 `value`：`null` 只能是 `Maybe`（`Option`）的无值，其他类型的 `value` 不能是 `null`。
struct PresentValue<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> DeserializeSeed<'de> for PresentValue<T> {
    type Value = T;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> std::result::Result<T, D::Error> {
        deserializer.deserialize_option(self)
    }
}

impl<'de, T: Deserialize<'de>> Visitor<'de> for PresentValue<T> {
    type Value = T;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the value of a successful result")
    }

    fn visit_none<E: de::Error>(self) -> std::result::Result<T, E> {
        // 只有 Option 会以 deserialize_option 接住「无」，其他类型（含 serde_json::Value）在这里失败
        T::deserialize(OnlyNone(PhantomData))
    }

    fn visit_unit<E: de::Error>(self) -> std::result::Result<T, E> {
        self.visit_none()
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> std::result::Result<T, D::Error> {
        T::deserialize(deserializer)
    }
}

/// 只表示「无」的反序列化器：`deserialize_option` 给出 `None`，其余一律失败。
struct OnlyNone<E>(PhantomData<E>);

impl<'de, E: de::Error> Deserializer<'de> for OnlyNone<E> {
    type Error = E;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> std::result::Result<V::Value, E> {
        Err(invalid("The value of a successful result must not be null unless the value type is a Maybe."))
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> std::result::Result<V::Value, E> {
        visitor.visit_none()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
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
