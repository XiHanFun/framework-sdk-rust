//! XiHan Framework SDK for Rust —— 占位版本。
//!
//! 本 crate 目前只占住名字，尚无实现。规范见 [framework-spec](https://github.com/XiHanFun/framework-spec)，
//! 参考实现是 .NET（[XiHan.Framework](https://github.com/XiHanFun/XiHan.Framework)）与 Go
//! （[framework-sdk-go](https://github.com/XiHanFun/framework-sdk-go)）。计划内容、包划分与参与方式见仓库 README。

#![forbid(unsafe_code)]
#![no_std]

/// 本 crate 的版本。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 本 SDK 实现的原语层大版本（RFC-0001）；占位版本尚未实现任何原语。
pub const SPEC_VERSION: &str = "1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_exposes_versions() {
        assert_eq!(VERSION, "0.0.1");
        assert_eq!(SPEC_VERSION, "1");
    }
}
