//! XiHan Framework SDK for Rust：[XiHan Framework Spec](https://github.com/XiHanFun/framework-spec) 的 Rust 实现。
//!
//! 本 crate 提供原语层、清单、调用上下文、管道与操作契约，语义与线上编码和 .NET 参考实现
//! （[XiHan.Framework](https://github.com/XiHanFun/XiHan.Framework)）及 Go SDK
//! （[framework-sdk-go](https://github.com/XiHanFun/framework-sdk-go)）一致，由规范仓库的一致性向量验证。
//!
//! 中间件、处理器与调用器都是异步的，返回与运行时无关的 future（[`BoxFuture`]），可以在任何执行器里运行。

#![forbid(unsafe_code)]

use std::future::Future;
use std::pin::Pin;

mod grammar;
pub mod invocation;
pub mod manifest;
pub mod primitives;

/// 本 crate 的版本，同 [`primitives::SDK_VERSION`]。
pub const VERSION: &str = primitives::SDK_VERSION;

/// 本 SDK 实现的原语层大版本（RFC-0001），同 [`primitives::SPEC_VERSION`]。
pub const SPEC_VERSION: &str = primitives::SPEC_VERSION;

/// 装箱的、可跨线程发送的 future：中间件、管道步骤与操作处理器的返回类型。
///
/// 用装箱的 trait 对象而不是关联类型，让中间件、管道步骤与处理器的 trait 可以作为 `dyn` 使用；
/// 实现时写 `Box::pin(async move { … })`。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_versions() {
        assert_eq!(VERSION, "0.1.0");
        assert_eq!(SPEC_VERSION, "1");
    }
}
