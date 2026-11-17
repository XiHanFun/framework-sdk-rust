//! XiHan 读写 JSON 文本的共同规则（RFC-0001「JSON 文本」与「Result」，RFC-0010 的值模型）。
//!
//! Result 与 Error 的线上编码、操作载荷与清单都先过 [`check`] 这一遍，任一条不满足即不合法：
//!
//! - 语法按 RFC 8259：没有注释、尾逗号、单引号、`NaN` / `Infinity`、前导零、正号与多个根值；空文本与只有空白也不是 JSON。
//!   文本是 UTF-8，开头的 BOM 是语法错误。
//! - 嵌套不超过 [`MAX_DEPTH`] 层，最外层的对象或数组是第 1 层。
//! - 字符串与成员名必须是合法的 Unicode：没有非法的 UTF-8（含过长编码与编码成 UTF-8 的代理项）、原始控制字符与非法转义，
//!   代理项的转义成对出现。
//! - 任何一层都不能有重复的成员名，名字按转义还原后比较（`"a"` 与 `"a"` 重复，大小写不同不算）。
//! - 语法错误不论出现在哪里都优先，位置只写 `$`；否则报第一处重复（第二次出现的成员）的路径。
//!
//! [`from_slice`] 先检查，再按 serde 读成目标类型，数字按值模型：整数写法（没有小数点与指数）在 64 位范围内是整数、
//! 原样往返（9007199254740993 不会变成 9007199254740992），其余是双精度浮点，上溢为无穷、下溢为 0；对象位置只收 JSON 对象。
//! 读线上的 Result 与 Error 用它，不要直接用 `serde_json::from_str`：后者对重复成员取最后一个，也不查值与细节里面。
//!
//! ```
//! use xihan_framework::primitives::json::{self, ReadError};
//! use xihan_framework::primitives::WireResult;
//!
//! assert_eq!(json::check(br#"{"a":{"b":1,"b":2}}"#), Err(ReadError::Duplicate { path: "$.a.b".to_owned() }));
//! assert_eq!(json::check(b"\xEF\xBB\xBF{}"), Err(ReadError::Syntax));
//! let result: WireResult<u64> = json::from_str(r#"{"ok":true,"value":9007199254740993}"#).unwrap();
//! assert_eq!(result.into_inner().unwrap(), 9007199254740993);
//! ```

mod de;
#[cfg(feature = "plugin")]
mod lenient;
mod reader;
mod ser;
mod value;

use std::fmt::{self, Write as _};

use serde::de::DeserializeOwned;
use serde_path_to_error::Segment;

#[cfg(feature = "plugin")]
pub(crate) use reader::Number;
pub(crate) use reader::{Node, parse};
pub(crate) use ser::to_vec_strict;
pub use ser::{Model, to_value};
pub use value::Json;

/// 嵌套的最大层数（RFC-0001「JSON 文本」）。
pub const MAX_DEPTH: usize = 64;

/// 读取 JSON 文本失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadError {
    /// 不是 JSON：RFC 8259 之外的写法、BOM、嵌套过深、非法 UTF-8、孤立的代理项；位置只写 `$`。
    Syntax,
    /// 有重复的成员；`path` 是第一处重复（第二次出现的成员）的路径。
    Duplicate {
        /// 第一处重复的路径，如 `$.a.b`。
        path: String,
    },
    /// 值与目标类型不符（含缺少必填的成员）；`path` 是出错的位置，缺少成员时是所在对象的路径。
    Mismatch {
        /// 出错位置的路径，根上为 `$`。
        path: String,
        /// serde 给出的英文说明，只给人看。
        message: String,
    },
}

impl ReadError {
    /// 出错的位置（RFC-0011 规则 2 的写法）：语法错误为 `$`。
    pub fn path(&self) -> &str {
        match self {
            Self::Syntax => "$",
            Self::Duplicate { path } | Self::Mismatch { path, .. } => path,
        }
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax => formatter.write_str("the text is not JSON"),
            Self::Duplicate { path } => write!(formatter, "the member '{path}' appears more than once"),
            Self::Mismatch { path, message } => write!(formatter, "the value at '{path}' does not match: {message}"),
        }
    }
}

impl std::error::Error for ReadError {}

/// 按共同读取检查核对 UTF-8 文本；通过时返回 `Ok(())`，否则是 [`ReadError::Syntax`] 或 [`ReadError::Duplicate`]。
pub fn check(text: &[u8]) -> Result<(), ReadError> {
    parse(text).map(drop)
}

/// 先过共同读取检查，再按 serde 读成 `T`；数字按值模型，对象位置只收对象，成员按文本顺序读，所以报的是文本里的第一处问题。
pub fn from_slice<T: DeserializeOwned>(text: &[u8]) -> Result<T, ReadError> {
    decode(&parse(text)?)
}

