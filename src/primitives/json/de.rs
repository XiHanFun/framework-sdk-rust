//! 从读出的值树按 serde 反序列化（RFC-0011 规则 2 的解码语义）。
//!
//! 与 `serde_json` 的读取不同的地方都是规范要求的：数字按值模型（`-0` 是整数 0，`1e400` 是无穷而不报错），
//! 对象位置只收 JSON 对象（serde 派生的结构体不再接受字段同序的数组），成员按文本顺序交给访问者，报错因此按文本顺序。

use std::slice;

use serde::de::value::BorrowedStrDeserializer;
use serde::de::{self, DeserializeSeed, EnumAccess, Error as _, Expected, MapAccess, SeqAccess, Unexpected, VariantAccess, Visitor};
use serde::forward_to_deserialize_any;

use super::reader::{Node, Number};

/// 反序列化的错误：借用 `serde_json` 的错误类型，调用方只关心出错的路径。
type Error = serde_json::Error;

impl Node {
    fn unexpected(&self) -> Unexpected<'_> {
        match self {
            Node::Null => Unexpected::Unit,
            Node::Bool(flag) => Unexpected::Bool(*flag),
            Node::Number(Number::Integer(number)) => Unexpected::Signed(*number),
            Node::Number(Number::Unsigned(number)) => Unexpected::Unsigned(*number),
            Node::Number(Number::Float(number)) => Unexpected::Float(*number),
            Node::String(text) => Unexpected::Str(text),
            Node::Array(_) => Unexpected::Seq,
            Node::Object(_) => Unexpected::Map,
        }
    }

    fn invalid_type(&self, expected: &dyn Expected) -> Error {
        Error::invalid_type(self.unexpected(), expected)
    }
}

impl<'de> de::Deserializer<'de> for &'de Node {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Node::Null => visitor.visit_unit(),
            Node::Bool(flag) => visitor.visit_bool(*flag),
            // 与 serde_json 一样：非负整数按 u64、负整数按 i64 交给访问者
            Node::Number(Number::Integer(number)) => match u64::try_from(*number) {
                Ok(unsigned) => visitor.visit_u64(unsigned),
                Err(_) => visitor.visit_i64(*number),
            },
            Node::Number(Number::Unsigned(number)) => visitor.visit_u64(*number),
            Node::Number(Number::Float(number)) => visitor.visit_f64(*number),
            Node::String(text) => visitor.visit_borrowed_str(text),
            Node::Array(items) => visit_array(items, visitor),
            Node::Object(members) => visit_object(members, visitor),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Node::Null => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Node::Null => visitor.visit_unit(),
            _ => Err(self.invalid_type(&visitor)),
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Node::Array(items) => visit_array(items, visitor),
            _ => Err(self.invalid_type(&visitor)),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(self, _name: &'static str, _len: usize, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Node::Object(members) => visit_object(members, visitor),
            _ => Err(self.invalid_type(&visitor)),
        }
    }

    /// 结构体只收 JSON 对象：数组形式（serde 派生的结构体本来也接受）是类型不符。
    fn deserialize_struct<V: Visitor<'de>>(self, _name: &'static str, _fields: &'static [&'static str], visitor: V) -> Result<V::Value, Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(self, _name: &'static str, _variants: &'static [&'static str], visitor: V) -> Result<V::Value, Error> {
        match self {
            Node::String(text) => visitor.visit_enum(BorrowedStrDeserializer::new(text)),
            Node::Object(members) if members.len() == 1 => {
                let (name, value) = &members[0];
                visitor.visit_enum(Variant { name, value })
            }
            _ => Err(self.invalid_type(&visitor)),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf identifier
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

fn visit_object<'de, V: Visitor<'de>>(members: &'de [(String, Node)], visitor: V) -> Result<V::Value, Error> {
    let mut access = Members { members: members.iter(), value: None };
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
        self.0.next().map(|item| seed.deserialize(item)).transpose()
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
    }
}

/// 按文本顺序给出成员。
struct Members<'de> {
    members: slice::Iter<'de, (String, Node)>,
    value: Option<&'de Node>,
}

impl<'de> MapAccess<'de> for Members<'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, Error> {
        let Some((name, value)) = self.members.next() else {
            return Ok(None);
        };
        self.value = Some(value);
        seed.deserialize(Key(name)).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        let value = self.value.take().ok_or_else(|| Error::custom("a member value was requested before its name"))?;
        seed.deserialize(value)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.members.len())
    }
}

/// 成员名：交给访问者的是字符串；整数与布尔类型的键（如 `BTreeMap<i32, _>`）从名字的文本解析。
struct Key<'de>(&'de str);

macro_rules! parsed_key {
    ($($method:ident => $visit:ident: $type:ty),* $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
                match self.0.parse::<$type>() {
                    Ok(value) => visitor.$visit(value),
                    Err(_) => visitor.visit_borrowed_str(self.0),
                }
            }
        )*
    };
}

impl<'de> de::Deserializer<'de> for Key<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_borrowed_str(self.0)
    }

    parsed_key! {
        deserialize_bool => visit_bool: bool,
        deserialize_i8 => visit_i8: i8,
        deserialize_i16 => visit_i16: i16,
        deserialize_i32 => visit_i32: i32,
        deserialize_i64 => visit_i64: i64,
        deserialize_i128 => visit_i128: i128,
        deserialize_u8 => visit_u8: u8,
        deserialize_u16 => visit_u16: u16,
        deserialize_u32 => visit_u32: u32,
        deserialize_u64 => visit_u64: u64,
        deserialize_u128 => visit_u128: u128,
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(self, _name: &'static str, _variants: &'static [&'static str], visitor: V) -> Result<V::Value, Error> {
        visitor.visit_enum(BorrowedStrDeserializer::new(self.0))
    }

    forward_to_deserialize_any! {
        f32 f64 char str string bytes byte_buf unit unit_struct seq tuple tuple_struct map struct identifier ignored_any
    }
}

/// 写成单成员对象的枚举变体 `{"变体": 值}`。
struct Variant<'de> {
    name: &'de str,
    value: &'de Node,
}

impl<'de> EnumAccess<'de> for Variant<'de> {
    type Error = Error;
    type Variant = &'de Node;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, &'de Node), Error> {
        Ok((seed.deserialize(Key(self.name))?, self.value))
    }
}

impl<'de> VariantAccess<'de> for &'de Node {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        match self {
            Node::Null => Ok(()),
            _ => Err(self.invalid_type(&"a unit variant")),
        }
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        seed.deserialize(self)
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_seq(self, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(self, _fields: &'static [&'static str], visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_map(self, visitor)
    }
}
