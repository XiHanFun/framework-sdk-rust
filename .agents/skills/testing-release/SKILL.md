---
name: xihan-sdk-rust-testing-release
description: 运行或排查 Rust SDK 的门禁、向量覆盖测试、黑盒运行器、文档测试与打包验证时使用，适用于提交前验证、CI 失败复现和升规范指针后的核对；不负责实现功能，版本与发布见 version-release。
---

# Rust SDK 测试与门禁

以 `.github/workflows/ci.yml` 为事实源；门禁的逐项说明与失败处理读 `references/gates.md`。

## 全部门禁

在仓库根按 CI 的顺序执行：

```bash
git submodule update --init
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features plugin -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --features plugin
cargo test
cargo test --features plugin
cargo +1.85 test --no-run
cargo +1.85 test --no-run --features plugin
cargo run --locked --manifest-path tools/protogen/Cargo.toml
git status --porcelain -- src/plugin/generated
```

- 最后一行应当没有输出：生成物与提交的一致。
- `cargo +1.85` 要 rustup 装有 1.85 工具链；CI 的 `msrv` 作业编译库、全部测试与示例（`--no-run`），不运行，也不检出规范 submodule：测试在运行时才读 `spec/`。
- 两种 feature 组合（默认与 `--features plugin`）都要过；`cargo test --features plugin` 含黑盒运行器，要 Go。
- 先跑受影响的测试（`cargo test --test <名字>`、`cargo test --features plugin --test plugin`），再跑全部。

## 向量覆盖

`tests/conformance_vectors.rs` 读 `spec/conformance/profiles.json`，认领的档（`core`、`plugin`）要求的每个向量要么在 `REPLAYED`，要么在 `GAPS` 并写理由。它失败时：给新向量写回放测试并加进 `REPLAYED`；确实暂不回放时列进 `GAPS`，同时写进两份 README 的「状态」与文档页的「一致性」。不靠删档或加豁免让它通过。

## 文档测试

两份 README 的 Rust 示例由 `src/lib.rs` 的 `ReadmeDoctests` / `ReadmeCnDoctests` 作为文档测试运行，`cargo test` 与 `cargo test --features plugin` 都会编译它们。改 README 后两种组合都跑；删掉的示例要确认 `examples/`、模块文档或测试仍覆盖到它。

## 打包

CI 的 `test` 作业在 ubuntu 上跑 `cargo publish --dry-run`：打包并构建包里的内容，不上传（版本已经发布过时只给警告）；本地复现要工作区干净，或加 `--allow-dirty`。它不看包里多了什么：发布前仍用 `cargo package --list` 核对包内容：不含 `spec/`、`tests/`、`tools/`、`scripts/`、Agent 的指令与技能（`AGENTS.md`、`CLAUDE.md`、`.agents/`、`.claude/`）和仓库配置文件（`Cargo.toml` 的 `exclude`）。发布见 `version-release` 技能。

## 本机没有 Go 时

黑盒运行器测试会失败。装 Go（版本见 `spec/conformance/go.mod`）后再跑；只在明确需要跳过时设 `XIHAN_CONFORMANCE_REQUIRED=0`，并在结果里说明没有跑运行器。

失败时保留输出作为证据，不删测试、不放宽门禁、不改 CI 制造通过。
