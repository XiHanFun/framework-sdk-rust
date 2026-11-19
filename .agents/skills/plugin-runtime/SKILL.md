---
name: xihan-sdk-rust-plugin-runtime
description: 实现、修复或对齐 Rust SDK 的进程外插件运行时（feature plugin）时使用，适用于握手、生命周期、调用与投递、宿主回调、端点与平台差异、协议代码生成和黑盒运行器；原语与操作契约本身用 sdk-development，发版用 version-release。
---

# Rust SDK 插件运行时

规范章节与实现文件的对应、平台差异的细节读 `references/runtime-map.md`。

## 规范位置

- RFC-0017 `spec/rfcs/0017-plugin-protocol.md`：协议总则、错误码、一致性。
- `spec/plugin/handshake.md`：环境变量（§1）、端点（§2、§4）、握手行与版本协商、握手内容（§3）、会话令牌。
- `spec/plugin/lifecycle.md` §7：次序表、配置节、就绪、健康、停止与收尾。
- `spec/plugin/invocation.md`：调用（§1）、上下文元数据（§2）、错误的读写（§3）、事件投递、宿主回调与状态映射（§5）、日志（§6）、共用消息键（§8）。
- `spec/plugin/plugin.proto`：`xihan.plugin.v1` 的消息与服务。Wasm 载体 `spec/plugin/wasm.md` 本 SDK 不实现。
- 向量在 `spec/conformance/vectors/plugin/`（`wasm-exports.json` 属于 `wasm` 档，不归本 SDK）；运行器的用例表在 `spec/conformance/README.md`。

## 实现位置

`src/plugin/` 只在 feature `plugin` 下编译：`runtime.rs` 组装与运行、`service.rs` 实现 `Plugin` 服务、`host.rs` 宿主回调、`endpoint.rs` 端点、`session.rs` 会话令牌、`convert.rs` 原语与协议消息的转换、`time.rs` 截止时间的线上写法、`catalog.rs` 与 `messages.rs` 消息目录和共用消息键、`log.rs` stderr 日志、`types.rs` 插件作者用的类型、`generated/` 生成代码、`localization/` 10 种区域的文案。示例插件 `examples/echo/`。

## 协议代码

`src/plugin/generated/xihan.plugin.v1.rs` 由 `spec/plugin/plugin.proto` 生成并提交，使用者不需要 protoc。升规范指针后重新生成并一并提交：

```bash
cargo run --manifest-path tools/protogen/Cargo.toml
```

`.proto` 由纯 Rust 的 protox 编译，本机也不需要 protoc；CI 的 `protocol` 作业以 `--locked` 重新生成，按 `git status --porcelain -- src/plugin/generated` 判断与提交的一致（能发现新增的生成文件）。生成器版本与 tonic 同一版本，钉在 `tools/protogen/Cargo.toml`，不手改生成代码。

## 黑盒运行器

`tests/plugin_conformance.rs` 构建 echo（`cargo build --example echo --features plugin`），与 `examples/echo/xihan.manifest.json` 一起放进临时目录，在 `spec/conformance` 用 `go run ./cmd/xihan-conformance plugin` 跑全部用例，断言每个用例都通过、没有跳过；用例名单 `CASES` 与运行器的用例表一致。手动运行：

```bash
cd spec/conformance
go run ./cmd/xihan-conformance plugin --dir <插件目录> --config '{"greeting":"hello"}' --sample 'echoes.create={"text":"conformance"}' --exclude echoes.crash
```

要 Go（版本见 `spec/conformance/go.mod`）；缺 Go 时测试失败，只有设 `XIHAN_CONFORMANCE_REQUIRED=0` 才打印原因并提前返回。CI 在 ubuntu 与 windows 上都跑，两个平台都要过。

## 平台差异

- Unix：监听临时目录下新建的私有目录（`0700`）里的 Unix 域套接字（`0600`）；SIGTERM 只在 Unix 上监听（`#[cfg(unix)]`），与 stdin 关闭一样立即收尾。
- Windows：监听命名管道，拒绝远程客户端；本 crate 禁用 unsafe，设不了只含当前用户的访问控制表，隔离靠会话令牌。收尾的请求是 stdin 关闭。
- `with_network(Network::Tcp)`：只绑定 `127.0.0.1` 的回环 TCP，隔离只靠会话令牌。
- 回调端点按 `Network::supported()` 取 `XIHAN_PLUGIN_HOST_ADDRESS` 里第一个本平台能连的项。

## 完成条件

1. 行为按上面的规范章节，`plugin/` 向量都回放，`tests/plugin.rs` 覆盖了改动的核对顺序与状态映射。
2. `cargo test --features plugin` 在本机通过，含黑盒运行器的全部用例；涉及端点或信号时说明 Windows 与 Unix 两边是否都验证过（本机只能跑一边时如实写明，交给 CI 的矩阵）。
3. 协议代码与 `spec/plugin/plugin.proto` 一致；文档页的「写一个插件」已同步。
