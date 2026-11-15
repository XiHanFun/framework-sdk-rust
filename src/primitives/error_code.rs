use super::framework::FrameworkError;
use crate::grammar::{digit, upper, word};

/// 原语参数非法（空 Code、空消息键、清单跨字段约束等）。
pub const ERROR_CODE_INVALID_ARGUMENT: &str = "PRIMITIVES_VALIDATION_001";

/// 使用了未初始化的结果；消息键 [`MESSAGE_UNINITIALIZED_RESULT`](super::MESSAGE_UNINITIALIZED_RESULT)。
///
/// Rust 的 `Result` 没有缺省值或零值，SDK 不产生本码；保留供解码其他语言的错误与按键翻译。
pub const ERROR_CODE_UNINITIALIZED_RESULT: &str = "PRIMITIVES_VALIDATION_002";

/// 错误码的 KIND 段：校验失败。领域可以自定义其他 KIND。
pub const KIND_VALIDATION: &str = "VALIDATION";
/// 错误码的 KIND 段：不存在。
pub const KIND_NOT_FOUND: &str = "NOTFOUND";
/// 错误码的 KIND 段：冲突。
pub const KIND_CONFLICT: &str = "CONFLICT";
/// 错误码的 KIND 段：拒绝。
pub const KIND_DENIED: &str = "DENIED";
/// 错误码的 KIND 段：不可用。
pub const KIND_UNAVAILABLE: &str = "UNAVAILABLE";
/// 错误码的 KIND 段：超时。
pub const KIND_TIMEOUT: &str = "TIMEOUT";
/// 错误码的 KIND 段：取消。
pub const KIND_CANCELLED: &str = "CANCELLED";
/// 错误码的 KIND 段：意外。
pub const KIND_UNEXPECTED: &str = "UNEXPECTED";

/// 判断错误码是否符合 `<领域大写>_<KIND>_<NNN>`。
///
/// 文法 `^[A-Z][A-Z0-9]*_[A-Z][A-Z0-9]*_[0-9]{3}$`（`conventions/error-codes.md`）。
pub fn is_error_code(code: &str) -> bool {
    split(code).is_some()
}

/// 校验错误码并原样返回；不合法时返回 `PRIMITIVES_VALIDATION_001`。
pub fn validate_error_code(code: &str) -> Result<&str, FrameworkError> {
    if is_error_code(code) {
        Ok(code)
    } else {
        Err(FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, format!("The error code '{code}' must match <DOMAIN>_<KIND>_<NNN>.")))
    }
}

/// 返回错误码的 KIND 段；不合法时为 `None`。
pub fn error_code_kind(code: &str) -> Option<&str> {
    split(code).map(|(_, kind)| kind)
}

/// 返回错误码的领域段；不合法时为 `None`。
pub fn error_code_domain(code: &str) -> Option<&str> {
    split(code).map(|(domain, _)| domain)
}

/// 按文法拆出领域段与 KIND 段。
fn split(code: &str) -> Option<(&str, &str)> {
    let mut parts = code.split('_');
    let (domain, kind, sequence) = (parts.next()?, parts.next()?, parts.next()?);
    // 各段都不含下划线，多出的段（如 _EXTRA）即不合法
    if parts.next().is_some() {
        return None;
    }
    let upper_digit = |byte: u8| upper(byte) || digit(byte);
    let valid = word(domain, upper, upper_digit) && word(kind, upper, upper_digit) && sequence.len() == 3 && sequence.bytes().all(digit);
    valid.then_some((domain, kind))
}
