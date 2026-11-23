use std::error::Error as StdError;
use std::fmt;

use serde::de::{self, Error as _, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::grammar::is_blank;

use super::error_code::{
    ERROR_CODE_INVALID_ARGUMENT, KIND_CANCELLED, KIND_CONFLICT, KIND_DENIED, KIND_NOT_FOUND, KIND_TIMEOUT, KIND_UNAVAILABLE, KIND_UNEXPECTED, KIND_VALIDATION,
    error_code_domain, error_code_kind, is_error_code, validate_error_code,
};
use super::framework::{FrameworkError, invalid_argument};
use super::messages::{Details, describe, is_message_key, resolve_with_builtin};

/// 没有细节时用来格式化的空参数表。
static NO_DETAILS: Details = Details::new();

/// 可恢复失败的描述（RFC-0001 Error）：错误码面向程序，消息键与参数面向人。
///
/// 值语义：`with_*` 方法消耗原值、返回新值。线上编码见 [`Serialize`] 的实现：
/// `{"code":…,"message":…,"messageKey":…,"details":{…},"inner":{…}}`，`message` 是发送端已解析的文案，
/// `messageKey` 让接收端可以再翻译。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    code: String,
    message_key: Option<String>,
    text: Option<String>,
    details: Option<Details>,
    inner: Option<Box<Error>>,
}

impl Error {
    /// 用错误码与消息键创建错误。
    ///
    /// # Panics
    ///
    /// 错误码不合 `<DOMAIN>_<KIND>_<NNN>` 或消息键不合文法时 panic（编程错误，内容是 `PRIMITIVES_VALIDATION_001` 的 [`FrameworkError`]）；
    /// 来自不可信输入的码与键用 [`Error::try_new`]。
    #[track_caller]
    pub fn new(code: impl Into<String>, message_key: impl Into<String>) -> Self {
        match Self::try_new(code, message_key) {
            Ok(error) => error,
            Err(error) => std::panic::panic_any(error),
        }
    }

    /// 用错误码与消息键创建错误；码或键不合法时返回 `PRIMITIVES_VALIDATION_001`。
    pub fn try_new(code: impl Into<String>, message_key: impl Into<String>) -> Result<Self, FrameworkError> {
        let (code, message_key) = (code.into(), message_key.into());
        validate_error_code(&code)?;
        if !is_message_key(&message_key) {
            return Err(FrameworkError::new(
                ERROR_CODE_INVALID_ARGUMENT,
                format!("The message key '{message_key}' must match <domain>.<segment>[.<segment>]."),
            ));
        }
        Ok(Self { code, message_key: Some(message_key), text: None, details: None, inner: None })
    }

    /// 用已解析的文案创建错误：线上还原与异常转换用，没有消息键。
    ///
    /// # Panics
    ///
    /// 错误码不合文法或文案为空、只有空白（RFC-0001「空白字符」）时 panic；来自不可信输入时用 [`Error::try_literal`]。
    #[track_caller]
    pub fn literal(code: impl Into<String>, text: impl Into<String>) -> Self {
        match Self::try_literal(code, text) {
            Ok(error) => error,
            Err(error) => std::panic::panic_any(error),
        }
    }

