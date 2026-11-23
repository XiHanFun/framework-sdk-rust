<div align="center">

<h1>XiHan Framework SDK for Rust</h1>

<h3>与 .NET 宿主相同的原语、清单与管道，由同一套一致性向量验证</h3>

<p>
  <a href="https://github.com/XiHanFun/framework-sdk-rust/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/XiHanFun/framework-sdk-rust/ci.yml?branch=main&style=flat-square&label=CI" /></a>
  <img alt="Rust 1.85+" src="https://img.shields.io/badge/Rust-1.85%2B-000000?style=flat-square&logo=rust&logoColor=white" />
  <a href="./LICENSE"><img alt="MIT License" src="https://img.shields.io/github/license/XiHanFun/framework-sdk-rust?style=flat-square&color=green" /></a>
  <a href="https://github.com/XiHanFun/framework-sdk-rust/stargazers"><img alt="GitHub Stars" src="https://img.shields.io/github/stars/XiHanFun/framework-sdk-rust?style=flat-square&logo=github&label=Stars&color=1f6feb" /></a>
</p>

<p>
  <a href="https://framework.docs.xihanfun.com/sdks/rust"><b>文档</b></a> ·
  <a href="#快速开始"><b>快速开始</b></a> ·
  <a href="https://github.com/XiHanFun/framework-spec"><b>规范</b></a> ·
  <a href="https://github.com/XiHanFun/XiHan.Framework"><b>参考实现</b></a>
</p>

<p><a href="./README.md">English</a> | <b>简体中文</b></p>

</div>

