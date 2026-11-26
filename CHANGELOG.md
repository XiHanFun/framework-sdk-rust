# 更新日志 · XiHan Framework SDK for Rust

本页记录 crate `xihan-framework` 各版本中使用者可感知的变更，每条标注 **新增 / 修复 / 优化 / 调整 / 移除** 类别；仓库自身的 CI、测试与构建脚本不列入。版本号见 `Cargo.toml`，详细说明见文档站 [Rust SDK](https://framework.docs.xihanfun.com/sdks/rust) 的「破坏性改动与迁移」。

## 未发布

### 升级须知

- `plugin::Envelope.time` 由 `Option<String>` 改为 `Option<SystemTime>`，新增 `with_time`：发布处把 `time = Some(t.to_rfc3339())` 改成 `.with_time(t.into())`，读 `envelope.time` 的按 `SystemTime` 读（ADR-0306）
- `plugin::Binding` 新增字段 `properties`、`Debug` 不再带 `value`，`plugin::v1::Binding` 多了 `properties`、不再实现 `Eq` 与 `Hash`：用结构体字面量构造 `v1::Binding` 的补上字段或写 `..Default::default()`，依赖 `Eq` / `Hash` 的改用 `PartialEq`（ADR-0285）
- `plugin::Envelope` 与 `plugin::v1::Envelope` 新增 `data_schema`：用结构体字面量构造 `v1::Envelope` 的补上字段或写 `..Default::default()`（ADR-0305）
- `plugin::Network`、`Level`、`SubscriptionMode`、`HealthStatus` 与 `manifest::Kind` 加了 `#[non_exhaustive]`：crate 外穷尽的 `match` 加上通配分支
- `manifest::Kind` / `Role` / `Isolation` 的 serde 只按字符串读写，不再接受 `{"plugin":null}` 这种写法：自己用 serde 存这些枚举的数据改成字符串
- `Registry::add` / `add_with` 拒绝五个协议名之外的值与写了两次的协议名（`PRIMITIVES_VALIDATION_001`）：`with_expose` 只用 `EXPOSE_*` 常量，去掉重复
- 超过 128 个字符的操作 ID 不再合法，`operation::is_id`、`Registry::add` 返回 `PRIMITIVES_VALIDATION_001`，读清单为 `kernel.manifest.invalid`：把 ID 缩到 128 个字符以内（ADR-0276）
- `with_*`（`Error`、`Contract`、`Plugin`、`Subscription`、`Envelope`、`Context`）标了 `#[must_use]`：丢掉返回值的调用改成接住返回值
- 类型化响应写出的文本嵌套超过 64 层、有重复成员或 `RawValue` 里有孤立代理项的转义时，以 `KERNEL_UNEXPECTED_001` + `kernel.invocation.unhandled_exception` 失败、没有载荷：自定义的 `Serialize` 与 `flatten` 不写重复的名字，任意 JSON 用 `json::Json`（ADR-0319）
- `WireResult` 与 `Error` 的线上编码从结果或错误的根算起超过 64 层，或 `RawValue`（含 `json::Json`）原样写进去的文本有重复成员、孤立代理项的转义时，序列化以 `PRIMITIVES_VALIDATION_001` 的错误失败、不交出文本：放进结果与错误之前保证原样嵌入的文本合法、层数不超（ADR-0320）

### 变更

- **新增** `plugin::Binding.properties`：`Configure` 下发的绑定属性交给配置钩子（ADR-0285）
- **新增** `plugin::Envelope.data_schema` 与 `with_data_schema`：发布与投递原样带上 CloudEvents `dataschema`（ADR-0305）
- **新增** `Host::invoke_json`、`Envelope::with_json_data` 与 `data_as`，公开 `json::to_vec`
- **新增** `Context::with_cancellation`、`with_deadline`、`with_timeout` 与 `cancellation_error`
- **新增** `pipeline::middleware_fn`、`operation::EXPOSE_ALL`、`operation::ID_MAX_LENGTH`、`From<FrameworkError> for Error`，`Error` 实现 `Eq`；目录的 `from_json` / `extend`、`LOCALES`、`is_locale_name`、`is_whitespace` 与 `is_blank` 在 `primitives` 公开
- **新增** `plugin::ERROR_CODE_PLUGIN_UNEXPECTED`，公开 `ERROR_CODE_INVALID_SOURCE` 与 `ERROR_CODE_HANDSHAKE_MISMATCH`
- **修复** 宿主写来的信封 `time` 读不出（Windows 上还有 1601 年之前）时投递以 `InvalidArgument` 拒绝（ADR-0306）
- **修复** 插件写给宿主的错误细节超过 64 层时这个错误不带细节、其余字段照写（ADR-0320）
- **修复** `Contract.expose` 的文档改准：空表示不对外发布，行为没变
- **调整** 弃用 `Context::cancellation()` 与 `plugin::ERROR_CODE_UNEXPECTED`，旧名仍可用：改用 `cancellation_error()` 与 `ERROR_CODE_PLUGIN_UNEXPECTED`；`pipeline::from_fn` 照常可用
- **调整** 清单 `activation` 里 `onEvent:` 的模式可以带下划线（ADR-0276）
- **调整** docs.rs 上标出 `plugin` 模块要打开 feature `plugin`；包的 `homepage` 改成文档站
- **调整** `examples/echo` 的 `events.list` 回显处理器拿到的信封（`envelopes`），黑盒运行器的用例 `event-envelope` 通过（ADR-0327）
- **调整** 规范指针升到规范 `6bd152b`

## v0.2.0-rc.1（2026-11-21）

- **新增** 首个对外预发布版，发布到 crates.io：原语、清单、调用上下文、管道、操作契约与 feature `plugin` 下的进程外插件运行时，钉规范 `v1.0.0-rc.1`，认领一致性档 `core` 与 `plugin`
