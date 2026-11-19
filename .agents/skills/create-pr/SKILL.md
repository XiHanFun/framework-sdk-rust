---
name: xihan-sdk-rust-create-pr
description: 为 Rust SDK 准备、审查或创建 Pull Request 时使用，适用于生成 PR 标题与正文、比较分支或执行 gh pr create；普通代码审查与 Issue 回复不使用。
---

# Rust SDK 创建 PR

## 前置检查

- 检查分支、上游、工作区、merge 或 rebase 与冲突；状态不正常时不创建 PR。
- base 默认 `dev`，用户明确要求时才面向 `main`。
- 用 `git log <base>..HEAD`、`git diff --stat <base>...HEAD` 和完整 diff 审查整条分支。
- 核对公开 API、文档注释、README（两份同步）、测试与规范指针；改了 `spec/` 时核对协议生成代码已重新生成。

## PR 内容

仓库没有 PR 模板，正文按下面几段写：

1. **关联 Issue**：编号或「无」。
2. **变更说明**：使用者可感知的结果，对应的 RFC、ADR 与规范提交。
3. **影响范围**：涉及的模块（`primitives`、`manifest`、`invocation`、`pipeline`、`operation`、`plugin`）与 feature。
4. **自测**：只写实际执行过的门禁（照 `testing-release` 技能），注明平台；没有跑黑盒运行器或只跑了一个平台时如实写明。
5. **破坏性变更**：公开 API、panic 条件、错误码、消息键或线上编码的变化与迁移方式；文档页「破坏性改动与迁移」是否已同步；没有写「无」。

标题用 Conventional Commit 形态，scope 用模块名。

只要草稿时不写远程。明确要求创建 PR 时才推送当前分支并执行 `gh pr create`；禁止 force push、合并或改写其他远端分支。创建后返回 PR 链接。
