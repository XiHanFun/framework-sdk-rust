# 插件运行时：规范与实现对应

用户可见的完整行为说明在 XiHan.Framework 的 `docs/sdks/rust.md`「写一个插件」，改行为时两边同步。

| 规范 | 内容 | 实现 | 向量与测试 |
| --- | --- | --- | --- |
| `handshake.md` §1 | 宿主下发的环境变量 | `runtime.rs` | `tests/plugin.rs` |
| `handshake.md` §2、§4 | 监听端点、握手行、回调端点列表、会话令牌 | `endpoint.rs`、`runtime.rs`、`session.rs` | `plugin/handshake-lines.json`、`plugin/host-addresses.json`（`tests/plugin.rs`） |
| `handshake.md` §3 | 版本协商与握手内容（清单原文、一元操作声明、订阅、消息目录） | `service.rs`、`catalog.rs` | `plugin/protocol-versions.json`（`tests/plugin.rs`） |
| `lifecycle.md` §7 | 次序表、配置节、就绪、健康与健康推送、停止宽限、stdin 与 SIGTERM 收尾 | `service.rs`、`runtime.rs`、`types.rs` | `tests/plugin.rs`、黑盒运行器 |
| `invocation.md` §1 | `Invoke`、`HandleEvent`、`InvokeStream` 的核对顺序 | `service.rs` | `tests/plugin.rs`、黑盒运行器 |
| `invocation.md` §2 | `InvocationMetadata` 与调用上下文的互转、截止时间的写法 | `convert.rs`、`time.rs` | `plugin/deadlines.json`（`src/plugin/time.rs`） |
| `invocation.md` §3 | 错误写进 `Status` 与读入宿主写来的 `Error` | `convert.rs` | `plugin/error-reading.json`（`src/plugin/convert.rs`） |
| `invocation.md` §5 | `Host.Invoke`、`Host.Publish`：发出前的本地核对、gRPC 状态映射 | `host.rs` | `plugin/callback-status.json`（`tests/plugin.rs`、`src/plugin/host.rs`） |
| `invocation.md` §6 | stderr 的 JSON 行日志 | `log.rs` | `tests/plugin.rs` |
| `invocation.md` §8 | 与宿主共用的 7 个消息键 | `messages.rs`、`catalog.rs`、`localization/` | `plugin/plugin-messages.json`（`src/plugin/catalog.rs`、`src/plugin/messages.rs`） |

## 端点

- Unix 域套接字：放在临时目录下新建的私有目录里（目录 `0700`、名字随机、已经存在就换个名字，套接字 `0600`）；临时目录的路径含空白或不是 UTF-8 时改用 `/tmp`，握手行的地址不含空白；停止后连同目录删掉。
- Windows 命名管道 `\\.\pipe\xihan-plugin-<随机>`：以 `first_pipe_instance` 创建，同名管道已经存在就换个名字；`reject_remote_clients` 拒绝远程客户端；用系统缺省的安全描述符，本用户、管理员与 SYSTEM 能读写，其他本机用户只能以只读打开、发不出请求。访问控制表要 unsafe 才设得了，本 crate 禁用 unsafe。
- 回环 TCP：`with_network(Network::Tcp)`，只绑定 `127.0.0.1`，同机的其他用户连得上，隔离只靠会话令牌。
- 回调端点：`XIHAN_PLUGIN_HOST_ADDRESS` 是 `;` 分隔的 `<network> <address>` 列表（`plugin::parse_host_addresses`），按顺序取第一个 `Network::supported()` 里的项（Unix 上 `unix` 与 `tcp`，Windows 上 `npipe` 与 `tcp`），之后不换；没有能连的项时照常服务、启动时记一条 `warning`，回调报 `plugins.host.unavailable` 且不发请求。

## 收尾

- stdin 关闭与 SIGTERM（只在 Unix 上监听）：可服务的进入 `Draining`，立即触发全部在途调用的取消令牌，启动成功过的调一次停止钩子；从收到起最多 3 秒以退出码 0 退出。
- `Running::shutdown` 按同样的步骤收尾，但不结束进程，测试里用它。

## 依赖与版本

tonic、tonic-prost、prost、tokio、tokio-stream、hyper-util 都是 `optional`，只由 feature `plugin` 打开；tonic 较新的补丁版本要求更高的 Rust，依赖按 `rust-version` 解析到兼容的版本，生成代码由同一版本的生成器（`tools/protogen/Cargo.toml` 钉死）生成。升级 tonic 时生成器与 `Cargo.toml` 一起改，并重新生成。运行时只要求当前线程的 tokio 运行时，多线程与否由插件作者选。
