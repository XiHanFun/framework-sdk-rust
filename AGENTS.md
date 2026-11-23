# AGENTS.md

本文件约束在 framework-sdk-rust 仓库内工作的 AI Agent。回答、文档与提交说明使用中文；代码标识、公开 API 和标准技术术语保持英文。

## 仓库概览

framework-sdk-rust 是 [XiHan Spec](https://github.com/XiHanFun/framework-spec) 的 Rust 实现，整个 SDK 是一个 crate `xihan-framework`：原语层、清单、调用上下文、管道与操作契约，以及 cargo feature `plugin` 下的进程外插件运行时（基于 tonic）。语义与线上编码按规范的 RFC，.NET 参考实现是 [XiHan.Framework](https://github.com/XiHanFun/XiHan.Framework)。规范以 submodule 放在 `spec/`，一致性由规范仓库的向量验证：测试直接读取 `spec/conformance/vectors/`，不复制向量文件。

### 技术栈

| 技术 | 要求 | 用途 |
| --- | --- | --- |
| Rust | edition 2024，`Cargo.toml` 的 `rust-version` 是 1.85（edition 2024 的要求）；CI 用 stable 跑门禁、另以 1.85 编译库、测试与示例 | crate 源码 |
| serde（`derive`）、serde_json（`raw_value`）、serde_path_to_error、getrandom | 以 `Cargo.toml` 为准 | 线上编码、XGI 载荷与清单读写、载荷错误的 JSON 路径、追踪 ID 与端点名的随机数 |
| tonic、tonic-prost、prost、tokio、tokio-stream、hyper-util | 只在 feature `plugin`；依赖按 `rust-version` 解析到兼容的版本（见 `Cargo.toml` 的注释） | 插件运行时的 gRPC 服务端、回调客户端与异步运行时 |
| protox、tonic-prost-build、prost-build | 版本钉死在 `tools/protogen/Cargo.toml`，它的锁文件提交 | 从 `spec/plugin/plugin.proto` 生成协议代码，不需要 protoc |
| Go | `spec/conformance/go.mod` 里的版本 | 黑盒运行器 `xihan-conformance`（`tests/plugin_conformance.rs` 用 `go run` 运行） |
| rustfmt、clippy | `rustfmt.toml`（行宽 160）；clippy 以 `-D warnings` 运行 | 格式与静态检查 |
| GitHub Actions | `.github/workflows/ci.yml` | ubuntu 与 windows 矩阵的门禁、最低 Rust 版本、协议生成代码核对 |

## 开始前必须做

1. 按任务读取对应技能：
   - 原语、清单、调用上下文、管道与操作契约的设计、实现与对齐读 `.agents/skills/sdk-development/SKILL.md`；
   - 进程外插件运行时（feature `plugin`）、协议代码生成与黑盒运行器读 `.agents/skills/plugin-runtime/SKILL.md`；
   - 门禁、向量覆盖测试与打包读 `.agents/skills/testing-release/SKILL.md`；版本与发布读 `.agents/skills/version-release/SKILL.md`；
   - 整理破坏性改动读 `.agents/skills/changelog-collect/SKILL.md`；提交信息读 `.agents/skills/commit-msg/SKILL.md`；PR 读 `.agents/skills/create-pr/SKILL.md`；Issue 读 `.agents/skills/issue-reply/SKILL.md`；测试审查读 `.agents/skills/test-review/SKILL.md`。
2. 同时涉及多个职责时加载对应多个技能，不读取无关资料。
3. 检查当前分支、`git status` 和最近提交；保留用户已有改动。
4. 读相关规范章节，只读与任务相关的部分：原语 `spec/rfcs/0001-primitives.md`、清单 `spec/rfcs/0002-manifest.md` 与 `spec/manifest/manifest.schema.json`、调用上下文 `spec/rfcs/0003-invocation-context.md`、管道 `spec/rfcs/0004-pipeline-stages.md`、消息模板 `spec/rfcs/0010-localization.md`、操作 `spec/rfcs/0011-operations.md`、插件 `spec/rfcs/0017-plugin-protocol.md` 与 `spec/plugin/`、版本与一致性档 `spec/rfcs/0024-versions-and-profiles.md`、命名 `spec/conventions/`；向量在 `spec/conformance/vectors/`，档表是 `spec/conformance/profiles.json`。
5. 读目标模块（`src/<模块>/`）的模块文档与实现，以及对应的测试（`tests/<模块>.rs`、模块内的 `#[cfg(test)]`）。
6. 先找已有实现：文法判定共用的部分（字节类、`word` / `all` / `kebab` 的组合、操作 ID、接口版本、空白字符表）在 `src/grammar.rs`，各领域的文法在各自的模块里用它们拼出来：错误码 `src/primitives/error_code.rs`、消息键与区域名 `src/primitives/messages.rs`、Baggage 键 `src/primitives/baggage.rs`、清单的各个字段 `src/manifest/shape.rs`、traceparent `src/invocation/trace.rs`、主题 `src/plugin/types.rs`、端点 `src/plugin/endpoint.rs`、截止时间 `src/plugin/time.rs`；JSON 读取检查与值模型在 `src/primitives/json/`，向量读取与执行器在 `tests/common/`，不重复建设。

## 目录结构

```text
/
├─ AGENTS.md
├─ .agents/skills/                     # 按开发、插件运行时、测试发布等分类的仓库技能
├─ .claude/skills -> ../.agents/skills # Claude Code 技能兼容入口
├─ CLAUDE.md -> AGENTS.md              # Claude Code 仓库指令入口
├─ Cargo.toml                          # crate 清单：版本、rust-version、feature plugin、依赖、lints、打包排除
├─ rustfmt.toml                        # 行宽 160
├─ .github/workflows/ci.yml            # 门禁
├─ .github/workflows/release.yml       # 发布：推送 v* 标签触发，核对后发布到 crates.io
├─ scripts/release-check.sh            # 发版核对：打标签之前与 release.yml 的 guard 都跑它
├─ spec/                               # 规范（XiHanFun/framework-spec 的 submodule）：RFC、术语表、清单 Schema、插件协议、一致性向量与运行器
├─ src/
│  ├─ lib.rs                           # crate 入口、BoxFuture、两份 README 的文档测试
│  ├─ grammar.rs                       # 文法判定共用的字节类、组合与空白字符表；各领域的文法在各自的模块里
│  ├─ primitives/                      # 原语层（RFC-0001、RFC-0010）；json/ 是共同的 JSON 读取检查、值模型与 serde 读写
│  ├─ manifest/                        # 清单（RFC-0002）
│  ├─ invocation/                      # 调用上下文：取消令牌、截止时间定时器、W3C 追踪、Baggage（RFC-0003）
│  ├─ pipeline/                        # 十一阶段管道与排序（RFC-0004）
│  ├─ operation/                       # 操作契约、登记表、调用器与内置步骤（RFC-0011）
│  └─ plugin/                          # 进程外插件运行时（feature plugin，RFC-0017）；generated/ 是协议生成代码，localization/ 是 10 种区域的文案
├─ examples/echo/                      # 示例插件，也是黑盒运行器的测试对象
├─ tests/                              # 集成测试：各模块的向量回放、conformance_vectors.rs、plugin_conformance.rs；common/ 是共用的向量读取与执行器
└─ tools/protogen/                     # 协议代码生成器（独立工作区，不进 crate 包）
```

## 常用命令

在仓库根执行：

| 任务 | 命令 |
| --- | --- |
| 初始化规范 | `git submodule update --init` |
| 生成协议代码 | `cargo run --manifest-path tools/protogen/Cargo.toml` |
| 格式 | `cargo fmt --check` |
| 静态检查 | `cargo clippy --all-targets -- -D warnings`，`cargo clippy --all-targets --features plugin -- -D warnings` |
| 文档 | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`，同上加 `--features plugin` |
| 测试 | `cargo test`，`cargo test --features plugin`（后者含黑盒运行器，要 Go） |
| 最低 Rust 版本 | `cargo +1.85 test --no-run`，同上加 `--features plugin`（编译库、测试与示例，不运行；要 rustup 装有 1.85） |
| 黑盒运行器 | `cd spec/conformance && go run ./cmd/xihan-conformance plugin --dir <插件目录> --exclude <会崩溃的操作 ID> --sample '<操作 ID>=<请求 JSON>'` |
| 发版核对 | `bash scripts/release-check.sh vX.Y.Z main`（打标签之前跑，联网取 `origin/main` 与规范的远端标签） |

全部门禁按 `.github/workflows/ci.yml` 的顺序见 `.agents/skills/testing-release/SKILL.md`。

## 规范与一致性

- 规范是事实源：以 `spec/` 子模块钉的提交为准。.NET（XiHan.Framework）是参考实现，行为、错误码、消息键与英文模板与它一致。
- 向量回放：测试经 `tests/common` 与各模块的单元测试直接读 `spec/conformance/vectors/`。`tests/conformance_vectors.rs` 按 `spec/conformance/profiles.json` 里本 SDK 认领的档（`core`、`plugin`，RFC-0024）核对：认领的档要求的每个向量要么列在 `REPLAYED`（行尾注释写回放它的测试），要么列在 `GAPS` 并写理由；规范在认领的档里新增向量时它失败。缺口要同样写进两份 README 的「状态」一节与文档页的「一致性」一节。
- 规范没写到的行为不自行发明：停下来报告差异（本 SDK、.NET 与规范各是什么），提出规范文字，在 framework-spec 仓库提交（RFC 与向量），再改实现。
- 公开契约的变化顺序：framework-spec 的 RFC → XiHan.Framework 的 ADR（`plans/adr`）→ 各 SDK。SDK 仓库里不改公开契约。
- 升规范指针：
  1. 规范的提交先在 framework-spec 推送；日常分支可以指向规范已推送的任意提交（RFC-0024「实现钉规范」）。
  2. `git -C spec fetch`，`git -C spec checkout <提交>`。
  3. 重新生成协议代码 `cargo run --manifest-path tools/protogen/Cargo.toml`，生成物一并提交（CI 重新生成并核对）。
  4. 跑全部门禁；`conformance_vectors.rs` 会指出认领的档里新增的向量，`plugin_conformance.rs` 会跑到运行器的新用例（`CASES` 与运行器的用例表一致）。
  5. 只升指针时提交 `chore(spec): 规范指针升到 <短哈希>（ADR-NNNN：…）`；与实现改动同属一个闭环时并进实现的提交，说明里写「规范指针升到 <短哈希>」。
- 发预发布或正式版时 `spec/` 必须正好在规范标签上：正式版只钉规范的正式版标签，预发布可以钉 rc 标签（RFC-0024）。

## 代码规范

- 不用 unsafe：`src/lib.rs` 有 `#![forbid(unsafe_code)]`，`Cargo.toml` 的 `[lints.rust]` 有 `unsafe_code = "forbid"`。所以 Windows 命名管道只能拒绝远程客户端，设不了只含当前用户的访问控制表，隔离靠会话令牌。
- 文档注释：`missing_docs = "warn"`，CI 的 clippy 与 `cargo doc` 都以 `-D warnings` 运行，公开项缺文档即失败；仓库的习惯是私有项、常量与测试辅助类型也写中文 `///`。模块文档 `//!` 写对应的 RFC 与规范章节、回放的向量；会 panic 的公开函数在文档注释里写明 panic 条件。
- `//` 注释用中文，说明为什么（锁、状态迁移、核对顺序、归因等），句末不加「。」。
- 格式：rustfmt（`max_width = 160`、`use_small_heuristics = "Max"`）；`.editorconfig` 规定 UTF-8、LF、Rust 4 空格缩进。命名按 Rust 惯例，领域词与规范术语表（`spec/conventions/vocabulary.md`）一致，只换本语言的大小写风格；错误码与消息键常量写作 `ERROR_CODE_*`、`MESSAGE_*`。
- 错误：可恢复失败返回 `Err(Error)`（`primitives::Result<T>` 即 `Result<T, Error>`），错误码按 `<DOMAIN>_<KIND>_<NNN>`；配置期失败（清单校验、管道排序、操作登记）返回 `FrameworkError`；编程错误（不合文法的错误码、消息键、Baggage 键）panic，内容是 `FrameworkError`（`std::panic::panic_any`），并给出返回 `Result` 的 `try_*` 或 `is_*` 判断供不可信输入使用。
- 文案只引用消息键：错误带消息键与参数（`Error::validation(code, key).with_detail(…)`），自由文本错误（`Error::literal`）只在没有登记键时用；与宿主共用的内核与插件消息的英文模板逐字同向量 `kernel-messages.json`、`plugin-messages.json`，插件运行时 10 种区域的文案在 `src/plugin/localization/<区域>.json`。panic 消息、日志与测试断言用英文。
- 取消与截止时间只经 `invocation::Context` 的令牌传递；超时与取消只按截止令牌归因（`KERNEL_TIMEOUT_001` / `KERNEL_CANCELLED_001`）；调用器与内置步骤不中止处理器、不丢弃它的 future。
- 异步：中间件、处理器与管道终点返回 `BoxFuture`；feature `plugin` 之外的模块不依赖任何异步运行时，插件专用的依赖设为 `optional` 并挂在 feature `plugin` 下。
- 文法：规范里的正则都是手写的逐字节判定，整串匹配，不引入正则库；共用的字节类与组合在 `src/grammar.rs`，各领域的文法写在各自的模块里（见「开始前必须做」第 6 条），新的文法先看能不能用已有的组合拼出来。空白按 RFC-0001「空白字符」的 25 个码位（实现是 `grammar::is_whitespace`，公开为 `primitives::is_whitespace` 与 `primitives::is_blank`），不用 `char::is_whitespace`。
- JSON：线上文本用 `primitives::json::from_slice` 读，先过共同的 JSON 读取检查，不直接用 `serde_json::from_str` 读外部输入；任意 JSON 的成员用 `json::Json` 原样保留数字。
- 容量有界：登记表容量 10000（`operation::CAPACITY`），Baggage 值、消息键都有长度上限；不建无界集合。
- 没有开发依赖：测试用 `tests/common` 自写的最小执行器运行 future，借 `serde_json` 的 `RawValue` 按原文取向量；新增依赖要有明确收益并写进 `Cargo.toml` 的注释。
- `src/plugin/generated/` 是生成代码，不手改。

## 测试

- 改哪个模块，就在 `tests/<模块>.rs` 或模块内的 `#[cfg(test)]` 补测试；优先验证公开可观察行为、错误码与消息键、取消与截止时间、核对顺序和边界容量。
- 向量回放：新回放的向量加进 `tests/conformance_vectors.rs` 的 `REPLAYED`，不再回放时删掉；`plugin/` 下的向量在打开 feature `plugin` 时回放。
- 插件运行时：`tests/plugin.rs` 以扮演宿主的 gRPC 客户端核对；`tests/plugin_conformance.rs` 构建示例 echo，用黑盒运行器跑全部用例，CI 在 ubuntu 与 windows 上都跑。缺 Go 时它失败，只有设 `XIHAN_CONFORMANCE_REQUIRED=0` 才打印原因并提前返回（Rust 测试没有运行期的跳过，提前返回会记为通过）。
- 两份 README 的 Rust 示例是文档测试（`src/lib.rs` 的 `ReadmeDoctests` / `ReadmeCnDoctests`），不带 feature 也要编译；需要 feature `plugin` 的示例放模块文档或 `examples/`。
- 修复缺陷先写能复现问题的测试，再改实现；不删除测试、不放宽门禁来通过。

## 文档

- README 只放概览、安装、一个最小示例与链接；README.md（英文）与 README_cn.md（中文）同构，两份同步。
- 详细文档在 XiHan.Framework 仓库的 `docs/sdks/rust.md`（站点 <https://framework.docs.xihanfun.com/sdks/rust>）。本地克隆在 `../XiHan.Framework`，是另一个仓库，单独提交；构建用 `pnpm --dir docs build`（在那个仓库根执行），VitePress 对站内死链会构建失败。
- 文档正文不写版本号：包版本、依赖版本、规范提交哈希、测试数量都指向真源（`Cargo.toml`、`spec/` 子模块）；安装命令不写具体版本号，只有预发布版时写 `@<版本>` 占位并指向 Releases，首个正式版发布后去掉占位。
- 改公开 API 或行为时同步 crate 文档、README（涉及最小示例时）与文档页的对应章节；破坏性改动写进文档页的「破坏性改动与迁移」。

## 发布

- 版本真源是 `Cargo.toml` 的 `[package] version`；`primitives::SDK_VERSION` 取自它，`src/lib.rs` 的单元测试 `exposes_versions` 写着当前版本，改版本时一并改。
- 注册表包名是 crates.io 的 `xihan-framework`；那里最早的 `0.0.1` 是占名用的，不是本仓库发布的版本。
- 发布由 `.github/workflows/release.yml` 在推送 `v*` 标签时执行：guard 跑 `scripts/release-check.sh`（标签在 `main` 上、标签号等于 `Cargo.toml` 的 `version`、规范钉在标签上），通过后以 crates.io 可信发布（OIDC）`cargo publish`；GitHub Release 与 XiHan.Framework 一样在标签推送后手动建。打标签之前先在本地跑同一份脚本；打标签与推送只在用户明确要求时执行，步骤见 `version-release` 技能。首次发布前要用户在 crates.io 配可信发布者。
- 打包排除 `spec/`、`tests/`、`tools/`、`scripts/`、Agent 的指令与技能（`AGENTS.md`、`CLAUDE.md`、`.agents/`、`.claude/`）和仓库配置文件（`Cargo.toml` 的 `exclude`）。
- 标签写成 `v<版本>`；规范（RFC-0024）要求发版时 `spec/` 正好在规范标签上，README 的「状态」写明钉的规范版本、认领的档与缺口。

## 分支

- 日常提交在 `dev`。
- `main` 只在用户明确说「推送并快进 main」时由 `dev` 快进（`git fetch . dev:main`，不是快进就停下查）。
- CI（`.github/workflows/ci.yml`）在 `main` 与 `dev` 的 push、目标为这两个分支的 PR 上运行，也可以手动触发。
- 发版工作流（`.github/workflows/release.yml`）只在推送 `v*` 标签、或手动运行时选中版本标签时运行，见「发布」与 `version-release` 技能。

## Git 与提交

- 使用 Conventional Commits：`<type>(<scope>): <中文说明>`；scope 用模块名 `primitives`、`manifest`、`invocation`、`pipeline`、`operation`、`plugin`，或 `conformance`、`spec`、`docs`、`build`、`ci`。
- 每项能力或独立修复单独提交；实现、测试与 README 属于同一闭环时放在同一提交，文档页在 XiHan.Framework 另行提交。
- 保留用户已有和无关改动，只暂存自己的文件；不 reset、checkout 或清理它们。
- 提交前检查 `git diff --check` 与相关门禁。
- 不推送、不发布、不打标签，除非用户明确要求。

## 禁止事项

- 不用 unsafe，不降低 `Cargo.toml` 的 lints，不去掉 CI 的 `-D warnings`。
- 不手改 `src/plugin/generated/` 代替重新生成。
- 不把向量文件复制进本仓库，不在本仓库发明规范没写的行为或改公开契约。
- 不给 feature `plugin` 之外的模块引入异步运行时，不引入正则库。
- 不添加静默兜底、无限重试或无界集合。
- 不猜测 API、版本或 CI 命令；以当前检出为准。
- 不运行发布或其他破坏性命令，除非用户明确授权。
