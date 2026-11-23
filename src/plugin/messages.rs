//! 插件运行时与宿主共用的消息键（`invocation.md` §8，向量 `plugin/plugin-messages.json`）：错误码、键、参数（也就是错误的
//! `details`，不另加成员）与 en-US 模板以向量为准，10 种区域的文案在 `localization/<区域>.json`（RFC-0010，同参考实现）。
//! 宿主有这些键的目录，运行时不随握手发送它们（`handshake.md` §3 第 6 条）。参数 `id` 一律是插件的清单 ID。

use crate::operation::ERROR_CODE_NOT_FOUND;
use crate::primitives::{Error, LifecycleState};

/// 插件进程不可用（RFC-0017「错误码」）：不在可服务状态时到达的调用与投递（`lifecycle.md` §7.3），回调连不上宿主或没有可用的
/// 回调端点（`invocation.md` §5）。
pub const ERROR_CODE_UNAVAILABLE: &str = "PLUGINS_UNAVAILABLE_001";
/// 插件回调被拒：会话令牌无效、所需权限未在清单声明或未被授予、带了宿主没有交给插件的租户（`invocation.md` §5）。
pub const ERROR_CODE_CALLBACK_DENIED: &str = "PLUGINS_DENIED_002";
/// 插件一侧的未预期失败（.NET 的 `PluginErrors.Unexpected`）：生命周期或健康钩子 panic、配置节不是 JSON 对象、对方写来的错误不合法、
/// 没有对应的 gRPC 状态。
///
/// 名字带 `PLUGIN`：内核的 [`operation::ERROR_CODE_UNEXPECTED`](crate::operation::ERROR_CODE_UNEXPECTED) 是另一个码
/// （`KERNEL_UNEXPECTED_001`），两个模块一起引入时不会认错。
pub const ERROR_CODE_PLUGIN_UNEXPECTED: &str = "PLUGINS_UNEXPECTED_001";
/// 插件的清单、种类或隔离级别不合法（.NET 的 `PluginErrors.InvalidSource`）：[`Plugin::start`](super::Plugin::start) 在清单不是
/// `kind: plugin`、`isolation: out-of-process` 或写了 `contributes.pipeline` 时报它，宿主发现插件时也以它拒绝。
pub const ERROR_CODE_INVALID_SOURCE: &str = "PLUGINS_VALIDATION_001";
/// 握手响应与清单不一致（.NET 的 `PluginErrors.HandshakeMismatch`）：[`Plugin::start`](super::Plugin::start) 在原语层版本不符、登记的
/// 操作不在清单里、订阅重复或不合法时报它，宿主会以同一个码拒绝握手（`handshake.md` §3）。
pub const ERROR_CODE_HANDSHAKE_MISMATCH: &str = "PLUGINS_VALIDATION_004";

/// 插件不可用的消息键，错误码 [`ERROR_CODE_UNAVAILABLE`]；参数 `id`、`state`（插件此刻的状态名，如 `Configured`）。
pub const MESSAGE_UNAVAILABLE: &str = "plugins.process.unavailable";
/// 插件一侧失败的消息键，错误码 [`ERROR_CODE_PLUGIN_UNEXPECTED`]；参数 `id`、`phase`（小写：`configure`、`start`、`stop`、`health`、
/// `invoke`、`publish`，或回调的操作 ID）、`reason`（英文诊断文本，不本地化）。
pub const MESSAGE_FAILED: &str = "plugins.process.failed";
/// 投递找不到订阅的消息键，错误码 `KERNEL_NOTFOUND_001`；参数 `id`、`topic`、`type`、`mode`（空的方式写 `competing`）。
pub const MESSAGE_UNSUBSCRIBED: &str = "plugins.event.unsubscribed";
/// 回调连不上宿主的消息键，错误码 [`ERROR_CODE_UNAVAILABLE`]；参数 `id`、`reason`（`<gRPC 状态名>: <说明>`，或
/// `no supported callback endpoint`）。
pub const MESSAGE_HOST_UNAVAILABLE: &str = "plugins.host.unavailable";
/// 回调所需权限未在清单声明或未被授予的消息键，错误码 [`ERROR_CODE_CALLBACK_DENIED`]；参数 `id`、`permission`。
pub const MESSAGE_CALLBACK_DENIED: &str = "plugins.callback.denied";
/// 回调带的租户不是宿主交给插件的调用的租户的消息键，错误码 [`ERROR_CODE_CALLBACK_DENIED`]；参数 `id`、`tenant`（ADR-0195）。
pub const MESSAGE_CALLBACK_TENANT_DENIED: &str = "plugins.callback.tenant_denied";
/// 宿主拒绝了回调的会话令牌的消息键，错误码 [`ERROR_CODE_CALLBACK_DENIED`]；没有参数。
pub const MESSAGE_SESSION_INVALID: &str = "plugins.callback.session_invalid";

