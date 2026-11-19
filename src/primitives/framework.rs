use std::error::Error as StdError;
use std::fmt;

use crate::grammar::is_blank;

use super::error::Error;
use super::error_code::{ERROR_CODE_INVALID_ARGUMENT, is_error_code};
use super::messages::{DEFAULT_LOCALE, Details, MessageResolver, builtin_catalog, describe};

/// 框架名（RFC-0001 FrameworkInfo）。
pub const NAME: &str = "XiHan Framework";

/// 本 SDK 实现的原语层大版本。
pub const SPEC_VERSION: &str = "1";

/// 握手时上报的 SDK 名。
pub const SDK_NAME: &str = "framework-sdk-rust";

/// 本 SDK 的版本。
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// SDK 仓库地址。
pub const REPOSITORY: &str = "https://github.com/XiHanFun/framework-sdk-rust";

/// 文档地址。
pub const DOCUMENTATION: &str = "https://framework.docs.xihanfun.com";

/// 配置或编程错误（RFC-0001 FrameworkException）：带错误码，消息是英文、不本地化。
///
/// 可恢复的业务失败用 [`Error`] 与 `Result`，不用它。清单校验、管道排序、操作登记等配置期失败以它返回；
/// 原语的非法参数（如不合文法的错误码）以它本身为 panic 的内容（`std::panic::panic_any`），接住 panic 的一方用
/// `downcast_ref::<FrameworkError>()` 精确认出它：调用器据此把它放进未处理失败的 `inner`（RFC-0011 规则 10）。
/// 标准库缺省的 panic hook 只打印字符串内容，对它打印 `Box<dyn Any>`；要看到错误码与说明，装自己的 hook 或接住后 downcast。
///
/// 有登记消息键的配置期错误（如 `KERNEL_OPERATION_001` + `kernel.operation.duplicate`，RFC-0011「错误码」）以
/// [`FrameworkError::with_message`] 创建，带上键与参数，说明取 SDK 自带的 en-US 模板，转成 [`Error`] 时一并带上。
#[derive(Debug)]
pub struct FrameworkError {
    code: String,
    message: String,
    message_key: Option<String>,
    details: Option<Details>,
    cause: Option<Box<dyn StdError + Send + Sync + 'static>>,
}

impl FrameworkError {
    /// 用错误码与英文说明创建。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), message_key: None, details: None, cause: None }
    }

    /// 用错误码、消息键与参数创建：说明是 SDK 自带的 en-US 模板填入参数，没有模板或缺参数时是「键 (参数)」。
    pub fn with_message(code: impl Into<String>, message_key: impl Into<String>, details: Details) -> Self {
        let message_key = message_key.into();
        let message = builtin_catalog().resolve(&message_key, DEFAULT_LOCALE, &details).unwrap_or_else(|| describe(&message_key, &details));
        Self { code: code.into(), message, message_key: Some(message_key), details: Some(details), cause: None }
    }

    /// 附上底层错误。
    pub fn with_cause(mut self, cause: impl Into<Box<dyn StdError + Send + Sync + 'static>>) -> Self {
        self.cause = Some(cause.into());
        self
    }

    /// 错误码。
    pub fn code(&self) -> &str {
        &self.code
    }

    /// 面向开发者的英文说明。
    pub fn message(&self) -> &str {
        &self.message
    }

    /// 消息键；以 [`FrameworkError::new`] 创建的为 `None`。
    pub fn message_key(&self) -> Option<&str> {
        self.message_key.as_deref()
    }

    /// 消息参数；以 [`FrameworkError::new`] 创建的为 `None`。
    pub fn details(&self) -> Option<&Details> {
        self.details.as_ref()
    }

    /// 底层错误；没有为 `None`。
    pub fn cause(&self) -> Option<&(dyn StdError + Send + Sync + 'static)> {
        self.cause.as_deref()
    }

    /// 转成可上线的 [`Error`]：带已解析文案，有消息键时连同键与参数；错误码不合文法时用 `PRIMITIVES_VALIDATION_001`、不带键。
    pub fn to_error(&self) -> Error {
        let code = if is_error_code(&self.code) { self.code.as_str() } else { ERROR_CODE_INVALID_ARGUMENT };
        // 说明为空或只有空白时用错误码作文案，错误必须有文案且不能只有空白（RFC-0001「空白字符」）
        let text = if is_blank(&self.message) { code } else { self.message.as_str() };
        let keyed = self.message_key.as_deref().filter(|_| code == self.code).and_then(|key| Error::try_new(code, key).ok());
        match keyed {
            Some(error) => error.with_details(self.details.clone().unwrap_or_default()).with_text(text),
            None => Error::literal(code, text),
        }
    }
}

impl fmt::Display for FrameworkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)?;
        if let Some(cause) = &self.cause {
            write!(formatter, " ({cause})")?;
        }
        Ok(())
    }
}

impl StdError for FrameworkError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.cause.as_deref().map(|cause| cause as &(dyn StdError + 'static))
    }
}

/// 以原语层的非法参数错误 panic：编程错误，消息是英文句子，panic 的内容就是这个 [`FrameworkError`]。
#[track_caller]
pub(crate) fn invalid_argument(message: String) -> ! {
    std::panic::panic_any(FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, message))
}
