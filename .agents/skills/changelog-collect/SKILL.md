---
name: xihan-sdk-rust-changelog-collect
description: 收集 Rust SDK 两个引用之间的用户可感知变化、写进 XiHan.Framework 文档站 docs/sdks/rust.md 的「破坏性改动与迁移」时使用；它不改版本、不打标签，也不发布 crate。
---

# Rust SDK 变更整理

## 事实源

- 用户指定的起止引用；没有指定时先列出最近的发布点（还没有标签时问用户）与当前 `HEAD`。
- 范围内的提交、实际 diff 与公开 API 的变化：`git log <起>..<止>`、`git diff <起>..<止> -- src Cargo.toml`。
- 规范指针的变化：`git diff <起>..<止> -- spec` 给出前后两个规范提交，规范一侧的变化看 framework-spec 的 `CHANGELOG.md`。
- 目标文件：XiHan.Framework 仓库（本地 `../XiHan.Framework`）的 `docs/sdks/rust.md`「破坏性改动与迁移」一节。

## 收录规则

- 只收 crate 使用者可感知的变化：公开 API 的删除、改名与签名变化，panic 条件、错误码、消息键与线上编码的变化，feature 与依赖要求的变化，默认行为的变化。
- 纯测试、CI、内部重构与生成器的改动不收，除非改变了包内容或使用方式。
- 一条一行，写结果与迁移动作（改成什么、怎么改），写明模块与 API 名，不罗列内部文件。
- 规范带来的变化写明对应的 RFC 或 ADR 编号。
- 同一底层改动影响多处时合并成一条。

文档页在另一个仓库，改完在那个仓库根用 `pnpm --dir docs build` 验证并单独提交。本技能不改 `Cargo.toml` 的版本、不打标签、不发布。