/// `phase`：`Configure`。
pub(crate) const PHASE_CONFIGURE: &str = "configure";
/// `phase`：`Start`。
pub(crate) const PHASE_START: &str = "start";
/// `phase`：`Stop`。
pub(crate) const PHASE_STOP: &str = "stop";
/// `phase`：健康钩子。
pub(crate) const PHASE_HEALTH: &str = "health";
/// `phase`：读入对方写来的错误码不合文法时（`invocation.md` §3）。
pub(crate) const PHASE_INVOKE: &str = "invoke";
/// `phase`：`Host.Publish`。
pub(crate) const PHASE_PUBLISH: &str = "publish";

/// `reason`：配置节不是 JSON 对象（`lifecycle.md` §7.2）。
pub(crate) const REASON_NOT_AN_OBJECT: &str = "the configuration is not a JSON object";
/// `reason`：没有可用的回调端点（`handshake.md` §4）。
pub(crate) const REASON_NO_ENDPOINT: &str = "no supported callback endpoint";
/// `reason`：宿主的 `Status.ok` 为假却没有 `error`（`invocation.md` §3）。
pub(crate) const REASON_NO_ERROR: &str = "the host reported a failure without an error";
/// `reason`：类型化的回调（`Host::invoke_json`）的请求写不成 JSON；诊断文本里不带值。
pub(crate) const REASON_REQUEST_NOT_JSON: &str = "the request cannot be written as JSON";
/// `reason`：钩子以 panic 结束（`lifecycle.md` §7.6：没有异常的语言写 `panic`，panic 的内容只进 stderr）。
pub(crate) const REASON_PANIC: &str = "panic";

/// 上面这些键的 en-US 模板，逐字同向量 `plugin-messages.json` 与 `localization/en-US.json`；打开 feature `plugin` 时并进 SDK 自带的
/// 模板（[`builtin_catalog`](crate::primitives::builtin_catalog)），没有设置进程默认解析器时 `Error::message` 也能给出文案。
pub(crate) const BUILTIN_TEMPLATES: &[(&str, &str)] = &[
    (MESSAGE_CALLBACK_DENIED, "Plugin '{id}' may not use permission '{permission}': it must be declared in the manifest and granted by the host."),
    (MESSAGE_SESSION_INVALID, "The plugin session token is missing or invalid."),
    (
        MESSAGE_CALLBACK_TENANT_DENIED,
        "Plugin '{id}' may not act in tenant '{tenant}': a callback can only carry the tenant of a call the host has handed to the plugin.",
    ),
    (MESSAGE_UNSUBSCRIBED, "Plugin '{id}' has no subscription for topic '{topic}', type '{type}' and mode '{mode}'."),
    (MESSAGE_HOST_UNAVAILABLE, "Plugin '{id}' cannot reach the host: {reason}"),
    (MESSAGE_FAILED, "Plugin '{id}' failed during {phase}: {reason}"),
    (MESSAGE_UNAVAILABLE, "Plugin '{id}' is not available (state: {state})."),
];

/// 插件不在可服务状态（`lifecycle.md` §7.3）。
pub(crate) fn unavailable(id: &str, state: LifecycleState) -> Error {
    Error::unavailable(ERROR_CODE_UNAVAILABLE, MESSAGE_UNAVAILABLE).with_detail("id", id).with_detail("state", state.name())
}

/// 插件一侧某个阶段的失败。
pub(crate) fn failed(id: &str, phase: &str, reason: impl Into<String>) -> Error {
    Error::unexpected(ERROR_CODE_PLUGIN_UNEXPECTED, MESSAGE_FAILED).with_detail("id", id).with_detail("phase", phase).with_detail("reason", reason.into())
}

