---
name: xihan-sdk-rust-commit-msg
description: 根据 Rust SDK 当前暂存区生成一行提交信息时使用，适用于「提交信息」「commit msg」「msg」或提交前概括暂存的改动；只生成消息，不负责暂存、解决冲突或执行提交。
---

# Rust SDK 提交信息

1. 检查 `git status --short --branch`；存在冲突、merge 或 rebase 时先报告，不生成完成态消息。
2. 只读取 `git diff --cached --stat`、完整的暂存 diff 和最近 20 条提交。暂存区为空时不从未暂存内容猜测。
3. 把全部暂存变化归纳成一个意图，输出一行 Conventional Commit。

格式：`<type>(<scope>): <中文说明>`。type 使用 `feat`、`fix`、`refactor`、`perf`、`docs`、`style`、`test`、`build`、`ci`、`chore`、`revert`。

scope 用本仓库的模块名：`primitives`、`manifest`、`invocation`、`pipeline`、`operation`、`plugin`；或 `conformance`（一致性向量与运行器测试）、`spec`（只升规范指针）、`docs`、`build`（`Cargo.toml`、格式与生成器配置）、`ci`。

- 说明写使用者可感知的结果，必要时写原因与对应的 RFC、ADR；随改动升了规范指针时写「规范指针升到 <短哈希>」。
- 删除、改名或改变公开 API、panic 条件、错误码、消息键或线上编码时，在说明里写明改了什么与怎么迁移。
- 只要求消息时最终只输出一行标题，不附解释或候选列表。
