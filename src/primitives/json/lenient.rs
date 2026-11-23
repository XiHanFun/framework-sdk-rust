//! 配置节的类型化读取（`plugin/lifecycle.md` §7.2）：宿主把 `IConfiguration` 的叶子一律写成 JSON 字符串（参考实现
//! `PluginJson.SerializeSection`），成员名与 `IConfiguration` 一样不区分大小写，所以在 [`Node`] 的读法之外放宽三处：
//!
//! - 结构体的成员名按 ASCII 不区分大小写匹配字段（完全相同的优先）；
//! - 字符串叶子读成数字时按 JSON 数字的文法（`-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`，不带空白），数按值模型；
//! - 字符串叶子读成布尔时只认 `true`、`false`（ASCII 不区分大小写）。
//!
//! 其余照目标类型的常规读法，与 [`Node`] 的反序列化相同。
//!
//! 数组与对象的访问（`visit_array`、`visit_object`）与 [`Node`] 的读法共用 `de.rs` 里的一份，以 `const LENIENT: bool` 区分各项、成员
//! 怎么读；JSON 数字的文法用读取器的那一份（`reader::parse_number`）。这里只有放宽的读法自己的部分。共用的访问要叫得出
//! [`Lenient`]，所以本模块总是编译；用它的入口（`json::decode_lenient`）只在 feature `plugin` 下。

use serde::de::{self, Visitor};

use super::de::{visit_array, visit_object};
use super::reader::{Node, Number, parse_number};

/// 反序列化的错误，同 [`Node`] 的反序列化。
type Error = serde_json::Error;

/// 放宽读法的值。
#[derive(Clone, Copy)]
pub(crate) struct Lenient<'de>(pub(crate) &'de Node);

impl<'de> Lenient<'de> {
    /// 字符串叶子按 JSON 数字的文法读成数（整串匹配、不带空白，按值模型）；不是这样的字符串为 `None`。
    fn number(&self) -> Option<Number> {
        match self.0 {
            Node::String(text) => parse_number(text),
            _ => None,
        }
    }

    /// 字符串叶子读成布尔：只认 `true`、`false`，ASCII 不区分大小写。
    fn boolean(&self) -> Option<bool> {
        match self.0 {
            Node::String(text) if text.eq_ignore_ascii_case("true") => Some(true),
            Node::String(text) if text.eq_ignore_ascii_case("false") => Some(false),
            _ => None,
        }
    }
}

/// 数按值模型交给访问者：非负整数按 `u64`、负整数按 `i64`，其余按 `f64`（同 [`Node`] 的 `deserialize_any`）。
fn visit_number<'de, V: Visitor<'de>>(number: Number, visitor: V) -> Result<V::Value, Error> {
    match number {
        Number::Integer(value) => match u64::try_from(value) {
            Ok(unsigned) => visitor.visit_u64(unsigned),
            Err(_) => visitor.visit_i64(value),
        },
        Number::Unsigned(value) => visitor.visit_u64(value),
        Number::Float(value) => visitor.visit_f64(value),
    }
}

/// 读成数的方法：字符串叶子先按 JSON 数字读，其余交给常规读法。
macro_rules! numeric {
    ($($method:ident),* $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
                match self.number() {
                    Some(number) => visit_number(number, visitor),
                    None => de::Deserializer::$method(self.0, visitor),
                }
            }
        )*
    };
}

/// 直接交给常规读法的方法。
macro_rules! strict {
    ($($method:ident),* $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
                de::Deserializer::$method(self.0, visitor)
            }
        )*
    };
}

impl<'de> de::Deserializer<'de> for Lenient<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Node::Array(items) => visit_array::<_, true>(items, visitor),
            Node::Object(members) => visit_object::<_, true>(members, &[], visitor),
            other => de::Deserializer::deserialize_any(other, visitor),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.boolean() {
            Some(flag) => visitor.visit_bool(flag),
            None => de::Deserializer::deserialize_bool(self.0, visitor),
        }
    }

    numeric! {
        deserialize_i8, deserialize_i16, deserialize_i32, deserialize_i64, deserialize_i128,
        deserialize_u8, deserialize_u16, deserialize_u32, deserialize_u64, deserialize_u128,
        deserialize_f32, deserialize_f64,
    }

    strict! {
        deserialize_char, deserialize_str, deserialize_string, deserialize_bytes, deserialize_byte_buf, deserialize_unit, deserialize_identifier,
        deserialize_ignored_any,
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Node::Null => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(self, name: &'static str, visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_unit_struct(self.0, name, visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, name: &'static str, visitor: V) -> Result<V::Value, Error> {
        // 任意 JSON（json::Json）要原文，交给常规读法；其余的新类型照放宽的读法读里面的值
        if name == super::value::JSON_TOKEN {
            return de::Deserializer::deserialize_newtype_struct(self.0, name, visitor);
        }
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Node::Array(items) => visit_array::<_, true>(items, visitor),
            other => de::Deserializer::deserialize_seq(other, visitor),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(self, _name: &'static str, _len: usize, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Node::Object(members) => visit_object::<_, true>(members, &[], visitor),
            other => de::Deserializer::deserialize_map(other, visitor),
        }
    }

    /// 结构体的成员名按 ASCII 不区分大小写对上字段名。
    fn deserialize_struct<V: Visitor<'de>>(self, _name: &'static str, fields: &'static [&'static str], visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Node::Object(members) => visit_object::<_, true>(members, fields, visitor),
            other => de::Deserializer::deserialize_map(other, visitor),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(self, name: &'static str, variants: &'static [&'static str], visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_enum(self.0, name, variants, visitor)
    }
}
