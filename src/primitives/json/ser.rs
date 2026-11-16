//! 按值模型写出（RFC-0001「Result」、RFC-0010「模板」的值模型）。

use serde::Serialize;
use serde::ser::{
    SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple, SerializeTupleStruct, SerializeTupleVariant, Serializer,
};
use serde_json::Value;

/// 按值模型写出任意可序列化的值：单精度浮点先换成最接近的双精度（0.1 写 `0.10000000149011612`），非有限的浮点写成字符串
/// `NaN`、`Infinity`、`-Infinity`，其余照原样交给内层的序列化器。
///
/// `serde_json` 自己把非有限的浮点写成 `null`，对方就读不回同一个值；整数值的浮点它写成 `2.0`、`-0.0`、`1e17`，
/// 对方按模型读回的仍是浮点，不用另做处理。[`WireResult`](crate::primitives::WireResult) 的值经它写出。
///
/// ```
/// use xihan_framework::primitives::json::Model;
///
/// assert_eq!(serde_json::to_string(&Model(&[f64::NAN, 2.0])).unwrap(), r#"["NaN",2.0]"#);
/// assert_eq!(serde_json::to_string(&Model(&0.1f32)).unwrap(), "0.10000000149011612");
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Model<'a, T: ?Sized>(pub &'a T);

impl<T: Serialize + ?Sized> Serialize for Model<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(ModelSerializer(serializer))
    }
}

/// 按值模型把值转成 `serde_json::Value`：给错误细节与模板参数用，非有限的浮点成为字符串，而不是 `Value::from` 给出的 `null`。
///
/// 序列化失败（如映射的键不是字符串）时返回 `serde_json` 的错误。
pub fn to_value<T: Serialize + ?Sized>(value: &T) -> Result<Value, serde_json::Error> {
    Model(value).serialize(serde_json::value::Serializer)
}

/// 非有限浮点的文本，与模板里的写法相同。
fn non_finite(number: f64) -> &'static str {
    if number.is_nan() {
        "NaN"
    } else if number > 0.0 {
        "Infinity"
    } else {
        "-Infinity"
    }
}

struct ModelSerializer<S>(S);

/// 容器的包装：每个元素与成员同样按值模型写出。
struct Compound<C>(C);