    /// 用已解析的文案创建错误；码不合法或文案为空、只有空白（RFC-0001「空白字符」）时返回 `PRIMITIVES_VALIDATION_001`。
    pub fn try_literal(code: impl Into<String>, text: impl Into<String>) -> Result<Self, FrameworkError> {
        let (code, text) = (code.into(), text.into());
        validate_error_code(&code)?;
        if is_blank(&text) {
            return Err(FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, "A literal error needs a text that is not empty or whitespace."));
        }
        Ok(Self { code, message_key: None, text: Some(text), details: None, inner: None })
    }

    /// 创建 `VALIDATION` 类错误。
    ///
    /// # Panics
    ///
    /// 同 [`Error::new`]，另外错误码的 KIND 段不是 `VALIDATION` 时 panic。下面各个按 KIND 的工厂同理。
    #[track_caller]
    pub fn validation(code: impl Into<String>, message_key: impl Into<String>) -> Self {
        Self::of_kind(KIND_VALIDATION, code, message_key)
    }

    /// 创建 `NOTFOUND` 类错误。
    #[track_caller]
    pub fn not_found(code: impl Into<String>, message_key: impl Into<String>) -> Self {
        Self::of_kind(KIND_NOT_FOUND, code, message_key)
    }

    /// 创建 `CONFLICT` 类错误。
    #[track_caller]
    pub fn conflict(code: impl Into<String>, message_key: impl Into<String>) -> Self {
        Self::of_kind(KIND_CONFLICT, code, message_key)
    }

    /// 创建 `DENIED` 类错误。
    #[track_caller]
    pub fn denied(code: impl Into<String>, message_key: impl Into<String>) -> Self {
        Self::of_kind(KIND_DENIED, code, message_key)
    }

    /// 创建 `UNAVAILABLE` 类错误。
    #[track_caller]
    pub fn unavailable(code: impl Into<String>, message_key: impl Into<String>) -> Self {
        Self::of_kind(KIND_UNAVAILABLE, code, message_key)
    }

    /// 创建 `TIMEOUT` 类错误。
    #[track_caller]
    pub fn timeout(code: impl Into<String>, message_key: impl Into<String>) -> Self {
        Self::of_kind(KIND_TIMEOUT, code, message_key)
    }

    /// 创建 `CANCELLED` 类错误。
    #[track_caller]
    pub fn cancelled(code: impl Into<String>, message_key: impl Into<String>) -> Self {
        Self::of_kind(KIND_CANCELLED, code, message_key)
    }

    /// 创建 `UNEXPECTED` 类错误；内部错误用 [`Error::with_inner`] 附上。
    #[track_caller]
    pub fn unexpected(code: impl Into<String>, message_key: impl Into<String>) -> Self {
        Self::of_kind(KIND_UNEXPECTED, code, message_key)
    }

    /// 创建错误并核对 KIND 段与工厂一致。
    #[track_caller]
    fn of_kind(kind: &str, code: impl Into<String>, message_key: impl Into<String>) -> Self {
        let error = Self::new(code, message_key);
        if error.kind() != kind {
            invalid_argument(format!("The error code '{}' must have the kind {kind}.", error.code));
        }
        error
    }

    /// 换上细节（同时是模板参数），替换原有的细节。
    #[must_use]
    pub fn with_details(mut self, details: Details) -> Self {
        self.details = Some(details);
        self
    }

    /// 加一项细节。
    #[must_use]
    pub fn with_detail(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.details.get_or_insert_with(Details::new).insert(name.into(), value.into());
        self
    }

    /// 附上内部错误。
    #[must_use]
    pub fn with_inner(mut self, inner: Error) -> Self {
        self.inner = Some(Box::new(inner));
        self
    }

    /// 换错误码，保留键、文案、细节与内部错误。
    ///
    /// # Panics
    ///
    /// 错误码不合文法时 panic。
    #[track_caller]
    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        let code = code.into();
        if let Err(error) = validate_error_code(&code) {
            std::panic::panic_any(error);
        }
        self.code = code;
        self
    }

    /// 填入已解析的文案。
    ///
    /// # Panics
    ///
    /// 文案为空或只有空白（RFC-0001「空白字符」）时 panic；来自不可信输入时用 [`Error::try_with_text`]。
    #[track_caller]
    #[must_use]
    pub fn with_text(self, text: impl Into<String>) -> Self {
        match self.try_with_text(text) {
            Ok(error) => error,
            Err(error) => std::panic::panic_any(error),
        }
    }

    /// 填入已解析的文案；文案为空或只有空白（RFC-0001「空白字符」）时返回 `PRIMITIVES_VALIDATION_001`。
    pub fn try_with_text(mut self, text: impl Into<String>) -> Result<Self, FrameworkError> {
        let text = text.into();
        if is_blank(&text) {
            return Err(FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, "The text of an error must not be empty or whitespace."));
        }
        self.text = Some(text);
        Ok(self)
    }

    /// 错误码 `<DOMAIN>_<KIND>_<NNN>`。
    pub fn code(&self) -> &str {
        &self.code
    }

    /// 消息键；字面量错误为 `None`。
    pub fn message_key(&self) -> Option<&str> {
        self.message_key.as_deref()
    }

    /// 已解析的文案；未解析为 `None`。
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// 细节，也是模板参数；没有为 `None`。
    pub fn details(&self) -> Option<&Details> {
        self.details.as_ref()
    }

    /// 内部错误；没有为 `None`。
    pub fn inner(&self) -> Option<&Error> {
        self.inner.as_deref()
    }

    /// 错误码的 KIND 段。
    pub fn kind(&self) -> &str {
        // 构造与解码都校验过错误码，KIND 段一定存在
        error_code_kind(&self.code).unwrap_or_default()
    }

    /// 错误码的领域段。
    pub fn domain(&self) -> &str {
        error_code_domain(&self.code).unwrap_or_default()
    }

    /// 面向人的消息：已解析文案；否则按进程默认区域解析消息键，再按 SDK 自带的 en-US 模板（[`builtin_catalog`](super::builtin_catalog)）；
    /// 都解析不出时是「键 (参数)」；没有键时是错误码。
    pub fn message(&self) -> String {
        if let Some(text) = &self.text {
            return text.clone();
        }
        let Some(key) = &self.message_key else {
            return self.code.clone();
        };
        let arguments = self.details.as_ref().unwrap_or(&NO_DETAILS);
        resolve_with_builtin(key, arguments).unwrap_or_else(|| describe(key, arguments))
    }
}

