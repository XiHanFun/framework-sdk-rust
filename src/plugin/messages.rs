//! 插件运行时自己产生的错误：错误码取 RFC-0017「错误码」与 `conventions/error-codes.md` 登记的 `PLUGINS_*`，
//! 消息键与参数同 .NET 参考实现的插件协议目录，文案在 `localization/<区域>.json`（RFC-0010，10 种区域）。

use crate::primitives::{Error, LifecycleState};

/// 插件进程未就绪、正在停止或不可达：`Start` 之前与 `Stop` 之后收到的调用（`lifecycle.md` §2、§5），回调时宿主不可达。
pub const ERROR_CODE_UNAVAILABLE: &str = "PLUGINS_UNAVAILABLE_001";
/// 插件回调被拒：会话令牌无效、所需权限未在清单声明或未被授予、带了宿主没有交给插件的租户（`invocation.md` §5）。
pub const ERROR_CODE_CALLBACK_DENIED: &str = "PLUGINS_DENIED_002";
/// 配置、启动、停止或调用时的未预期失败。
pub const ERROR_CODE_UNEXPECTED: &str = "PLUGINS_UNEXPECTED_001";

/// 插件不可用的消息键；参数 `id`（清单 ID）、`state`（当前生命周期状态）。
pub const MESSAGE_UNAVAILABLE: &str = "plugins.process.unavailable";
/// 某个阶段失败的消息键；参数 `id`、`phase`（RPC 或钩子名）、`reason`（英文说明）。
pub const MESSAGE_FAILED: &str = "plugins.process.failed";
/// 回调所需权限未在清单声明或未被授予的消息键；参数 `id`、`permission`。
pub const MESSAGE_CALLBACK_DENIED: &str = "plugins.callback.denied";
/// 回调或发布带的租户不是宿主交给插件的调用的租户的消息键；参数 `id`、`tenant`（ADR-0195）。
pub const MESSAGE_CALLBACK_TENANT_DENIED: &str = "plugins.callback.tenant_denied";
/// 回调的会话令牌无效的消息键；没有参数。
pub const MESSAGE_SESSION_INVALID: &str = "plugins.callback.session_invalid";

/// 上面这些键的 en-US 模板，同 `localization/en-US.json`；打开 feature `plugin` 时并进 SDK 自带的模板
/// （[`builtin_catalog`](crate::primitives::builtin_catalog)），没有设置进程默认解析器时 `Error::message` 也能给出文案。
pub(crate) const BUILTIN_TEMPLATES: &[(&str, &str)] = &[
    (MESSAGE_CALLBACK_DENIED, "Plugin '{id}' may not use permission '{permission}': it must be declared in the manifest and granted by the host."),
    (MESSAGE_SESSION_INVALID, "The plugin session token is missing or invalid."),
    (
        MESSAGE_CALLBACK_TENANT_DENIED,
        "Plugin '{id}' may not act in tenant '{tenant}': a callback can only carry the tenant of a call the host has handed to the plugin.",
    ),
    (MESSAGE_FAILED, "Plugin '{id}' failed during {phase}: {reason}"),
    (MESSAGE_UNAVAILABLE, "Plugin '{id}' is not available (state: {state})."),
];

/// 插件不可用：在途调用之外的状态（未启动、排空中、已失败）。
pub(crate) fn unavailable(id: &str, state: LifecycleState) -> Error {
    Error::unavailable(ERROR_CODE_UNAVAILABLE, MESSAGE_UNAVAILABLE).with_detail("id", id).with_detail("state", state.name())
}

/// 某个阶段的未预期失败：钩子 panic、找不到投递的订阅、宿主回了不合法的错误。
pub(crate) fn failed(id: &str, phase: &str, reason: impl Into<String>) -> Error {
    Error::unexpected(ERROR_CODE_UNEXPECTED, MESSAGE_FAILED).with_detail("id", id).with_detail("phase", phase).with_detail("reason", reason.into())
}

/// 回调时宿主不可达：传输层失败，同样用阶段失败的消息，错误码是不可用。
pub(crate) fn unreachable(id: &str, phase: &str, reason: impl Into<String>) -> Error {
    Error::unavailable(ERROR_CODE_UNAVAILABLE, MESSAGE_FAILED).with_detail("id", id).with_detail("phase", phase).with_detail("reason", reason.into())
}

/// 回调所需权限未在清单声明或未被授予。
pub(crate) fn denied(id: &str, permission: &str) -> Error {
    Error::denied(ERROR_CODE_CALLBACK_DENIED, MESSAGE_CALLBACK_DENIED).with_detail("id", id).with_detail("permission", permission)
}

/// 回调或发布带了宿主没有交给插件的租户。
pub(crate) fn tenant_denied(id: &str, tenant: &str) -> Error {
    Error::denied(ERROR_CODE_CALLBACK_DENIED, MESSAGE_CALLBACK_TENANT_DENIED).with_detail("id", id).with_detail("tenant", tenant)
}

/// 宿主说回调的会话令牌无效。
pub(crate) fn session_invalid() -> Error {
    Error::denied(ERROR_CODE_CALLBACK_DENIED, MESSAGE_SESSION_INVALID)
}
