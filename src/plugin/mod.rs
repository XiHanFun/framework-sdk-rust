//! 进程外插件运行时（RFC-0017，规范性文本 `spec/plugin/`）：让一个 Rust 程序成为宿主启动的插件进程。
//!
//! 运行时读取宿主下发的环境变量（`handshake.md` §1），在监听之前核对清单与声明，监听本机端点并向 stdout 写握手行（§2、§4），以 gRPC
//! 服务端提供 `xihan.plugin.v1` 的 `Plugin` 服务：握手、配置、启停、健康与健康推送、操作调用与事件投递；每个 RPC 先按常数时间核对
//! 会话令牌，生命周期按 `lifecycle.md` §7 的次序表、配置、就绪、健康、停止与收尾执行。插件里的操作就是
//! [`operation::Registry`](crate::operation::Registry) 里登记的操作，`Invoke` 收到的调用经 [`operation::Invoker`](crate::operation::Invoker)
//! 与完整管道执行，调用上下文按 `invocation.md` §2 从 `InvocationMetadata` 还原。业务失败放进 `Status.error`（RFC-0001 编码，带消息键的
//! `text` 留空），gRPC 状态只表示协议层的失败。处理器经 [`host`] 取得宿主回调的客户端 [`Host`]（`Host.Invoke`、`Host.Publish`）。
//! stdin 关闭或收到 SIGTERM 即收尾，最多 3 秒后退出。流式调用不实现（流式操作契约的 RFC 之前，`invocation.md` §1）。
//!
//! 只在 cargo feature `plugin` 打开时编译；运行在 tokio 运行时里（单线程或多线程都可以）。
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use serde::{Deserialize, Serialize};
//! use xihan_framework::operation::{self, Contract, Registry};
//! use xihan_framework::plugin::{MANIFEST_FILE_NAME, Plugin};
//!
//! #[derive(Deserialize)]
//! struct Ping {
//!     text: String,
//! }
//!
//! #[derive(Serialize)]
//! struct Pong {
//!     text: String,
//! }
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let registry = Arc::new(Registry::new());
//!     registry.add(
//!         Contract::new("pings.create"),
//!         operation::handler_fn(|_ctx, ping: Ping| Box::pin(async move { Ok(Pong { text: ping.text }) })),
//!     )?;
//!     // 握手交给宿主的清单必须与插件目录里的文件一致：工作目录就是插件目录，直接读它
//!     Plugin::from_manifest_file(MANIFEST_FILE_NAME)?.with_operations(registry).serve().await?;
//!     Ok(())
//! }
//! ```

mod catalog;
mod convert;
mod endpoint;
mod host;
mod log;
mod messages;
mod runtime;
mod service;
mod session;
mod time;
mod types;

/// 协议 `xihan.plugin.v1` 的消息与服务（`spec/plugin/plugin.proto` 的生成代码，由 `tools/protogen` 生成，不要手改）。
///
/// 写插件用不到它：[`Plugin`] 与 [`Host`] 已经包装了全部 RPC。扮演宿主的测试或工具可以用其中的客户端与服务端。
#[allow(missing_docs, clippy::all, clippy::pedantic, rustdoc::all)]
pub mod v1 {
    include!("generated/xihan.plugin.v1.rs");
}

pub use catalog::{LOCALES, MessageCatalog, runtime_catalog};
pub use endpoint::{Endpoint, Network, is_whitespace, parse_handshake_line, parse_host_addresses, select_host_endpoint};
pub use host::{Host, host};
pub use log::{Level, Logger};
pub(crate) use messages::BUILTIN_TEMPLATES;
pub use messages::{
    ERROR_CODE_CALLBACK_DENIED, ERROR_CODE_UNAVAILABLE, ERROR_CODE_UNEXPECTED, MESSAGE_CALLBACK_DENIED, MESSAGE_CALLBACK_TENANT_DENIED, MESSAGE_FAILED,
    MESSAGE_HOST_UNAVAILABLE, MESSAGE_SESSION_INVALID, MESSAGE_UNAVAILABLE, MESSAGE_UNSUBSCRIBED,
};
pub use runtime::{Launch, Plugin, Running};
pub use types::{
    Binding, Configuration, Envelope, Event, EventHandler, EventHandlerFn, Health, HealthItem, HealthStatus, Hooks, Subscription, SubscriptionMode,
    event_handler_fn,
};

/// 本运行时支持的协议版本（`handshake.md` §3 第 1 条），升序。
pub const PROTOCOL_VERSIONS: &[u32] = &[1];

/// 握手行的格式版本（`handshake.md` §2）：只在行格式变化时递增，与协议版本无关。
pub const HANDSHAKE_LINE_VERSION: &str = "xihan-plugin/1";

/// 携带会话令牌的元数据键：宿主调用插件、插件回调宿主的每个 RPC 都带它（`handshake.md` §4）。
pub const SESSION_METADATA_KEY: &str = "xihan-plugin-session";

/// 插件目录里的清单文件名（`handshake.md` §1）。
pub const MANIFEST_FILE_NAME: &str = "xihan.manifest.json";

/// 握手时上报的实现语言（`SdkInfo.language`）。
pub const SDK_LANGUAGE: &str = "rust";

/// 回调 `Host.Invoke` 需要的权限（`invocation.md` §5）。
pub const PERMISSION_PIPELINE: &str = "capability:kernel.pipeline";

/// 回调 `Host.Publish` 需要的权限（`invocation.md` §5）。
pub const PERMISSION_MESSAGING: &str = "capability:messaging.queue";

/// 信封里携带租户的扩展属性名（RFC-0009）：宿主对 `Host.Publish` 逐项按它判定租户，插件不在本地核对（`invocation.md` §5）。
pub const TENANT_EXTENSION: &str = "xihantenant";

/// 宿主支持的协议版本，逗号分隔、升序（`handshake.md` §1）。
pub const ENV_PROTOCOL_VERSIONS: &str = "XIHAN_PLUGIN_PROTOCOL_VERSIONS";
/// 宿主的原语层大版本。
pub const ENV_PRIMITIVES: &str = "XIHAN_PLUGIN_PRIMITIVES";
/// 本次启动的会话令牌。
pub const ENV_SESSION: &str = "XIHAN_PLUGIN_SESSION";
/// 宿主回调端点：一个或多个 `<network> <address>`，按宿主的偏好排序、以 `;` 分隔（`handshake.md` §4）。
pub const ENV_HOST_ADDRESS: &str = "XIHAN_PLUGIN_HOST_ADDRESS";
/// 实例 ID。
pub const ENV_INSTANCE: &str = "XIHAN_PLUGIN_INSTANCE";
/// 宿主默认区域（BCP 47）。
pub const ENV_LOCALE: &str = "XIHAN_PLUGIN_LOCALE";

/// 协议版本协商（`handshake.md` §3 第 1 条，向量 `plugin/protocol-versions.json`）：取宿主列表里最高的、插件也支持的版本；
/// 没有共同版本时为 `None`。
pub fn negotiate(host: &[u32], plugin: &[u32]) -> Option<u32> {
    host.iter().copied().filter(|version| plugin.contains(version)).max()
}
