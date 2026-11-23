# 模块、规范与测试对应表

向量路径相对 `spec/conformance/vectors/`；回放清单以 `tests/conformance_vectors.rs` 的 `REPLAYED` 为准。

| 模块 | 实现 | 规范 | 向量 | 测试 |
| --- | --- | --- | --- | --- |
| `primitives` | `src/primitives/`（错误、错误码、结果与 `WireResult`、`TypedId`、生命周期、阶段、稳定性、Baggage、消息键与模板、`Catalog`、`FrameworkError`） | RFC-0001、RFC-0010「模板」、`conventions/error-codes.md`、`conventions/baggage-keys.md` | `primitives/` 下除 `trace-context.json`、`capability-requirements.json` 外的全部 | `tests/primitives.rs` |
| `primitives::json` | `src/primitives/json/`：`reader.rs` 共同读取检查、`de.rs` 按 serde 读入、`ser.rs` 值模型写出、`value.rs` 原样保留的 `Json`、`lenient.rs` 配置节的类型化读取 | RFC-0001「JSON 文本」「Result」、RFC-0011 规则 2 | `primitives/json-text.json`、`primitives/wire-encoding.json` | `tests/primitives.rs` |
| `manifest` | `src/manifest/`（`shape.rs` 按 Schema 逐项核对） | RFC-0002、`manifest/manifest.schema.json` | `manifest/manifests.json`、`primitives/capability-requirements.json` | `tests/manifest.rs` |
| `invocation` | `src/invocation/`（`cancellation.rs` 令牌、`deadline.rs` 后台定时器、`trace.rs` W3C 追踪） | RFC-0003 | `primitives/trace-context.json` | `tests/invocation.rs` |
| `pipeline` | `src/pipeline/` | RFC-0004 | `pipeline/ordering.json` | `tests/pipeline.rs` |
| `operation` | `src/operation/`（契约、`Schema`、`Registry`、`Invoker`、三个内置步骤） | RFC-0011 | `operations/payload-reasons.json`、`primitives/kernel-messages.json` | `tests/operation.rs` |
| 文法 | 共用的字节类、组合与空白字符表在 `src/grammar.rs`；各领域的文法在各自的模块：`primitives/error_code.rs`、`primitives/messages.rs`（消息键、区域名）、`primitives/baggage.rs`、`manifest/shape.rs`、`invocation/trace.rs`、`plugin/types.rs`（主题）、`plugin/endpoint.rs`、`plugin/time.rs` | 各 RFC 与 `conventions/` 里的正则、RFC-0001「空白字符」 | 随所在模块 | 随所在模块 |
| `plugin` | `src/plugin/`（见 `plugin-runtime` 技能） | RFC-0017、`plugin/` | `plugin/` | `tests/plugin.rs`、`tests/plugin_conformance.rs`、模块内单元测试 |

`primitives/kernel-messages.json` 由 `tests/common` 读取，供 primitives、invocation、manifest、operation 的测试共用。

## 回放向量的写法

- 集成测试用 `tests/common` 的 `common::vector(relative)` 读向量（`mod common;` 引入），没有检出 submodule 时它给出 `git submodule update --init` 的提示；crate 内的单元测试按 `CARGO_MANIFEST_DIR` 拼出 `spec/` 下的路径读取。
- 需要按原文保留成员顺序时（如清单），借 `serde_json::value::RawValue` 取出原文。
- 异步用例用 `tests/common` 的最小执行器运行，不引入 tokio 等开发依赖。
- 断言逐个用例给出名字，失败时能定位到向量里的那一项。
- 新回放的向量加进 `tests/conformance_vectors.rs` 的 `REPLAYED`，行尾注释写回放它的测试。