impl fmt::Display for Error {
    /// 码 + 消息：`ORDERS_NOTFOUND_001: Order 42 was not found.`。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message())
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.inner.as_deref().map(|inner| inner as &(dyn StdError + 'static))
    }
}

/// 线上编码的写出形态（RFC-0001「Result」）：成员按 `code`、`message`、`messageKey`、`details`、`inner` 写出；顺序不是契约，
/// 向量 `wire-encoding.json` 按 JSON 值比较。
#[derive(Serialize)]
struct ErrorOut<'a> {
    code: &'a str,
    message: String,
    #[serde(rename = "messageKey", skip_serializing_if = "Option::is_none")]
    message_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a Details>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inner: Option<&'a Error>,
}

impl Serialize for Error {
    /// 写出线上编码：`message` 是 [`Error::message`]，没有的键、空细节与内部错误省略。
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ErrorOut {
            code: &self.code,
            message: self.message(),
            message_key: self.message_key.as_deref(),
            details: self.details.as_ref().filter(|details| !details.is_empty()),
            inner: self.inner.as_deref(),
        }
        .serialize(serializer)
    }
}

/// 逐成员读入线上编码（RFC-0001「Result」）：只收 JSON 对象，未知成员忽略，可选成员写成 `null` 不合法。
struct ErrorVisitor;

/// 记下一个成员的值；同一成员出现两次即不合法。
fn set_once<T, E: de::Error>(slot: &mut Option<T>, value: T, name: &'static str) -> Result<(), E> {
    if slot.replace(value).is_some() {
        return Err(E::duplicate_field(name));
    }
    Ok(())
}

impl<'de> Visitor<'de> for ErrorVisitor {
    type Value = Error;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(r#"an error object {"code":…,"message":…}"#)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Error, A::Error> {
        let (mut code, mut message, mut message_key, mut details, mut inner) = (None, None, None, None::<Details>, None::<Error>);
        while let Some(name) = map.next_key::<String>()? {
            // 各成员按类型读入：null 读不成字符串、对象或 Error，因此可选成员写成 null 也被拒绝
            match name.as_str() {
                "code" => set_once(&mut code, map.next_value::<String>()?, "code")?,
                "message" => set_once(&mut message, map.next_value::<String>()?, "message")?,
                "messageKey" => set_once(&mut message_key, map.next_value::<String>()?, "messageKey")?,
                "details" => set_once(&mut details, map.next_value::<Details>()?, "details")?,
                "inner" => set_once(&mut inner, map.next_value::<Error>()?, "inner")?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        let code = code.ok_or_else(|| A::Error::missing_field("code"))?;
        if !is_error_code(&code) {
            return Err(A::Error::custom(format!("{ERROR_CODE_INVALID_ARGUMENT}: The error code '{code}' must match <DOMAIN>_<KIND>_<NNN>.")));
        }
        let message = message.ok_or_else(|| A::Error::missing_field("message"))?;
        // 空串与只有空白都不合法（RFC-0001「Result」与「空白字符」），两端的空白原样保留
        if is_blank(&message) {
            return Err(A::Error::custom(format!("{ERROR_CODE_INVALID_ARGUMENT}: The message of the error '{code}' must not be empty or whitespace.")));
        }
        if let Some(key) = message_key.as_deref().filter(|key| !is_message_key(key)) {
            return Err(A::Error::custom(format!("{ERROR_CODE_INVALID_ARGUMENT}: The message key '{key}' must match <domain>.<segment>[.<segment>].")));
        }
        Ok(Error { code, message_key, text: Some(message), details, inner: inner.map(Box::new) })
    }
}

impl<'de> Deserialize<'de> for Error {
    /// 从线上编码还原：`message` 成为已解析文案，消息键原样保留、只用于再翻译。
    ///
    /// `code` 与 `message` 必填，`message` 不能为空或只有空白（RFC-0001「空白字符」）；`messageKey`、`details`、`inner` 写了就必须合文法、是对象、是 Error，
    /// 写成 `null` 不合法；未知成员忽略；不是 JSON 对象（含数组形式）即失败。
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ErrorVisitor)
    }
}
