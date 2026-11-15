//! 原语层（RFC-0001）的 Rust 形态：错误与错误码、结果、类型化标识、生命周期状态、管道阶段、稳定性、
//! Baggage 键与消息原语。
//!
//! 零行为、只增不改；语义与线上编码和 .NET 参考实现一致，由 framework-spec 的一致性向量验证。
//!
//! | 规范 | Rust |
//! | --- | --- |
//! | `Result` / `Result<T>` | [`Result<T>`]（标准库 `Result<T, Error>`），线上编码 [`WireResult`] |
//! | `Maybe<T>` | [`Maybe<T>`]（标准库 `Option<T>`），线上编码同 serde 的 `Option` |
//! | `Error` / `ErrorCode` | [`Error`]、[`is_error_code`] 等 |
//! | `TypedId` | [`TypedId`] |
//! | `FrameworkException` | [`FrameworkError`] |
//! | `MessageTemplate` / `IMessageResolver` / `MessageScope.Default` | [`format_message`]、[`MessageResolver`]、[`set_default_messages`] |

mod baggage;
mod error;
mod error_code;
mod framework;
mod id;
mod lifecycle;
mod messages;
mod named;
mod result;
mod stability;
mod stage;

pub use baggage::{
    BAGGAGE_CORRELATION, BAGGAGE_IDEMPOTENCY_KEY, BAGGAGE_LOCALE, BAGGAGE_MAX_VALUE_LENGTH, BAGGAGE_PRINCIPAL, BAGGAGE_TENANT, REGISTERED_BAGGAGE_KEYS,
    is_baggage_key_registered, is_baggage_key_well_formed, is_baggage_value_acceptable,
};
pub use error::Error;
pub use error_code::{
    ERROR_CODE_INVALID_ARGUMENT, ERROR_CODE_UNINITIALIZED_RESULT, KIND_CANCELLED, KIND_CONFLICT, KIND_DENIED, KIND_NOT_FOUND, KIND_TIMEOUT, KIND_UNAVAILABLE,
    KIND_UNEXPECTED, KIND_VALIDATION, error_code_domain, error_code_kind, is_error_code, validate_error_code,
};
pub(crate) use framework::invalid_argument;
pub use framework::{DOCUMENTATION, FrameworkError, NAME, REPOSITORY, SDK_NAME, SDK_VERSION, SPEC_VERSION};
pub use id::TypedId;
pub use lifecycle::LifecycleState;
pub use messages::{
    Catalog, DEFAULT_LOCALE, Details, MESSAGE_KEY_MAX_LENGTH, MessageResolver, default_locale, describe, format_message, format_value, is_message_key,
    message_key_domain, placeholders, positional, resolve_message, set_default_messages, try_format_message,
};
pub use result::{Maybe, Result, WireResult, wire_result};
pub use stability::Stability;
pub use stage::PipelineStage;