/// 同 [`from_slice`]。
pub fn from_str<T: DeserializeOwned>(text: &str) -> Result<T, ReadError> {
    from_slice(text.as_bytes())
}

/// 从已读出的值树按 serde 读成 `T`，失败时带出错位置的路径。
pub(crate) fn decode<T: DeserializeOwned>(node: &Node) -> Result<T, ReadError> {
    serde_path_to_error::deserialize(node).map_err(|error| ReadError::Mismatch { path: path_text(error.path()), message: error.into_inner().to_string() })
}

/// 同 [`decode`]，按配置节放宽的读法（`plugin/lifecycle.md` §7.2）：成员名不区分 ASCII 大小写，字符串叶子可以读成数字与布尔。
#[cfg(feature = "plugin")]
pub(crate) fn decode_lenient<T: DeserializeOwned>(node: &Node) -> Result<T, ReadError> {
    serde_path_to_error::deserialize(lenient::Lenient(node))
        .map_err(|error| ReadError::Mismatch { path: path_text(error.path()), message: error.into_inner().to_string() })
}

/// 成员名里需要写成 `['名字']` 的字符（RFC-0011 规则 2，照 System.Text.Json 的实测）。
const BRACKETED: &[char] = &['.', ' ', '\'', '/', '"', '[', ']', '(', ')', '\\', '\t', '\n', '\r', '\u{0C}', '\u{08}', '\u{85}', '\u{2028}', '\u{2029}'];

/// 在路径后接一个成员：名字含 [`BRACKETED`] 里的字符时写 `['名字']`，名字原样、不转义；其余写 `.名字`，空名字即 `.`。
pub(crate) fn push_member(path: &mut String, name: &str) {
    if name.contains(BRACKETED) {
        path.push_str("['");
        path.push_str(name);
        path.push_str("']");
    } else {
        path.push('.');
        path.push_str(name);
    }
}

/// 在路径后接一个数组下标 `[i]`。
pub(crate) fn push_index(path: &mut String, index: usize) {
    // 写进 String 不会失败
    let _ = write!(path, "[{index}]");
}

/// serde 报出的路径写成 RFC-0011 规则 2 的形式：根是 `$`，成员 `.名字` 或 `['名字']`，下标 `[i]`。
fn path_text(path: &serde_path_to_error::Path) -> String {
    let mut text = String::from("$");
    for segment in path.iter() {
        match segment {
            Segment::Seq { index } => push_index(&mut text, *index),
            Segment::Map { key } | Segment::Enum { variant: key } => push_member(&mut text, key),
            // 路径不明的一段（如未标记的枚举内部）不写出
            Segment::Unknown => {}
        }
    }
    text
}

/// 按 JSON 字符串写法写出（RFC-0010「模板」，清单写出也用它）：`"` 与 `\` 加反斜杠，退格、换页、换行、回车、制表符用短写，
/// 其余 U+0000–U+001F 写成小写十六进制的 `\u001f`，别的字符（含 `/`、U+007F–U+009F、U+2028 与非 ASCII）原样。
///
/// Rust 的字符串里没有孤立的代理项，规范里它们的 `\ud800` 写法在这里用不到。
pub(crate) fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for char in text.chars() {
        match char {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control < ' ' => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_escape_only_what_must_be_escaped() {
        let mut out = String::new();
        write_string(&mut out, "q\"b\\s/\n\t\u{08}\u{0C}\r\u{01}\u{1F} é\u{7F}\u{85}\u{2028}中");
        assert_eq!(out, "\"q\\\"b\\\\s/\\n\\t\\b\\f\\r\\u0001\\u001f é\u{7F}\u{85}\u{2028}中\"");
    }

    #[test]
    fn numbers_follow_the_value_model() {
        use reader::Number;
        let numbers = |text: &str| match parse(text.as_bytes()).unwrap() {
            Node::Number(number, _) => number,
            other => panic!("{other:?}"),
        };
        assert_eq!(numbers("-0"), Number::Integer(0));
        assert_eq!(numbers("9007199254740993"), Number::Integer(9_007_199_254_740_993));
        assert_eq!(numbers("18446744073709551615"), Number::Unsigned(u64::MAX));
        assert_eq!(numbers("18446744073709551616"), Number::Float(18_446_744_073_709_551_616.0));
        assert_eq!(numbers("1e400"), Number::Float(f64::INFINITY));
        assert_eq!(numbers("1e-400"), Number::Float(0.0));
        assert!(matches!(numbers("-0.0"), Number::Float(zero) if zero == 0.0 && zero.is_sign_negative()));
        assert_eq!(numbers("2.0"), Number::Float(2.0));
    }
}
