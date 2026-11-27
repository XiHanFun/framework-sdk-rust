//! 按值模型写出（RFC-0001「Result」、RFC-0010「模板」的值模型）。

use std::cell::Cell;

use serde::Serialize;
use serde::ser::Error as _;
use serde::ser::{
    SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple, SerializeTupleStruct, SerializeTupleVariant, Serializer,
};
use serde_json::Value;

use crate::primitives::ERROR_CODE_INVALID_ARGUMENT;

/// 按值模型写出任意可序列化的值：单精度浮点先换成最接近的双精度（0.1 写 `0.10000000149011612`），`i128` / `u128` 超出 64 位的
/// 是双精度浮点，非有限的浮点写成字符串 `NaN`、`Infinity`、`-Infinity`，其余照原样交给内层的序列化器。
///
/// `serde_json` 自己把非有限的浮点写成 `null`，对方就读不回同一个值；整数值的浮点它写成 `2.0`、`-0.0`、`1e17`，
/// 对方按模型读回的仍是浮点，不用另做处理。[`WireResult`](crate::primitives::WireResult) 的值经它写出。
///
/// ```
/// use xihan_framework::primitives::json::Model;
///
/// assert_eq!(serde_json::to_string(&Model(&[f64::NAN, 2.0])).unwrap(), r#"["NaN",2.0]"#);
/// assert_eq!(serde_json::to_string(&Model(&0.1f32)).unwrap(), "0.10000000149011612");
/// assert_eq!(serde_json::to_string(&Model(&(u128::from(u64::MAX) + 1))).unwrap(), "1.8446744073709552e+19");
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Model<'a, T: ?Sized>(pub &'a T);

impl<T: Serialize + ?Sized> Serialize for Model<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(ModelSerializer::<S, false>(serializer))
    }
}

/// 值与它的写法：`STRICT` 为假时按值模型（同 [`Model`]），为真时是类型化的响应——非有限的浮点写不出、报错，
/// 不写成 `null` 或字符串（RFC-0011 规则 2 与规则 10「编码」），单精度与 128 位整数照原样交给内层。
pub(crate) struct Wrap<'a, T: ?Sized, const STRICT: bool>(pub(crate) &'a T);

impl<T: Serialize + ?Sized, const STRICT: bool> Serialize for Wrap<'_, T, STRICT> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(ModelSerializer::<S, STRICT>(serializer))
    }
}

/// 把请求或响应对象写成载荷的 JSON（RFC-0011 规则 2 与规则 10「编码」）：调用器编码响应用的就是它。
///
/// 这是类型化的写法：非有限的浮点（及序列化不支持的值，如键不是字符串的映射）写不出、报错，不写成 `null` 或字符串；单精度与
/// 64 位整数照原样写。要把非有限的浮点写成字符串的是值模型的写法 [`Model`]。回调宿主的请求（feature `plugin` 的 `Host::invoke_json`）、
/// 信封的数据（`Envelope::with_json_data`）都经它写，读回来用 [`from_slice`](super::from_slice)。
///
/// 写出的整段文本还要过 RFC-0001「JSON 文本」的共同检查（[`check`](super::check)；ADR-0319、ADR-0340）：嵌套超过 [`MAX_DEPTH`](super::MAX_DEPTH)
/// 层（从这个值的根算起，原样嵌进去的 `RawValue` 与 [`Json`](super::Json) 也算）、任何一层有重复的成员名（自定义的 `Serialize` 两次写同名、
/// `#[serde(flatten)]` 的映射键与字段同名、`RawValue` 原样交出的）、`RawValue` 里孤立代理项的转义，都同样写不出，返回
/// `PRIMITIVES_VALIDATION_001` 的错误、不交出文本。嵌在里面的 [`WireResult`](crate::primitives::WireResult) 与
/// [`Error`](crate::primitives::Error) 由这一遍核对一并覆盖，不再各自先写一遍。
///
/// ```
/// use xihan_framework::primitives::json;
///
/// assert_eq!(json::to_vec(&[1.5, 2.0]).unwrap(), b"[1.5,2.0]");
/// assert!(json::to_vec(&f64::NAN).is_err());
/// assert_eq!(json::from_slice::<Vec<f64>>(&json::to_vec(&[1.5, 2.0]).unwrap()).unwrap(), [1.5, 2.0]);
/// let deep = (0..65).fold(serde_json::json!(0), |value, _| serde_json::json!([value]));
/// assert!(json::to_vec(&deep).is_err(), "65 levels cannot be written");
/// ```
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    // 写的时候标记外层会核对：嵌在里面的 Result 与 Error 不再先在缓冲里各写一遍（serialize_checked）。不论外面是否已在标记里都核对自己
    // 写出的这段文本：它是另一个缓冲，外层的核对看不到
    let text = {
        let _scope = CheckedScope(WITHIN_CHECKED.replace(true));
        serde_json::to_vec(&Wrap::<T, true>(value))?
    };
    // serde_json 的序列化器不数层数、不核对成员名，RawValue 的原文它也只照抄：写完之后对整段文本再读一遍（与调用器编码响应的做法相同）
    match super::check(&text) {
        Ok(()) => Ok(text),
        Err(problem) => Err(serde_json::Error::custom(format!("{ERROR_CODE_INVALID_ARGUMENT}: the value cannot be written as JSON text: {problem}."))),
    }
}

