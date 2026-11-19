---
name: xihan-sdk-rust-development
description: 设计、实现、重构或对齐 Rust SDK 的原语、清单、调用上下文、管道与操作契约时使用，适用于按规范改行为、回放新向量和与 .NET 参考实现核对差异；进程外插件运行时用 plugin-runtime，门禁与发布用 testing-release 与 version-release。
---

# Rust SDK 开发

模块、规范章节、向量与测试的对应表读 `references/module-map.md`；代码规范以 `AGENTS.md`「代码规范」为准。

## 先查事实

```bash
git status --short --branch
git log -5 --oneline
git -C spec log -1 --oneline
```

按顺序读：`spec/rfcs/0024-versions-and-profiles.md` 里本 SDK 认领的档（`core`、`plugin`）→ 目标模块对应的 RFC 与 `spec/conventions/` → `spec/conformance/vectors/` 里相关的向量 → 目标模块的 `//!` 文档、实现与测试。只读与任务相关的部分。

## 与 .NET 一致

- 规范是事实源，.NET（XiHan.Framework）是参考实现：错误码、消息键、参数、英文模板、核对顺序与线上编码都与它一致；向量的期望值就是 .NET 的行为。
- 只有规范明确留给各语言的部分（文档页的「语言映射」：请求类型成员、值模型、文法的写法）才按 Rust 的习惯定，写进文档页，并说明与规范的对应。
- 拿不准 .NET 的行为时读 XiHan.Framework 的源码与测试（本地克隆在 `../XiHan.Framework`），不凭名字猜。

## 规范没写到时

1. 停下来，不在 SDK 里自行决定。
2. 报告差异：规范怎么写、.NET 怎么做、本 SDK 现在怎么做、影响哪些向量或用例。
3. 提出规范文字（与需要的向量），在 framework-spec 仓库提交；契约变化的顺序是 framework-spec 的 RFC → XiHan.Framework 的 ADR → 各 SDK。
4. 规范提交推送后再升 `spec/` 指针（步骤见 `AGENTS.md`「规范与一致性」）并改实现。

## 实现要点

- 可恢复失败返回 `Err(Error)`，配置期失败返回 `FrameworkError`，编程错误 panic 且内容是 `FrameworkError`；对不可信输入另给 `try_*` 或 `is_*`。
- 文法手写逐字节判定（`src/grammar.rs`），整串匹配；空白用 `grammar::is_whitespace`。
- 线上文本经 `primitives::json::from_slice`；值经 `json::Model` 写出。
- 取消与截止时间只经 `Context` 的令牌；归因只看截止令牌。
- feature `plugin` 之外不引入异步运行时；新依赖要有明确收益。

## 完成条件

1. 行为与规范和 .NET 一致；认领的档要求的向量都回放（`tests/conformance_vectors.rs` 通过），新回放的向量已加进 `REPLAYED`。
2. 全部门禁通过（`testing-release` 技能），两种 feature 组合都过。
3. 公开项有中文文档注释，会 panic 的写明条件；README 的最小示例仍能编译。
4. 改了公开 API 或行为时，XiHan.Framework 的 `docs/sdks/rust.md` 对应章节与「破坏性改动与迁移」已同步（另一个仓库，单独提交）。
5. 一项能力一个提交：`<type>(<scope>): <中文说明>`，scope 用模块名。
