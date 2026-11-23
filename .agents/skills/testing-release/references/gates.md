# 门禁明细

## CI 作业（`.github/workflows/ci.yml`）

| 作业 | 平台 | 步骤 | 失败时 |
| --- | --- | --- | --- |
| `test` | ubuntu-latest、windows-latest | 检出含规范 submodule；装 stable Rust（rustfmt、clippy）与 Go（版本取 `spec/conformance/go.mod`）；缓存 cargo 产物（`Swatinem/rust-cache`，另两个作业同样缓存） | — |
| | | `cargo fmt --check` | `cargo fmt` 后复核改动 |
| | | `cargo clippy --all-targets -- -D warnings` 与 `--features plugin` 各一遍 | 修代码；`missing_docs` 随 `-D warnings` 生效，公开项补中文文档注释 |
| | | `RUSTDOCFLAGS=-D warnings` 下 `cargo doc --no-deps` 与 `--features plugin` 各一遍 | 修断掉的文档链接与文档注释 |
| | | `cargo test`，`cargo test --features plugin` | 先单独跑失败的测试目标定位 |
| `msrv` | ubuntu-latest | Rust 1.85 下 `cargo build --lib` 与 `--features plugin` | 换用 1.85 有的标准库 API；依赖按 `rust-version` 解析，不手钉更高要求的版本 |
| `protocol` | ubuntu-latest | `cargo run --locked --manifest-path tools/protogen/Cargo.toml`，再按 `git status --porcelain -- src/plugin/generated` 判空 | 重新生成并提交生成物，不手改 |

触发：`main` 与 `dev` 的 push、目标为这两个分支的 PR、手动 `workflow_dispatch`；同一引用的新运行取消旧运行。

## 最低 Rust 版本

`Cargo.toml` 的 `rust-version` 是 1.85（edition 2024 的要求）。clippy 的 `incompatible_msrv` 按它核对标准库 API，依赖也按它解析到兼容的版本（tonic 与 tonic-prost 停在 `Cargo.toml` 注释写明的版本）。库 crate 不提交 `Cargo.lock`；`tools/protogen` 的锁文件提交。

## 测试目标

| 目标 | feature | 内容 |
| --- | --- | --- |
| `tests/primitives.rs`、`invocation.rs`、`manifest.rs`、`pipeline.rs`、`operation.rs` | 默认 | 各模块的向量回放与单元测试 |
| `tests/conformance_vectors.rs` | 默认 | 认领的档要求的向量都回放或列为缺口 |
| `tests/plugin.rs` | `plugin` | 扮演宿主的 gRPC 客户端核对插件运行时；`plugin/` 向量 |
| `tests/plugin_conformance.rs` | `plugin` | 构建 echo，黑盒运行器的全部用例 |
| crate 内 `#[cfg(test)]` | 两者 | 内部函数，含 `plugin/callback-status.json`、`plugin/deadlines.json`、`plugin/error-reading.json`、`plugin/plugin-messages.json` 的回放 |
| 文档测试 | 两者 | 模块文档与两份 README 的示例 |

## 黑盒运行器的环境

- 运行器以 `go run` 从 `spec/conformance` 运行，版本就是锁定的规范；第一次运行要下载 Go 依赖。
- 本机访问不了默认的 Go 代理与校验和库时，只在当前命令的环境里设 `GOPROXY=https://goproxy.cn,direct` 与 `GOSUMDB=sum.golang.google.cn`，不写进全局配置。
- 运行器输出 `--json` 时，测试逐项打印 `case`、`passed`、`skipped` 与 `message`，失败的用例从这里看。
