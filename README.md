# XiHan Framework SDK for Rust

[XiHan Framework Spec](https://github.com/XiHanFun/framework-spec) 的 Rust 实现：原语层、清单、调用上下文、管道与操作契约。语义与线上编码和 .NET 参考实现（[XiHan.Framework](https://github.com/XiHanFun/XiHan.Framework)）及 Go SDK（[framework-sdk-go](https://github.com/XiHanFun/framework-sdk-go)）一致。规范以 submodule 放在 `spec/`，一致性由规范仓库的向量验证：测试直接读取 `spec/conformance/vectors/`，不复制向量文件。

```toml
[dependencies]
xihan-framework = "0.1"
```

| 模块 | 内容 | 规范 |
| --- | --- | --- |
| `primitives` | `Error` / 错误码、`Result<T>`（标准库 `Result<T, Error>`）与线上编码 `WireResult`、`Maybe<T>`（`Option<T>`）、`TypedId<Tag, V>`、`LifecycleState`、`PipelineStage`、`Stability`、Baggage 键与值约束、消息键与模板（可按区域取数字符号）、`Catalog` 与进程默认解析器、`FrameworkError`；`primitives::json`：JSON 文本的共同读取检查（向量 `primitives/json-text.json`）、按值模型读入（`json::from_slice`）与写出（`json::Model`、`json::to_value`） | RFC-0001、RFC-0010、`conventions/` |
| `manifest` | `xihan.manifest.json` 的模型、读取、按规范排版的写出与校验，能力声明与依赖文本（版本不带前导零、不超过 2147483647）；读取按 RFC-0002 解析规则 2 的顺序（向量 `manifest/manifests.json`）：JSON 文本 → `null` → 按 Schema 逐项核对 → 跨字段约束，错误都是 `KERNEL_CAPABILITY_006`，带消息键与路径或清单 ID | RFC-0002、`manifest/manifest.schema.json` |
| `invocation` | `Context`：可等待、父子传播的 `CancellationToken`，后台定时器触发的截止时间，W3C 追踪（`traceparent` 只认版本 `00` 与小写十六进制，不合法的当作没有给出；采样只看最低位，写出 `00` 或 `01`，没有上游时写 `01`）、已登记的 Baggage（已登记键的值非空、按 UTF-16 码元最多 256、不含控制字符，幂等键除外：构建上下文时按外部输入丢弃不合的成员、保留原值，`with_baggage` 给出不合的值即 panic）、按类型存取的对象袋；超时与取消只按截止令牌归因为 `KERNEL_TIMEOUT_001` / `KERNEL_CANCELLED_001` | RFC-0003 |
| `pipeline` | 十一阶段的 `Builder`：`use_middleware` / `use_at` / `use_named_instance`、`before` / `after`（按名字登记的实例用实例名，其余用中间件名；约束的主体在声明时必须已登记）、`describe` / `dump` / `build`；排序规则与错误码 `KERNEL_PIPELINE_00x`（向量 `pipeline/ordering.json`）：登记方法以 `&mut self` 返回 `Result`，重复登记、约束主体未登记在登记时即报，约束目标未登记、跨阶段与成环在排序时报；中间件与终点都是异步的 | RFC-0004 |
| `operation` | `Contract`、`Schema`、异步 `Handler<Req, Res>`、容量 10000 的 `Registry`、经完整管道执行的 `Invoker`（XGI：JSON 请求与响应；另有不经 JSON 的类型化入口）；管道里带三个内置步骤，位置与 .NET 的内置中间件相同；内核错误的码、键、参数与 en-US 模板同向量 `kernel-messages.json`（`type_mismatch` 只带 `id`）；载荷先过共同的 JSON 读取检查，按文本顺序报第一处问题，`reason` 是 JSON 路径（向量 `operations/payload-reasons.json`）；中间件以成功短路而没有响应时报 `KERNEL_UNEXPECTED_001` + `kernel.operation.no_response` | RFC-0011 |

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

1. 中间件、处理器与管道终点返回 `BoxFuture`（`Pin<Box<dyn Future + Send>>`），写 `Box::pin(async move { … })`；本 crate 不依赖任何异步运行时，future 交给 tokio、`futures::executor::block_on` 或任何执行器。`Invoker::invoke` 与 `invoke_typed` 的签名写明返回 `Send` 的 future，`invoke_owned` 返回 `'static` 的 future，可以直接 `tokio::spawn`。
2. 可恢复失败返回 `Err(Error)`，错误码按 `<领域大写>_<KIND>_<NNN>`，文案用消息键 + 参数（`Error::validation(code, key).with_detail(…)`）；`Error::message` 先按 `primitives::set_default_messages` 设置的解析器解析，再按 SDK 自带的 en-US 模板（`primitives::builtin_catalog()`，即 SDK 与宿主共用的内核错误，逐字同向量 `kernel-messages.json`），都没有时是「键 (参数)」。配置期有登记键的错误（如操作重复登记）以 `FrameworkError::with_message` 带上键与参数，`to_error()` 一并转出。
3. 编程错误（不合文法的错误码、消息键或 Baggage 键）panic，各函数的文档写明 panic 条件；panic 的内容是 `FrameworkError` 本身（`std::panic::panic_any`），不是字符串，接住后用 `downcast_ref::<FrameworkError>()` 取出；标准库缺省的 panic hook 对它只打印 `Box<dyn Any>`，要看到码与说明就装自己的 hook。来自不可信输入的值先用 `is_error_code`、`is_baggage_key_well_formed` 判断，或用 `Error::try_new`。清单校验、管道排序、操作登记等配置期失败返回 `FrameworkError`。
4. `Context` 的截止时间由进程里一个后台线程（`xihan-deadline-timer`，第一次登记截止时间时启动）按时触发；`Context::done()` 在调用方取消或到达截止时间时完成，`Context::cancellation()` 给出 `KERNEL_TIMEOUT_001`（`kernel.invocation.deadline_exceeded`）或 `KERNEL_CANCELLED_001`（`kernel.invocation.cancelled`）。调用器的管道里有三个内置步骤，都按归属 `xihan.kernel.core`（`operation::KERNEL_OWNER`）放在所在阶段的 `Beginning`，与同位置的中间件按 RFC-0004 排序：Observe 阶段的 `ObserveMiddleware` 把排在它之后的中间件与处理器的 panic 翻译成 `KERNEL_UNEXPECTED_001` + `kernel.invocation.unhandled_exception`（内容是 `FrameworkError` 时它进 `inner`），排在它之前的 panic 原样交给调用方；Resilience 阶段的 `DeadlineMiddleware` 在进入时按时钟核对截止时间，已到即报 `KERNEL_TIMEOUT_001` + `kernel.invocation.deadline_before_start`，后面的中间件与处理器不执行（Ingress 与 Observe 阶段照常执行）；Validate 阶段的 `ValidationMiddleware` 解码原始调用的载荷，类型化调用跳过。进管道前只核对未登记与类型化调用的类型不符，按这个顺序。panic 的翻译要 `panic = "unwind"`。

线上编码与 .NET 参考实现一致：

```rust
use xihan_framework::primitives::{Error, WireResult};

let ok = serde_json::to_string(&WireResult(Ok::<_, Error>(7))).unwrap();
assert_eq!(ok, r#"{"ok":true,"value":7}"#);
let failed = serde_json::to_string(&WireResult(Err::<u32, _>(Error::literal("ORDERS_CONFLICT_001", "stale")))).unwrap();
assert_eq!(failed, r#"{"ok":false,"error":{"code":"ORDERS_CONFLICT_001","message":"stale"}}"#);
```

读写按 RFC-0001「Result」与向量 `wire-encoding.json`：不带值的成功 `Result<()>` 只写 `{"ok":true}`，读入时带了 `value` 即拒绝；有值成功的 `value` 必填，只有值类型是 `Maybe<T>`（`Option<T>`）时可以是 `null`；`Error` 的 `code` 与非空的 `message` 必填，可选成员写成 `null` 不合法；Result 与 Error 只收 JSON 对象，未知成员忽略。`Maybe<T>` 有值为值本身、无值为 `null`；`TypedId` 编码为底层值。线上文本用 `primitives::json::from_slice` 读：它先过共同的 JSON 读取检查（RFC 8259、64 层、合法 Unicode、任何一层的重复成员，含值、细节与内部错误里面），数字按值模型（64 位范围内的整数原样往返，`-0` 是整数 0）；直接用 `serde_json::from_str` 读不到这些检查。值经 `json::Model` 写出：整数值的浮点仍写成浮点（`2.0`、`-0.0`、`1e+17`），非有限的浮点写成字符串 `NaN`、`Infinity`、`-Infinity`。

消息模板按 RFC-0010「模板」与向量 `message-templates.json`：`{name}` / `{0}`，`{{`、`}}` 转义；值的文本形式是字符串原样、布尔 `true` / `false`、`null` 为空串、浮点在 1e-4 ≤ |x| < 1e17 时写最短往返的十进制（`2.0` 写 `2`），范围之外写 `1E+17`、`9.9E-05`，负零写 `-0`，数组与对象写紧凑 JSON（成员按名字的 UTF-16 码元序，超过 64 层的容器写 `null`）。说明符只认 `^[DdFfNnXx][0-9]{0,2}$`：`D`、`X` / `x` 只用于整数（`X` 只用于非负整数），`F`、`N` 用于整数与浮点，正好一半时取偶、写出全部整数位；其余说明符（`E2`、`0.00`、`yyyy-MM-dd`、`D100`）与不适用的情形一律按无格式输出，宿主也一样。`format_message` 等按不变文化；`format_message_in` / `try_format_message_in` / `format_value_in` 按区域取小数点与分组符（规范的固定表：de-DE、es-ES、pt-BR 是 `,` 与 `.`，fr-FR 是 `,` 与 U+202F，ru-RU 是 `,` 与 U+00A0，其余区域同不变文化），`Catalog` 按请求的区域格式化。`Describe` 的参数按名字的序数顺序（UTF-16 码元），按不变文化。清单用 `manifest::read` 读入、`manifest::write` 按规范的排版写出（两空格缩进、LF、末尾不换行、成员顺序固定、列表总写、缺省值写出），各语言逐字相同；`Manifest` 里没写的 `role`、`activation` 保持「没有」，写出时写成缺省值，再读回来是显式的值。

## Rust 里的写法

规范留给各语言写明的映射：

**请求类型的成员**（RFC-0011 规则 2）。载荷按 serde 读入请求类型：成员名区分大小写，未知成员忽略（不要加 `deny_unknown_fields`），数字不从字符串读，整数不接受 `1.0` 与 `1e2`、超出类型的范围即不符，浮点接受整数写法、`1e400` 为无穷、`1e-400` 为 0，对象位置只收 JSON 对象。

| 规范 | Rust |
| --- | --- |
| 必填，不接受 `null` | 不带缺省值的非 `Option` 成员：`n: i64` |
| 必填，接受 `null` | `#[serde(deserialize_with = "operation::nullable")] name: Option<String>`：缺少即不符，`null` 为 `None` |
| 可选，不接受 `null`（整数、浮点、布尔） | `#[serde(default)] count: i64`：缺少取缺省值，`null` 不符 |
| 可选，接受 `null`（字符串、数组、对象） | `Option<T>`：缺少与 `null` 都是 `None` |
| 接受 `null` 的数组项 | `Vec<Option<String>>` |
| 任意 JSON | `serde_json::Value` 或 `Option<Value>` |

缺少必填成员在所在对象结束时报该对象的路径（根上为 `$`），类型不符按文本顺序报第一处；这两条都由 serde 派生的读入顺序给出，自定义的 `Deserialize` 要照样在读完对象之后再报缺少的成员。

**值模型**（RFC-0010「模板」）。模板参数与错误细节是 `serde_json::Value`：

- 整数：`Value` 里 `i64` 范围内的整数是整数；`u64` 的上半段（大于 `i64::MAX`）是双精度浮点；`i128` / `u128` 经 `json::to_value` 转，64 位之外的是双精度浮点。
- 浮点：`f64` 原样；`f32` 先换成最接近的 `f64`（`Value::from(0.1f32)` 与 `json::to_value(&0.1f32)` 都是 `0.10000000149011612`）。`Value` 装不下非有限的浮点：`Value::from(f64::NAN)` 是 `null`，`json::to_value(&f64::NAN)` 是字符串 `"NaN"`；读入时上溢的数（`1e400`）进 `f64` 成员是无穷，进 `Value` 是 `null`。
- 字符：`char` 经 `json::to_value` 是一个字符的字符串。映射：`BTreeMap<String, _>`、`HashMap<String, _>` 与结构体是对象，`Vec`、数组、元组是数组。
- 模型之外的类型（`chrono` 的时间、`uuid::Uuid`、枚举等）：先按它们的 `Serialize`（`json::to_value`，时间与 UUID 通常是字符串、单元枚举是变体名）或 `to_string()`（`Display`）转成值，SDK 不另行格式化；要跨语言格式化时先转成字符串。

**文法**：规范里的正则都是手写的逐字节判定，整串匹配，结尾的换行不算匹配。

## 依赖

| crate | 用途 |
| --- | --- |
| `serde`（`derive`）、`serde_json` | `Result` / `Error` / `Maybe` 的线上编码、XGI 载荷编解码、清单读写；`serde_json::Value` 是错误细节与模板参数的值类型 |
| `getrandom` | 追踪 ID 与 Span ID 的随机数 |
| `serde_path_to_error` | 载荷不合法时给出出错位置的 JSON 路径（`$.lines[1].quantity`）：RFC-0011 规则 2 要求 `reason` 只写路径，`serde_json` 本身只报行列 |

JSON 文本的共同读取检查是自带的逐字节读取器（`primitives::json`），读出保留文档顺序的值树，再经自带的 serde `Deserializer` 读入目标类型：`serde_json` 对重复成员取最后一个、不查孤立的代理项转义、把 `-0` 读成浮点、对 `1e400` 报错，`Value` 又按键排序，都与规范不符。

规范里的文法（错误码、消息键、Baggage 键、清单各字段、`traceparent` 等）是手写的逐字节判定，不依赖正则库；取消令牌与截止时间是自带的轻量实现，不依赖异步运行时。开发依赖只有给 `serde_json` 打开 `raw_value`：一致性测试按原文取出向量里的清单，保留成员在文档里的顺序；测试用自写的最小执行器运行 future。

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

`0.1`：原语、清单、调用上下文、管道与操作可用，通过规范仓库 `conformance/vectors/` 下 primitives（含 `json-text.json`）、manifest、pipeline、operations 的全部向量（尚未发布到 crates.io，那里是名字占位的 0.0.1）。规划中：

- 进程外插件运行时 `plugin`（`xihan.plugin.v1`，基于 tonic，以 cargo feature 启用；unix 套接字 / Windows 命名管道 / tcp 回环），用规范仓库的黑盒运行器 `xihan-conformance plugin --dir <产物目录>` 验证；
- Wasm 插件 ABI（RFC-0018，`xihan_*` 导出、`xihan` 导入，WASI preview 1 reactor），样板在 XiHan.Framework 的 `samples/EchoWasmPlugin`；
- 远程 Provider 的操作集（RFC-0019，首个是 `caching.distributed@1`）与示例插件。

拆成多个 crate 时用 `xihan-framework-<模块>` 前缀；领域词与规范术语表（`conventions/vocabulary.md`）一致，只换本语言的大小写风格。公开契约的变更不在 SDK 仓库里做：走 framework-spec 的 RFC 流程，再同步到各 SDK。

## 许可

[MIT](LICENSE)