impl<S: Serializer> Serializer for ModelSerializer<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = Compound<S::SerializeSeq>;
    type SerializeTuple = Compound<S::SerializeTuple>;
    type SerializeTupleStruct = Compound<S::SerializeTupleStruct>;
    type SerializeTupleVariant = Compound<S::SerializeTupleVariant>;
    type SerializeMap = Compound<S::SerializeMap>;
    type SerializeStruct = Compound<S::SerializeStruct>;
    type SerializeStructVariant = Compound<S::SerializeStructVariant>;

    fn serialize_bool(self, value: bool) -> Result<S::Ok, S::Error> {
        self.0.serialize_bool(value)
    }

    fn serialize_i8(self, value: i8) -> Result<S::Ok, S::Error> {
        self.0.serialize_i8(value)
    }

    fn serialize_i16(self, value: i16) -> Result<S::Ok, S::Error> {
        self.0.serialize_i16(value)
    }

    fn serialize_i32(self, value: i32) -> Result<S::Ok, S::Error> {
        self.0.serialize_i32(value)
    }

    fn serialize_i64(self, value: i64) -> Result<S::Ok, S::Error> {
        self.0.serialize_i64(value)
    }

    fn serialize_i128(self, value: i128) -> Result<S::Ok, S::Error> {
        self.0.serialize_i128(value)
    }

    fn serialize_u8(self, value: u8) -> Result<S::Ok, S::Error> {
        self.0.serialize_u8(value)
    }

    fn serialize_u16(self, value: u16) -> Result<S::Ok, S::Error> {
        self.0.serialize_u16(value)
    }

    fn serialize_u32(self, value: u32) -> Result<S::Ok, S::Error> {
        self.0.serialize_u32(value)
    }

    fn serialize_u64(self, value: u64) -> Result<S::Ok, S::Error> {
        self.0.serialize_u64(value)
    }

    fn serialize_u128(self, value: u128) -> Result<S::Ok, S::Error> {
        self.0.serialize_u128(value)
    }

    fn serialize_f32(self, value: f32) -> Result<S::Ok, S::Error> {
        self.serialize_f64(f64::from(value))
    }

    fn serialize_f64(self, value: f64) -> Result<S::Ok, S::Error> {
        if value.is_finite() { self.0.serialize_f64(value) } else { self.0.serialize_str(non_finite(value)) }
    }

    fn serialize_char(self, value: char) -> Result<S::Ok, S::Error> {
        self.0.serialize_char(value)
    }

    fn serialize_str(self, value: &str) -> Result<S::Ok, S::Error> {
        self.0.serialize_str(value)
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<S::Ok, S::Error> {
        self.0.serialize_bytes(value)
    }

    fn serialize_none(self) -> Result<S::Ok, S::Error> {
        self.0.serialize_none()
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<S::Ok, S::Error> {
        self.0.serialize_some(&Model(value))
    }

    fn serialize_unit(self) -> Result<S::Ok, S::Error> {
        self.0.serialize_unit()
    }

    fn serialize_unit_struct(self, name: &'static str) -> Result<S::Ok, S::Error> {
        self.0.serialize_unit_struct(name)
    }

    fn serialize_unit_variant(self, name: &'static str, index: u32, variant: &'static str) -> Result<S::Ok, S::Error> {
        self.0.serialize_unit_variant(name, index, variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, name: &'static str, value: &T) -> Result<S::Ok, S::Error> {
        self.0.serialize_newtype_struct(name, &Model(value))
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(self, name: &'static str, index: u32, variant: &'static str, value: &T) -> Result<S::Ok, S::Error> {
        self.0.serialize_newtype_variant(name, index, variant, &Model(value))
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, S::Error> {
        self.0.serialize_seq(len).map(Compound)
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, S::Error> {
        self.0.serialize_tuple(len).map(Compound)
    }

    fn serialize_tuple_struct(self, name: &'static str, len: usize) -> Result<Self::SerializeTupleStruct, S::Error> {
        self.0.serialize_tuple_struct(name, len).map(Compound)
    }

    fn serialize_tuple_variant(self, name: &'static str, index: u32, variant: &'static str, len: usize) -> Result<Self::SerializeTupleVariant, S::Error> {
        self.0.serialize_tuple_variant(name, index, variant, len).map(Compound)
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, S::Error> {
        self.0.serialize_map(len).map(Compound)
    }

    fn serialize_struct(self, name: &'static str, len: usize) -> Result<Self::SerializeStruct, S::Error> {
        self.0.serialize_struct(name, len).map(Compound)
    }

    fn serialize_struct_variant(self, name: &'static str, index: u32, variant: &'static str, len: usize) -> Result<Self::SerializeStructVariant, S::Error> {
        self.0.serialize_struct_variant(name, index, variant, len).map(Compound)
    }

    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
}

impl<C: SerializeSeq> SerializeSeq for Compound<C> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
        self.0.serialize_element(&Model(value))
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeTuple> SerializeTuple for Compound<C> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
        self.0.serialize_element(&Model(value))
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeTupleStruct> SerializeTupleStruct for Compound<C> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
        self.0.serialize_field(&Model(value))
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeTupleVariant> SerializeTupleVariant for Compound<C> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
        self.0.serialize_field(&Model(value))
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeMap> SerializeMap for Compound<C> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), C::Error> {
        self.0.serialize_key(&Model(key))
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
        self.0.serialize_value(&Model(value))
    }

    fn serialize_entry<K: Serialize + ?Sized, V: Serialize + ?Sized>(&mut self, key: &K, value: &V) -> Result<(), C::Error> {
        self.0.serialize_entry(&Model(key), &Model(value))
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeStruct> SerializeStruct for Compound<C> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), C::Error> {
        self.0.serialize_field(key, &Model(value))
    }

    fn skip_field(&mut self, key: &'static str) -> Result<(), C::Error> {
        self.0.skip_field(key)
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeStructVariant> SerializeStructVariant for Compound<C> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), C::Error> {
        self.0.serialize_field(key, &Model(value))
    }

    fn skip_field(&mut self, key: &'static str) -> Result<(), C::Error> {
        self.0.skip_field(key)
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}