/// 按值模型把值转成 `serde_json::Value`：给错误细节与模板参数用，非有限的浮点成为字符串，而不是 `Value::from` 给出的 `null`。
///
/// 序列化失败（如映射的键不是字符串）时返回 `serde_json` 的错误。
pub fn to_value<T: Serialize + ?Sized>(value: &T) -> Result<Value, serde_json::Error> {
    Model(value).serialize(serde_json::value::Serializer)
}

thread_local! {
    /// 外层会核对整段文本：正在经 [`to_vec`] 写出，或正在为外层的线上编码核对与写出。嵌在里面的 Result 与 Error 不再各自核对，
    /// 外层核对的整段文本已经包括它们，层数从外层的根算起，不会比从它们自己的根算起少
    static WITHIN_CHECKED: Cell<bool> = const { Cell::new(false) };
}

/// 离开外层的核对时恢复原来的标记；写出途中 panic 也恢复。
struct CheckedScope(bool);

impl Drop for CheckedScope {
    fn drop(&mut self) {
        WITHIN_CHECKED.set(self.0);
    }
}

/// 按线上编码写出一个 Result 或 Error（RFC-0001「写出与转交」，ADR-0320）：先写成 JSON 文本过共同检查（[`check`](super::check)），
/// 通过了才交给 `serializer` 写出；没过即写不出，返回 `PRIMITIVES_VALIDATION_001` 的序列化错误、不交出文本。
///
/// Rust 的字符串总是合法的 UTF-8，写不出的是嵌套超过 64 层（从这个结果或错误的根算起）与 `RawValue`（含 [`Json`](super::Json)）
/// 原样写进去的文本里的重复成员、孤立代理项的转义：序列化器不数层数、不核对成员名，`RawValue` 的原文它只照抄，所以在缓冲里写一遍
/// 再读。嵌在里面的 Result 与 Error 由最外层一并核对，不再各写一遍；经 [`to_vec`] 写出时它写完之后核对整段文本，这里也不再先写一遍
/// （ADR-0340），结果相同：层数从更外的根算起只会更多，重复成员与转义在整段文本里同样看得到。
pub(crate) fn serialize_checked<T, S>(value: &T, what: &str, serializer: S) -> Result<S::Ok, S::Error>
where
    T: Serialize + ?Sized,
    S: Serializer,
{
    let outer = WITHIN_CHECKED.replace(true);
    let _scope = CheckedScope(outer);
    if !outer {
        let text = serde_json::to_vec(value).map_err(S::Error::custom)?;
        if let Err(problem) = super::check(&text) {
            return Err(S::Error::custom(format!("{ERROR_CODE_INVALID_ARGUMENT}: {what} cannot be written as JSON text: {problem}.")));
        }
    }
    value.serialize(serializer)
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

struct ModelSerializer<S, const STRICT: bool>(S);

/// 容器的包装：每个元素与成员同样按值模型写出。
struct Compound<C, const STRICT: bool>(C);

impl<S: Serializer, const STRICT: bool> Serializer for ModelSerializer<S, STRICT> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = Compound<S::SerializeSeq, STRICT>;
    type SerializeTuple = Compound<S::SerializeTuple, STRICT>;
    type SerializeTupleStruct = Compound<S::SerializeTupleStruct, STRICT>;
    type SerializeTupleVariant = Compound<S::SerializeTupleVariant, STRICT>;
    type SerializeMap = Compound<S::SerializeMap, STRICT>;
    type SerializeStruct = Compound<S::SerializeStruct, STRICT>;
    type SerializeStructVariant = Compound<S::SerializeStructVariant, STRICT>;

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
        if STRICT {
            return self.0.serialize_i128(value);
        }
        // 64 位之外的整数按值模型是双精度浮点（serde_json 本来也写不出）
        match (i64::try_from(value), u64::try_from(value)) {
            (Ok(small), _) => self.0.serialize_i64(small),
            (_, Ok(unsigned)) => self.0.serialize_u64(unsigned),
            _ => self.serialize_f64(value as f64),
        }
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
        if STRICT {
            return self.0.serialize_u128(value);
        }
        match u64::try_from(value) {
            Ok(small) => self.0.serialize_u64(small),
            Err(_) => self.serialize_f64(value as f64),
        }
    }

    fn serialize_f32(self, value: f32) -> Result<S::Ok, S::Error> {
        match STRICT {
            true if value.is_finite() => self.0.serialize_f32(value),
            _ => self.serialize_f64(f64::from(value)),
        }
    }

    fn serialize_f64(self, value: f64) -> Result<S::Ok, S::Error> {
        match (value.is_finite(), STRICT) {
            (true, _) => self.0.serialize_f64(value),
            // 类型化的响应写不出非有限的浮点，不悄悄换成 null 或字符串
            (false, true) => Err(S::Error::custom(format!("the floating-point value {} cannot be written as JSON", non_finite(value)))),
            (false, false) => self.0.serialize_str(non_finite(value)),
        }
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
        self.0.serialize_some(&Wrap::<_, STRICT>(value))
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
        self.0.serialize_newtype_struct(name, &Wrap::<_, STRICT>(value))
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(self, name: &'static str, index: u32, variant: &'static str, value: &T) -> Result<S::Ok, S::Error> {
        self.0.serialize_newtype_variant(name, index, variant, &Wrap::<_, STRICT>(value))
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

impl<C: SerializeSeq, const STRICT: bool> SerializeSeq for Compound<C, STRICT> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
        self.0.serialize_element(&Wrap::<_, STRICT>(value))
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeTuple, const STRICT: bool> SerializeTuple for Compound<C, STRICT> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
        self.0.serialize_element(&Wrap::<_, STRICT>(value))
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeTupleStruct, const STRICT: bool> SerializeTupleStruct for Compound<C, STRICT> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
        self.0.serialize_field(&Wrap::<_, STRICT>(value))
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeTupleVariant, const STRICT: bool> SerializeTupleVariant for Compound<C, STRICT> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
        self.0.serialize_field(&Wrap::<_, STRICT>(value))
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeMap, const STRICT: bool> SerializeMap for Compound<C, STRICT> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), C::Error> {
        self.0.serialize_key(&Wrap::<_, STRICT>(key))
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
        self.0.serialize_value(&Wrap::<_, STRICT>(value))
    }

    fn serialize_entry<K: Serialize + ?Sized, V: Serialize + ?Sized>(&mut self, key: &K, value: &V) -> Result<(), C::Error> {
        self.0.serialize_entry(&Wrap::<_, STRICT>(key), &Wrap::<_, STRICT>(value))
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeStruct, const STRICT: bool> SerializeStruct for Compound<C, STRICT> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), C::Error> {
        self.0.serialize_field(key, &Wrap::<_, STRICT>(value))
    }

    fn skip_field(&mut self, key: &'static str) -> Result<(), C::Error> {
        self.0.skip_field(key)
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeStructVariant, const STRICT: bool> SerializeStructVariant for Compound<C, STRICT> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), C::Error> {
        self.0.serialize_field(key, &Wrap::<_, STRICT>(value))
    }

    fn skip_field(&mut self, key: &'static str) -> Result<(), C::Error> {
        self.0.skip_field(key)
    }

    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}
