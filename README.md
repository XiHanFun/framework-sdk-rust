# XiHan Framework SDK for Rust

[XiHan Framework Spec](https://github.com/XiHanFun/framework-spec) 的 Rust 实现：原语层、清单、调用上下文、管道与操作契约。语义与线上编码和 .NET 参考实现（[XiHan.Framework](https://github.com/XiHanFun/XiHan.Framework)）及 Go SDK（[framework-sdk-go](https://github.com/XiHanFun/framework-sdk-go)）一致。规范以 submodule 放在 `spec/`，一致性由规范仓库的向量验证：测试直接读取 `spec/conformance/vectors/`，不复制向量文件。

```toml
[dependencies]
xihan-framework = "0.1"
```

| 模块 | 内容 | 规范 |
| --- | --- | --- |
| `primitives` | `Error` / 错误码、`Result<T>`（标准库 `Result<T, Error>`）与线上编码 `WireResult`、`Maybe<T>`（`Option<T>`）、`TypedId<Tag, V>`、`LifecycleState`、`PipelineStage`、`Stability`、Baggage 键与值约束、消息键与模板、`Catalog` 与进程默认解析器、`FrameworkError` | RFC-0001、RFC-0010、`conventions/` |
| `manifest` | `xihan.manifest.json` 的模型、读取、规范化输出与校验，能力声明与依赖文本；读取按 Schema 逐字执行：成员名区分大小写，可选成员写成 `null` 不合法，只有 `requires[].version` 可以带 `^`，`contributes.operations` 按操作 ID 文法 | RFC-0002、`manifest/manifest.schema.json` |
| `invocation` | `Context`：可等待、父子传播的 `CancellationToken`，后台定时器触发的截止时间，W3C 追踪（上游的 trace-flags 原样保留，采样只看最低位；没有上游时写出 `01`）、已登记的 Baggage（构建与派生时丢弃已登记键里不合值约束的成员：空值、按 UTF-16 码元超过 256、含控制字符；幂等键除外）、按类型存取的对象袋；超时与取消只按截止令牌归因为 `KERNEL_TIMEOUT_001` / `KERNEL_CANCELLED_001` | RFC-0003 |
| `pipeline` | 十一阶段的 `Builder`：`use_middleware` / `use_at` / `use_named_instance`、`before` / `after`（按名字登记的实例用实例名，其余用中间件名；约束的主体在声明时必须已登记）、`describe` / `dump` / `build`；排序规则与错误码 `KERNEL_PIPELINE_00x`；中间件与终点都是异步的 | RFC-0004 |
| `operation` | `Contract`、`Schema`、异步 `Handler<Req, Res>`、容量 10000 的 `Registry`、经完整管道执行的 `Invoker`（XGI：JSON 请求与响应；另有不经 JSON 的类型化入口）；内核错误的码、键、参数与 en-US 模板同向量 `kernel-messages.json`，载荷不合法的 `reason` 是 JSON 路径，中间件以成功短路而没有响应时报 `KERNEL_UNEXPECTED_001` + `kernel.operation.no_response`，中间件或处理器 panic 时报 `KERNEL_UNEXPECTED_001` + `kernel.invocation.unhandled_exception`（`panic = "unwind"` 时） | RFC-0011 |

## 用法

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
    let builder = Builder::new().use_at(observe, PipelineStage::Observe, Placement::End, APP_OWNER);
    print!("{}", builder.dump()?); // observe      Observe

    let invoker = Invoker::new(registry, &builder)?;
    let ctx = Context::builder().timeout(Duration::from_secs(2)).baggage(BAGGAGE_TENANT, "t1").build();
    let payload = invoker.invoke(&ctx, Request::new("orders.create", r#"{"quantity":2}"#)).await?;
    assert_eq!(payload, br#"{"quantity":2,"tenant":"t1"}"#);
    Ok(())
}
```

1. 中间件、处理器与管道终点返回 `BoxFuture`（`Pin<Box<dyn Future + Send>>`），写 `Box::pin(async move { … })`；本 crate 不依赖任何异步运行时，future 交给 tokio、`futures::executor::block_on` 或任何执行器。`Invoker::invoke` 与 `invoke_typed` 的签名写明返回 `Send` 的 future，`invoke_owned` 返回 `'static` 的 future，可以直接 `tokio::spawn`。
2. 可恢复失败返回 `Err(Error)`，错误码按 `<领域大写>_<KIND>_<NNN>`，文案用消息键 + 参数（`Error::validation(code, key).with_detail(…)`）；`Error::message` 先按 `primitives::set_default_messages` 设置的解析器解析，再按 SDK 自带的 en-US 模板（`primitives::builtin_catalog()`，即 SDK 与宿主共用的内核错误，逐字同向量 `kernel-messages.json`），都没有时是「键 (参数)」。配置期有登记键的错误（如操作重复登记）以 `FrameworkError::with_message` 带上键与参数，`to_error()` 一并转出。
3. 编程错误（不合文法的错误码、消息键或 Baggage 键）panic，各函数的文档写明 panic 条件；来自不可信输入的值先用 `is_error_code`、`is_baggage_key_well_formed` 判断，或用 `Error::try_new`。清单校验、管道排序、操作登记等配置期失败返回 `FrameworkError`。
4. `Context` 的截止时间由进程里一个后台线程（`xihan-deadline-timer`，第一次登记截止时间时启动）按时触发；`Context::done()` 在调用方取消或到达截止时间时完成，`Context::cancellation()` 给出 `KERNEL_TIMEOUT_001`（消息键 `kernel.invocation.deadline_exceeded`；构建上下文时截止时间已经过去的是 `kernel.invocation.deadline_before_start`）或 `KERNEL_CANCELLED_001`（`kernel.invocation.cancelled`）。

线上编码与 .NET 参考实现一致：

```rust
use xihan_framework::primitives::{Error, WireResult};

let ok = serde_json::to_string(&WireResult(Ok::<_, Error>(7))).unwrap();
assert_eq!(ok, r#"{"ok":true,"value":7}"#);
let failed = serde_json::to_string(&WireResult(Err::<u32, _>(Error::literal("ORDERS_CONFLICT_001", "stale")))).unwrap();
assert_eq!(failed, r#"{"ok":false,"error":{"code":"ORDERS_CONFLICT_001","message":"stale"}}"#);
```

读写按 RFC-0001「Result」与向量 `wire-encoding.json`：不带值的成功 `Result<()>` 只写 `{"ok":true}`，读入时带了 `value` 即拒绝；有值成功的 `value` 必填，只有值类型是 `Maybe<T>`（`Option<T>`）时可以是 `null`；`Error` 的 `code` 与非空的 `message` 必填，可选成员写成 `null` 不合法；Result 与 Error 只收 JSON 对象（serde 派生的数组形式也拒绝），未知成员忽略。`Maybe<T>` 有值为值本身、无值为 `null`；`TypedId` 编码为底层值。

消息模板按 RFC-0010「模板」与向量 `message-templates.json`：`{name}` / `{0}`，`{{`、`}}` 转义；值的文本形式是字符串原样、布尔 `true` / `false`、`null` 为空串、浮点写最短往返的十进制（`2.0` 写 `2`）；可移植的格式说明符 `D`、`X` / `x`、`F`、`N`（`{n:D5}`、`{n:x4}`、`{n:F2}`、`{n:N2}`），`F` 与 `N` 正好一半时取偶，按不变文化格式化；其余说明符（如 `yyyy-MM-dd`）留给宿主，SDK 按无格式输出。插件要在本地格式化的目录只用可移植的这组。清单用 `manifest::read` 读入、`manifest::write` 写出规范化文本（两空格缩进，字段按 Schema 顺序，缺省值省略）。

## 依赖

| crate | 用途 |
| --- | --- |
| `serde`（`derive`）、`serde_json` | `Result` / `Error` / `Maybe` 的线上编码、XGI 载荷编解码、清单读写；`serde_json::Value` 是错误细节与模板参数的值类型 |
| `getrandom` | 追踪 ID 与 Span ID 的随机数 |
| `serde_path_to_error` | 载荷不合法时给出出错位置的 JSON 路径（`$.lines[1].quantity`）：RFC-0011 规则 2 要求 `reason` 只写路径，`serde_json` 本身只报行列 |

规范里的文法（错误码、消息键、Baggage 键、清单各字段、`traceparent` 等）是手写的逐字节判定，不依赖正则库；取消令牌与截止时间是自带的轻量实现，不依赖异步运行时。没有开发依赖：测试用自写的最小执行器运行 future。

## 开发

```bash
git submodule update --init
```

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

```bash
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

最低 Rust 版本 1.85（edition 2024 的要求），clippy 的 `incompatible_msrv` 按它核对标准库 API。`spec/` 与 `tests/` 不进 crate 包。

## 状态

`0.1`：原语、清单、调用上下文、管道与操作可用，通过规范仓库 `conformance/vectors/primitives/` 的全部向量（尚未发布到 crates.io，那里是名字占位的 0.0.1）。规划中：

- 进程外插件运行时 `plugin`（`xihan.plugin.v1`，基于 tonic，以 cargo feature 启用；unix 套接字 / Windows 命名管道 / tcp 回环），用规范仓库的黑盒运行器 `xihan-conformance plugin --dir <产物目录>` 验证；
- Wasm 插件 ABI（RFC-0018，`xihan_*` 导出、`xihan` 导入，WASI preview 1 reactor），样板在 XiHan.Framework 的 `samples/EchoWasmPlugin`；
- 远程 Provider 的操作集（RFC-0019，首个是 `caching.distributed@1`）与示例插件。

拆成多个 crate 时用 `xihan-framework-<模块>` 前缀；领域词与规范术语表（`conventions/vocabulary.md`）一致，只换本语言的大小写风格。公开契约的变更不在 SDK 仓库里做：走 framework-spec 的 RFC 流程，再同步到各 SDK。

## 许可

[MIT](LICENSE)
