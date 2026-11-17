//! 配置节的类型化读取（`plugin/lifecycle.md` §7.2）：宿主把 `IConfiguration` 的叶子一律写成 JSON 字符串（参考实现
//! `PluginJson.SerializeSection`），成员名与 `IConfiguration` 一样不区分大小写，所以在 [`Node`] 的读法之外放宽三处：
//!
//! - 结构体的成员名按 ASCII 不区分大小写匹配字段（完全相同的优先）；
//! - 字符串叶子读成数字时按 JSON 数字的文法（`-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`，不带空白），数按值模型；
//! - 字符串叶子读成布尔时只认 `true`、`false`（ASCII 不区分大小写）。
//!
//! 其余照目标类型的常规读法，与 [`Node`] 的反序列化相同。

use std::slice;

use serde::de::value::BorrowedStrDeserializer;
use serde::de::{self, DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};

use super::de::Key;
use super::reader::{Node, Number, parse};

/// 反序列化的错误，同 [`Node`] 的反序列化。
type Error = serde_json::Error;

/// 放宽读法的值。
#[derive(Clone, Copy)]
pub(crate) struct Lenient<'de>(pub(crate) &'de Node);

impl<'de> Lenient<'de> {
    /// 字符串叶子按 JSON 数字的文法读成数；不是这样的字符串为 `None`。
    fn number(&self) -> Option<Number> {
        let Node::String(text) = self.0 else {
            return None;
        };
        if !is_json_number(text) {
            return None;
        }
        // 文法已核对过，按值模型读出来的一定是数
        match parse(text.as_bytes()) {
            Ok(Node::Number(number, _)) => Some(number),
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
            Node::Array(items) => visit_array(items, visitor),
            Node::Object(members) => visit_object(members, &[], visitor),
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
            Node::Array(items) => visit_array(items, visitor),
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
            Node::Object(members) => visit_object(members, &[], visitor),
            other => de::Deserializer::deserialize_map(other, visitor),
        }
    }

    /// 结构体的成员名按 ASCII 不区分大小写对上字段名。
    fn deserialize_struct<V: Visitor<'de>>(self, _name: &'static str, fields: &'static [&'static str], visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Node::Object(members) => visit_object(members, fields, visitor),
            other => de::Deserializer::deserialize_map(other, visitor),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(self, name: &'static str, variants: &'static [&'static str], visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_enum(self.0, name, variants, visitor)
    }
}

fn visit_array<'de, V: Visitor<'de>>(items: &'de [Node], visitor: V) -> Result<V::Value, Error> {
    let mut access = Items(items.iter());
    let value = visitor.visit_seq(&mut access)?;
    match access.0.len() {
        0 => Ok(value),
        _ => Err(Error::invalid_length(items.len(), &"fewer elements in the array")),
    }
}

fn visit_object<'de, V: Visitor<'de>>(members: &'de [(String, Node)], fields: &'static [&'static str], visitor: V) -> Result<V::Value, Error> {
    let mut access = Members { members: members.iter(), fields, value: None };
    let value = visitor.visit_map(&mut access)?;
    match access.members.len() {
        0 => Ok(value),
        _ => Err(Error::invalid_length(members.len(), &"fewer members in the object")),
    }
}

struct Items<'de>(slice::Iter<'de, Node>);

impl<'de> SeqAccess<'de> for Items<'de> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, Error> {
        self.0.next().map(|item| seed.deserialize(Lenient(item))).transpose()
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
    }
}

/// 按文本顺序给出成员；`fields` 是结构体的字段名，映射与集合为空。
struct Members<'de> {
    members: slice::Iter<'de, (String, Node)>,
    fields: &'static [&'static str],
    value: Option<&'de Node>,
}

impl<'de> MapAccess<'de> for Members<'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, Error> {
        let Some((name, value)) = self.members.next() else {
            return Ok(None);
        };
        self.value = Some(value);
        // 完全相同的字段优先，否则取第一个只差 ASCII 大小写的字段；都对不上的照原名交给访问者（未知成员照常忽略）
        let field = self.fields.iter().find(|field| **field == name).or_else(|| self.fields.iter().find(|field| field.eq_ignore_ascii_case(name)));
        match field {
            Some(field) => seed.deserialize(BorrowedStrDeserializer::new(field)).map(Some),
            None => seed.deserialize(Key(name)).map(Some),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        let value = self.value.take().ok_or_else(|| Error::custom("a member value was requested before its name"))?;
        seed.deserialize(Lenient(value))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.members.len())
    }
}

/// JSON 数字的文法 `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`，整串匹配、不带空白。
fn is_json_number(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut at = usize::from(bytes.first() == Some(&b'-'));
    let digits = |at: usize| bytes[at..].iter().take_while(|byte| byte.is_ascii_digit()).count();
    match bytes.get(at) {
        Some(b'0') => at += 1,
        Some(b'1'..=b'9') => at += digits(at),
        _ => return false,
    }
    if bytes.get(at) == Some(&b'.') {
        let count = digits(at + 1);
        if count == 0 {
            return false;
        }
        at += 1 + count;
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(bytes.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let count = digits(at);
        if count == 0 {
            return false;
        }
        at += count;
    }
    at == bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_numbers_follow_the_grammar() {
        for text in ["0", "-0", "12", "-1.5", "1e3", "2E-7", "0.25e+2"] {
            assert!(is_json_number(text), "{text}");
        }
        for text in ["", "-", "01", "+1", "1.", ".5", "1e", "1e+", " 1", "1 ", "0x10", "NaN", "Infinity", "１"] {
            assert!(!is_json_number(text), "{text:?}");
        }
    }
}
