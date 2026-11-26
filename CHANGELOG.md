# 更新日志 · XiHan Framework SDK for Rust

本文件记录 crate `xihan-framework` 各版本中使用者可感知的变更，每条标注 **新增 / 修复 / 优化 / 调整 / 移除** 类别；仓库自身的 CI、测试与构建脚本不列入。迁移的细节见文档站 [Rust SDK](https://framework.docs.xihanfun.com/sdks/rust) 的「破坏性改动与迁移」。

## 未发布

### 升级须知

- `WireResult` 与 `Error` 的线上编码从结果或错误的根算起超过 64 层，或 `RawValue`（含 `json::Json`）原样写进去的文本有重复成员、孤立代理项的转义时，序列化以 `PRIMITIVES_VALIDATION_001` 的错误失败、不交出文本：放进结果与错误之前保证原样嵌入的文本合法、层数不超（ADR-0320）

### 变更

- **修复** 插件写给宿主的错误细节超过 64 层时这个错误不带细节、其余字段照写（ADR-0320）
- **调整** `examples/echo` 的 `events.list` 回显处理器拿到的信封（`envelopes`），黑盒运行器的用例 `event-envelope` 通过（ADR-0327）
- **调整** 规范指针升到 `6bd152b`（ADR-0320、ADR-0321、ADR-0327、ADR-0328）

## v0.2.0-rc.1（2026-11-21）

### 升级须知

- 首个对外版本，是预发布版：`cargo add xihan-framework@0.2.0-rc.1`
- 钉在规范 `v1.0.0-rc.1`

### 变更

- **新增** crate `xihan-framework`：`primitives`、`manifest`、`invocation`、`pipeline`、`operation`，不打开 feature 时不依赖异步运行时
- **新增** feature `plugin`：基于 tonic 的进程外插件运行时与宿主回调，示例插件 `examples/echo`