/// 投递找不到（主题，方式，类型）匹配的订阅（`invocation.md` §4）。
pub(crate) fn unsubscribed(id: &str, topic: &str, event_type: &str, mode: &str) -> Error {
    Error::not_found(ERROR_CODE_NOT_FOUND, MESSAGE_UNSUBSCRIBED)
        .with_detail("id", id)
        .with_detail("topic", topic)
        .with_detail("type", event_type)
        .with_detail("mode", mode)
}

/// 回调连不上宿主，或没有可用的回调端点。
pub(crate) fn host_unavailable(id: &str, reason: impl Into<String>) -> Error {
    Error::unavailable(ERROR_CODE_UNAVAILABLE, MESSAGE_HOST_UNAVAILABLE).with_detail("id", id).with_detail("reason", reason.into())
}

/// 回调所需权限未在清单声明或未被授予。
pub(crate) fn denied(id: &str, permission: &str) -> Error {
    Error::denied(ERROR_CODE_CALLBACK_DENIED, MESSAGE_CALLBACK_DENIED).with_detail("id", id).with_detail("permission", permission)
}

/// 回调带了宿主没有交给插件的租户。
pub(crate) fn tenant_denied(id: &str, tenant: &str) -> Error {
    Error::denied(ERROR_CODE_CALLBACK_DENIED, MESSAGE_CALLBACK_TENANT_DENIED).with_detail("id", id).with_detail("tenant", tenant)
}

/// 宿主说回调的会话令牌无效。
pub(crate) fn session_invalid() -> Error {
    Error::denied(ERROR_CODE_CALLBACK_DENIED, MESSAGE_SESSION_INVALID)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    /// 运行时产生的每个键的错误码与参数（即 `details` 的成员，不另加）同向量 `plugin/plugin-messages.json`。
    #[test]
    fn errors_follow_the_vectors() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/spec/conformance/vectors/plugin/plugin-messages.json");
        let text = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("cannot read {path}: {error}; run `git submodule update --init`"));
        let vectors: Value = serde_json::from_str(&text).unwrap();
        let errors = [
            unavailable("xihan.acme.echo", LifecycleState::Configured),
            failed("xihan.acme.echo", PHASE_CONFIGURE, REASON_NOT_AN_OBJECT),
            unsubscribed("xihan.acme.echo", "t", "x", "competing"),
            host_unavailable("xihan.acme.echo", REASON_NO_ENDPOINT),
            denied("xihan.acme.echo", "capability:kernel.pipeline"),
            tenant_denied("xihan.acme.echo", "t1"),
            session_invalid(),
        ];
        let messages = vectors["messages"].as_array().unwrap();
        assert_eq!(messages.len(), errors.len());
        for (message, error) in messages.iter().zip(&errors) {
            assert_eq!((error.code(), error.message_key()), (message["code"].as_str().unwrap(), message["key"].as_str()), "{message}");
            let mut arguments: Vec<&str> = message["arguments"].as_array().unwrap().iter().map(|argument| argument.as_str().unwrap()).collect();
            arguments.sort_unstable();
            let names: Vec<&str> = error.details().map(|details| details.keys().map(String::as_str).collect()).unwrap_or_default();
            assert_eq!(names, arguments, "{message}");
            assert!(error.details().into_iter().flatten().all(|(_, value)| value.is_string()), "the arguments are text: {message}");
            // en-US 并进了 SDK 自带的模板：没有设置解析器时也能渲染出向量的模板
            assert!(!error.message().contains(message["key"].as_str().unwrap()), "{}", error.message());
        }
        // 固定的 reason 文本
        let reasons: Vec<&str> = vectors["reasons"].as_array().unwrap().iter().map(|reason| reason["reason"].as_str().unwrap()).collect();
        for reason in [REASON_NOT_AN_OBJECT, REASON_NO_ENDPOINT, REASON_NO_ERROR] {
            assert!(reasons.contains(&reason), "{reason}");
        }
        assert_eq!(unavailable("p", LifecycleState::Configured).message(), "Plugin 'p' is not available (state: Configured).");
    }
}
