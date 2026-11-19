---
name: xihan-sdk-rust-version-release
description: 为 Rust SDK 准备版本、更新 Cargo.toml 的版本、核对规范标签与包内容或向 crates.io 发布时使用；只有用户明确要求时才由用户的凭据执行 cargo publish，不负责实现功能与日常门禁。
---

# Rust SDK 版本发布

## 事实

- 版本真源是 `Cargo.toml` 的 `[package] version`；`primitives::SDK_VERSION` 取自 `CARGO_PKG_VERSION`，`src/lib.rs` 的单元测试 `exposes_versions` 写着当前版本。
- 注册表是 crates.io，包名 `xihan-framework`；那里目前只有占名的版本，还没有发布过正式版本。
- 仓库没有发布工作流，也还没有版本标签与标签规则。
- 打包排除 `spec/`、`tests/`、`tools/`、Agent 的指令与技能（`AGENTS.md`、`CLAUDE.md`、`.agents/`、`.claude/`）和仓库配置文件（`Cargo.toml` 的 `exclude`）。

## 与规范标签的关系（RFC-0024「实现钉规范」）

- 发预发布或正式版时，`spec/` 子模块必须正好在某个规范标签上：正式版只能钉规范的正式版标签（不带预发布后缀），预发布可以钉 rc 标签。
- 规范还没有标签时，先在 framework-spec 打标签（同样要用户明确要求），再把 `spec/` 指过去。
- RFC-0024 要求各实现的发布工作流核对这一条；本仓库还没有发布工作流，需要时先与用户确认再加。
- 两份 README 的「状态」写明钉的规范版本、认领的档与缺口，发版时把钉的规范版本改成那个标签；文档页的「一致性」同步。

## 准备版本

每一步都在用户要求之后才做，版本号由用户定：

1. 工作区干净，在 `dev` 上，无 merge 或 rebase。
2. 核对上一个发布点到当前的公开 API 与行为变化，文档页的「破坏性改动与迁移」已写（`changelog-collect` 技能）。
3. 改 `Cargo.toml` 的 `version` 与 `exposes_versions` 里的版本，`spec/` 钉在规范标签上，README 的「状态」与文档页同步。
4. 跑全部门禁（`testing-release` 技能），再用 `cargo package --list` 核对包内容，`cargo package` 确认打出的包能构建。
5. 版本提交推 `dev`、等 CI 全绿；需要时按用户要求由 `dev` 快进 `main`。

## 发布

- 只有用户明确要求发布时才执行：用户用自己的 crates.io 令牌（`cargo login`）运行 `cargo publish`；令牌由用户自己输入，Agent 不读取、不保存、不转述。
- crates.io 上的版本删不掉，只能 yank，发错了只能用下一个号。
- 发布后核对 crates.io 与 docs.rs 上的版本，失败时停止并报告，不改写已推送的提交。