XiHan Framework SDK for Rust 是 [XiHan Spec](https://github.com/XiHanFun/framework-spec) 的 Rust 实现，提供原语层、清单、调用上下文、管道与操作契约，以及进程外插件运行时（cargo feature `plugin`），语义与线上编码按规范的 RFC；.NET 参考实现是 [XiHan.Framework](https://github.com/XiHanFun/XiHan.Framework)。规范以 submodule 放在 `spec/`，一致性由规范仓库的向量验证。

## 模块

整个 SDK 是一个 crate `xihan-framework`；领域词与规范术语表一致，只换本语言的大小写风格。

| 模块 | 内容 | 规范 |
| --- | --- | --- |
| `primitives` | `Error` 与错误码、`Result<T>` 与线上编码、`Maybe<T>`、`TypedId`、Baggage、消息键与模板、共同的 JSON 读取检查 | RFC-0001、RFC-0010 |
| `manifest` | `xihan.manifest.json` 的读取、按规范排版的写出与校验 | RFC-0002 |
| `invocation` | `Context`：取消、截止时间、W3C 追踪、已登记的 Baggage | RFC-0003 |
| `pipeline` | 十一阶段的 `Builder` 与排序规则 | RFC-0004 |
| `operation` | `Contract`、`Handler`、`Registry` 与经完整管道执行的 `Invoker` | RFC-0011 |
| `plugin`（feature `plugin`） | 基于 tonic 的进程外插件运行时：握手、生命周期、调用、事件与宿主回调 | RFC-0017 |

不打开 feature `plugin` 时本 crate 不依赖任何异步运行时。

## 安装

```bash
cargo add xihan-framework@<版本>
```

插件运行时在 cargo feature `plugin` 下（`--features plugin`）。目前只有预发布版，要写明版本号：`<版本>` 换成 [Releases](https://github.com/XiHanFun/framework-sdk-rust/releases) 里的版本（不带 `v`）；不写版本会装到 crates.io 上占名用的旧版本。

## 快速开始

```rust
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use xihan_framework::invocation::Context;
use xihan_framework::operation::{self, Contract, EXPOSE_HTTP, Invoker, Registry, Request};
use xihan_framework::pipeline::{self, APP_OWNER, Builder, Placement};
use xihan_framework::primitives::{BAGGAGE_TENANT, Error, PipelineStage, Stability};

#[derive(Clone, Deserialize)]
struct CreateOrder {
    quantity: u32,
}

#[derive(Serialize)]
struct OrderCreated {
    quantity: u32,
    tenant: Option<String>,
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let registry = Arc::new(Registry::new());
    registry.add(
        Contract::new("orders.create").with_stability(Stability::Experimental).with_expose([EXPOSE_HTTP]),
        operation::handler_fn(|ctx: &Context, request: CreateOrder| {
            Box::pin(async move {
                if request.quantity == 0 {
                    return Err(Error::validation("ORDERS_VALIDATION_001", "orders.quantity.invalid").with_detail("quantity", 0));
                }
                Ok(OrderCreated { quantity: request.quantity, tenant: ctx.tenant().map(str::to_owned) })
            })
        }),
    )?;

    let observe = pipeline::from_fn("Observe", |ctx, next| {
        Box::pin(async move {
            let outcome = next.run(ctx).await;
            if let Err(error) = &outcome {
                eprintln!("{} failed: {error}", ctx.trace_id());
            }
            outcome
        })
    });
    let mut builder = Builder::new();
    builder.use_at(observe, PipelineStage::Observe, Placement::End, APP_OWNER)?;
    print!("{}", builder.dump()?); // observe      Observe

    let invoker = Invoker::new(registry, &builder)?;
    let ctx = Context::builder().timeout(Duration::from_secs(2)).baggage(BAGGAGE_TENANT, "t1").build();
    let payload = invoker.invoke(&ctx, Request::new("orders.create", r#"{"quantity":2}"#)).await?;
    assert_eq!(payload, br#"{"quantity":2,"tenant":"t1"}"#);
    Ok(())
}
```

中间件、处理器与管道终点返回 `BoxFuture`，future 交给 tokio 或任何执行器。写一个插件、这个示例背后的规则与和宿主共用的行为见文档。

## 文档

| 去处 | 内容 |
| --- | --- |
| [Rust SDK](https://framework.docs.xihanfun.com/sdks/rust) | 模块详解、快速开始、写一个插件、与宿主共用的行为、语言映射、设计取舍、一致性、破坏性改动与迁移 |
| [进程外插件](https://framework.docs.xihanfun.com/plugins/out-of-process) | 插件协议与宿主一侧 |
| [规范与多语言](https://framework.docs.xihanfun.com/appendix/spec) | 规范的组织、一致性档与各语言 SDK |
| [XiHan Spec](https://github.com/XiHanFun/framework-spec) | RFC、术语表、协议映射与一致性向量 |

## 状态

成熟度是 Experimental：已有钉在规范 rc 标签上的预发布版，到 Stable 还要钉在规范正式版标签上的发布版；尚未提供的部分见文档。

- 钉的规范版本：`spec/` 子模块指向的规范标签。
- 认领的档（RFC-0024）：`core` 与 `plugin`。
- 缺口：无。
- 范围外：流式操作、Wasm 插件与远程 Provider 尚未提供，它们不在认领的档里。

## 生态

| 项目 | 说明 |
| --- | --- |
| [framework-spec](https://github.com/XiHanFun/framework-spec) | XiHan Spec：RFC、术语表、协议映射与一致性向量 |
| [XiHan.Framework](https://github.com/XiHanFun/XiHan.Framework) | .NET 参考实现 |
| [framework-sdk-go](https://github.com/XiHanFun/framework-sdk-go) | Go SDK |
| [framework-sdk-typescript](https://github.com/XiHanFun/framework-sdk-typescript) | TypeScript SDK |
| [framework-sdk-java](https://github.com/XiHanFun/framework-sdk-java) | Java SDK |
| [framework-sdk-python](https://github.com/XiHanFun/framework-sdk-python) | Python SDK |
| [framework-sdk-php](https://github.com/XiHanFun/framework-sdk-php) | PHP SDK |

## 参与

欢迎提交 [Issue](https://github.com/XiHanFun/framework-sdk-rust/issues) 与 Pull Request。公开契约的变更不在 SDK 仓库里做：先走 [framework-spec](https://github.com/XiHanFun/framework-spec) 的 RFC，再同步到各 SDK。开发约定、门禁与发布流程见 [AGENTS.md](./AGENTS.md)。

## 许可证

[MIT](./LICENSE) © 2021-Present XiHanFun and contributors。
