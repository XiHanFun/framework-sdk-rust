//! XiHan Framework SDK for Rust：[XiHan Framework Spec](https://github.com/XiHanFun/framework-spec) 的 Rust 实现。
//!
//! 本 crate 提供原语层、清单、调用上下文、管道与操作契约，语义与线上编码按规范的 RFC，由规范仓库的一致性向量验证；
//! .NET 参考实现是 [XiHan.Framework](https://github.com/XiHanFun/XiHan.Framework)。
//!
//! 中间件、处理器与调用器都是异步的，返回的 future 与运行时无关，可以在任何执行器里运行：中间件、管道终点与处理器返回装箱的
//! [`BoxFuture`]（它们要能作为 `dyn` 使用），调用器的 [`operation::Invoker::invoke`] 等返回 `impl Future + Send`。
//! 打开 cargo feature `plugin` 时另有进程外插件运行时 `plugin`（RFC-0017），它基于 tonic 与 tokio。

#![forbid(unsafe_code)]

use std::future::Future;
use std::pin::Pin;

mod grammar;
pub mod invocation;
pub mod manifest;
pub mod operation;
pub mod pipeline;
#[cfg(feature = "plugin")]
pub mod plugin;
pub mod primitives;

/// 本 crate 的版本，同 [`primitives::SDK_VERSION`]。
pub const VERSION: &str = primitives::SDK_VERSION;

/// 本 SDK 实现的原语层大版本（RFC-0001），同 [`primitives::SPEC_VERSION`]。
pub const SPEC_VERSION: &str = primitives::SPEC_VERSION;

/// 装箱的、可跨线程发送的 future：中间件、管道步骤与操作处理器的返回类型。
///
/// 用装箱的 trait 对象而不是关联类型，让 [`pipeline::Middleware`]、[`pipeline::Step`] 与 [`operation::Handler`] 可以作为 `dyn` 使用；
/// 实现时写 `Box::pin(async move { … })`。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// README 里的 Rust 示例作为文档测试编译运行，保证示例与 API 一致。
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

/// 中文版 README 里的 Rust 示例同样作为文档测试编译运行，保证两份 README 的示例都与 API 一致。
#[cfg(doctest)]
#[doc = include_str!("../README_cn.md")]
struct ReadmeCnDoctests;

/// `with_*` 消耗原值、返回改过的值，返回值丢掉就等于什么都没做：这些方法标了 `#[must_use]`，丢掉返回值在 `deny(unused_must_use)`
/// 下编译不过。
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// let error = xihan_framework::primitives::Error::literal("ORDERS_CONFLICT_001", "The order changed.");
/// error.with_detail("id", "o1");
/// ```
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// let contract = xihan_framework::operation::Contract::new("orders.create");
/// contract.with_expose(xihan_framework::operation::EXPOSE_ALL);
/// ```
///
/// 用上返回值就能编译：
///
/// ```
/// #![deny(unused_must_use)]
/// let error = xihan_framework::primitives::Error::literal("ORDERS_CONFLICT_001", "The order changed.").with_detail("id", "o1");
/// let contract = xihan_framework::operation::Contract::new("orders.create").with_expose(xihan_framework::operation::EXPOSE_ALL);
/// assert_eq!((error.details().map(|details| details.len()), contract.expose.len()), (Some(1), 5));
/// ```
#[cfg(doctest)]
struct MustUseDoctests;

/// 插件运行时的 `with_*` 同样标了 `#[must_use]`。
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// xihan_framework::plugin::Subscription::new("orders.events").with_message_type("orders.created");
/// ```
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// xihan_framework::plugin::Envelope::new("orders.events", "e1", "urn:orders", "orders.created").with_subject("o1");
/// ```
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// xihan_framework::plugin::Plugin::new("{}").with_network(xihan_framework::plugin::Network::Tcp);
/// ```
///
/// ```
/// #![deny(unused_must_use)]
/// use xihan_framework::plugin::{Envelope, Network, Plugin, Subscription};
///
/// let subscription = Subscription::new("orders.events").with_message_type("orders.created");
/// let envelope = Envelope::new("orders.events", "e1", "urn:orders", "orders.created").with_subject("o1");
/// let _plugin = Plugin::new("{}").with_network(Network::Tcp);
/// assert_eq!((subscription.message_type.as_deref(), envelope.subject.as_deref()), (Some("orders.created"), Some("o1")));
/// ```
#[cfg(all(doctest, feature = "plugin"))]
struct PluginMustUseDoctests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_versions() {
        assert_eq!(VERSION, "0.2.0-rc.1");
        assert_eq!(SPEC_VERSION, "1");
    }
}
