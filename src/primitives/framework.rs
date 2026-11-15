use std::error::Error as StdError;
use std::fmt;

use super::error::Error;
use super::error_code::{ERROR_CODE_INVALID_ARGUMENT, is_error_code};

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
/// 原语的非法参数（如不合文法的错误码）以它为内容 panic。
#[derive(Debug)]
pub struct FrameworkError {
    code: String,
    message: String,
    cause: Option<Box<dyn StdError + Send + Sync + 'static>>,
}

impl FrameworkError {
    /// 用错误码与英文说明创建。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), cause: None }
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

    /// 底层错误；没有为 `None`。
    pub fn cause(&self) -> Option<&(dyn StdError + Send + Sync + 'static)> {
        self.cause.as_deref()
    }

    /// 转成可上线的 [`Error`]（字面量错误，带已解析文案）；错误码不合文法时用 `PRIMITIVES_VALIDATION_001`。
    pub fn to_error(&self) -> Error {
        let code = if is_error_code(&self.code) { self.code.as_str() } else { ERROR_CODE_INVALID_ARGUMENT };
        // 说明为空时用错误码作文案，字面量错误必须有文案
        let text = if self.message.is_empty() { code } else { self.message.as_str() };
        Error::literal(code, text)
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

/// 以原语层的非法参数错误 panic：编程错误，消息是英文句子。
#[track_caller]
pub(crate) fn invalid_argument(message: String) -> ! {
    panic!("{}", FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, message))
}
