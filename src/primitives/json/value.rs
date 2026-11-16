//! 任意 JSON 值，原样保留读到的值（RFC-0011 规则 2）。

use std::fmt;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::value::RawValue;

use super::reader::write_node;
use super::{ReadError, parse};

/// [`Json`] 向本模块的反序列化器要原文时用的名字；别的反序列化器不认它，照常交给 `visit_newtype_struct`。
pub(crate) const JSON_TOKEN: &str = "$xihan_framework::private::Json";

/// 任意 JSON 值，原样保留读到的值：请求与响应里「任意 JSON」类型的成员用它（RFC-0011 规则 2，ADR-0236）。
///
/// 数字照原文保留，`1e400`、超出双精度的整数（`123456789012345678901234567890`）、`2.0` 与 `2` 的区别都不变，写回时与读到的
/// JSON 值相等；`serde_json::Value` 做不到：它把 `1e400` 读成 `null`，把超出 64 位的整数读成近似的浮点。保存的是紧凑文本，
/// 成员按文档顺序，字符串按 JSON 字符串写法重新转义（转义的写法不是契约）。`null` 也是一个值；接受 `null` 而表示「没有」时用
/// `Option<Json>`。
///
/// ```
/// use serde::Deserialize;
/// use xihan_framework::primitives::json::{self, Json};
///
/// #[derive(Deserialize)]
/// struct Event {
///     data: Json,
/// }
///
/// let event: Event = json::from_str(r#"{"data": {"big": 1e400, "exact": 9007199254740993, "float": 2.0}}"#).unwrap();
/// assert_eq!(event.data.as_str(), r#"{"big":1e400,"exact":9007199254740993,"float":2.0}"#);
/// assert_eq!(serde_json::to_string(&event.data).unwrap(), event.data.as_str());
/// ```
#[derive(Clone)]
pub struct Json(Box<RawValue>);

impl Json {
    /// 从 JSON 文本创建：先过共同的读取检查（[`check`](super::check)），保存它的紧凑写法。
    pub fn from_text(text: &str) -> Result<Self, ReadError> {
        let node = parse(text.as_bytes())?;
        let mut compact = String::with_capacity(text.len());
        write_node(&mut compact, &node);
        Ok(Self::from_compact(compact))
    }

    /// 紧凑文本，数字照原文。
    pub fn as_str(&self) -> &str {
        self.0.get()
    }

    /// 已核对过的紧凑文本。
    fn from_compact(text: String) -> Self {
        // 文本来自本模块的读取器，总是合法的 JSON；serde_json 再核对一遍不会失败
        Self(RawValue::from_string(text).unwrap_or_default())
    }
}

impl PartialEq for Json {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Json {}

impl fmt::Debug for Json {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("Json").field(&self.as_str()).finish()
    }
}

impl fmt::Display for Json {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for Json {
    /// 原文写出：`serde_json` 的序列化器照文本写，数字一字不改。
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Json {
    /// 从本模块的反序列化器（[`from_slice`](super::from_slice)、载荷解码）读时得到原文；从 `serde_json` 的反序列化器读时借它的
    /// `RawValue` 取原文，再过共同的读取检查。
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_newtype_struct(JSON_TOKEN, JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_string<E: de::Error>(self, text: String) -> Result<Json, E> {
        Ok(Json::from_compact(text))
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Json, E> {
        Ok(Json::from_compact(text.to_owned()))
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(self, deserializer: D) -> Result<Json, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        Json::from_text(raw.get()).map_err(de::Error::custom)
    }
}
