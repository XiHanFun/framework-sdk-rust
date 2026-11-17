//! 插件进程的组装与运行：读清单与环境变量、核对声明、监听并写握手行、按生命周期状态机服务到停止（`handshake.md`、`lifecycle.md` §7）。

use std::collections::HashSet;
use std::fmt;
use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{OnceCell, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_stream::wrappers::ReceiverStream;

use super::catalog::MessageCatalog;
use super::endpoint::{self, Endpoint, Network, select_host_endpoint};
use super::host::{Grants, Host, Tenants};
use super::log::{Level, Logger};
use super::messages::{ERROR_CODE_UNAVAILABLE, PHASE_HEALTH, PHASE_STOP, REASON_PANIC, failed, unavailable};
use super::service::PluginService;
use super::session::SessionGuard;
use super::types::{EventHandler, Health, Hooks, NoHooks, Subscription, is_topic};
use super::v1::plugin_server::PluginServer;
use super::{ENV_HOST_ADDRESS, ENV_INSTANCE, ENV_LOCALE, ENV_PRIMITIVES, ENV_PROTOCOL_VERSIONS, ENV_SESSION, PROTOCOL_VERSIONS, negotiate};
use crate::grammar::positive;
use crate::invocation::CancellationToken;
use crate::manifest::{self, Isolation, Kind, Manifest};
use crate::operation::{Invoker, Registry};
use crate::pipeline;
use crate::primitives::{ERROR_CODE_INVALID_ARGUMENT, Error, FrameworkError, LifecycleState, SDK_NAME, SPEC_VERSION};

/// 发现时被拒的错误码（RFC-0017「错误码」）：清单不是进程外插件、写了 `contributes.pipeline`。
const ERROR_CODE_INVALID_PLUGIN: &str = "PLUGINS_VALIDATION_001";
/// 握手响应与清单不一致的错误码：原语层版本不符、声明的操作不在清单里、订阅重复或不合法。
const ERROR_CODE_MISMATCH: &str = "PLUGINS_VALIDATION_004";

/// `grace_seconds` 为 0 时等在途调用的宽限（`lifecycle.md` §7.5）。
pub(crate) const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(5);
/// stdin 关闭或收到 SIGTERM 之后到退出的时限（`lifecycle.md` §7.7）：短于宿主发出终止信号后再等的 5 秒。
const EXIT_LIMIT: Duration = Duration::from_secs(3);
/// stdin 关闭与 SIGTERM 时交给停止钩子的原因（`lifecycle.md` §7.7）。
const REASON_SHUTDOWN: &str = "shutdown";
/// 收到停止之后最多再等 gRPC 服务端优雅关闭这么久：`Stop` 的应答要送出去，之后不让留着的连接拖住进程退出。
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(2);

/// 一个进程外插件：清单、操作、管道、订阅、消息目录与生命周期钩子；[`Plugin::serve`] 运行它。
///
/// 构建方法不核对，[`Plugin::start`]（[`Plugin::serve`] 里调用）在监听与写握手行之前一次核对（`handshake.md` §1），不合的在 stderr
/// 记一条 `error` 日志、不监听，返回 [`FrameworkError`]：
///
/// - 清单读不出（`KERNEL_CAPABILITY_006`），不是 `kind: plugin`、`isolation: out-of-process` 或写了 `contributes.pipeline`
///   （进程外插件不能贡献中间件，RFC-0017「范围」）为 `PLUGINS_VALIDATION_001`；
/// - 清单的 `primitives` 不是本 SDK 实现的原语层大版本、登记的操作不在清单 `contributes.operations` 里、订阅的主题不合文法或
///   （主题，类型）对重复为 `PLUGINS_VALIDATION_004`（`handshake.md` §3，宿主会以同一个码拒绝握手）；
/// - 消息目录不合法或没有 `en-US`、目录名重复或用了运行时自己的目录名（SDK 名）为 `LOCALIZATION_VALIDATION_002`；
/// - 管道排序失败为 `KERNEL_PIPELINE_00x`，环境变量缺失或不合法为 `PRIMITIVES_VALIDATION_001`。
///
/// 健康项不在这里核对：它们由 [`Hooks::health`] 动态报出，不在清单 `contributes.health` 里的名字在上报时丢掉（`lifecycle.md` §7.4）。
pub struct Plugin {
    manifest: String,
    registry: Arc<Registry>,
    pipeline: pipeline::Builder,
    subscriptions: Vec<(Subscription, Arc<dyn EventHandler>)>,
    catalogs: Vec<MessageCatalog>,
    hooks: Arc<dyn Hooks>,
    network: Network,
    logger: Option<Logger>,
}

impl Plugin {
    /// 用清单的 JSON 文本创建；握手时原样交给宿主（开头的 BOM 去掉，`handshake.md` §3 第 3 条），所以必须与插件目录里的
    /// `xihan.manifest.json` 一致。
    pub fn new(manifest: impl Into<String>) -> Self {
        let mut manifest = manifest.into();
        if manifest.starts_with('\u{FEFF}') {
            manifest.drain(..'\u{FEFF}'.len_utf8());
        }
        Self {
            manifest,
            registry: Arc::new(Registry::new()),
            pipeline: pipeline::Builder::new(),
            subscriptions: Vec::new(),
            catalogs: Vec::new(),
            hooks: Arc::new(NoHooks),
            network: Network::platform_default(),
            logger: None,
        }
    }

    /// 读清单文件创建，通常是工作目录（即插件目录）里的 [`MANIFEST_FILE_NAME`](super::MANIFEST_FILE_NAME)；开头的 UTF-8 BOM 去掉。
    ///
    /// 文件不存在、读不出或清单不合法时返回与 [`manifest::read_file`] 相同的错误（`KERNEL_CAPABILITY_006`）。
    pub fn from_manifest_file(path: impl AsRef<Path>) -> Result<Self, FrameworkError> {
        let path = path.as_ref();
        manifest::read_file(path)?;
        let data = std::fs::read(path).map_err(|error| {
            FrameworkError::new(manifest::ERROR_CODE_INVALID_MANIFEST, format!("The manifest file '{}' could not be read.", path.display())).with_cause(error)
        })?;
        // 上面已按清单读过：文本是合法的 UTF-8
        Ok(Self::new(String::from_utf8_lossy(&data).into_owned()))
    }

    /// 插件服务的操作；每个 ID 都要在清单 `contributes.operations` 里。缺省是空的登记表。
    pub fn with_operations(mut self, registry: Arc<Registry>) -> Self {
        self.registry = registry;
        self
    }

    /// 插件进程里的管道：`Invoke` 经它与调用器的内置步骤执行。缺省是空的构建器。
    pub fn with_pipeline(mut self, builder: pipeline::Builder) -> Self {
        self.pipeline = builder;
        self
    }

    /// 加一个事件订阅与它的处理器；握手按加入的顺序声明，一次投递匹配多个订阅时也按这个顺序依次执行（`invocation.md` §4）。
    pub fn with_subscription(mut self, subscription: Subscription, handler: impl EventHandler) -> Self {
        self.subscriptions.push((subscription, Arc::new(handler)));
        self
    }

    /// 加一个随握手交给宿主的消息目录。运行时自己的消息键宿主已经有，不随握手发送（`handshake.md` §3 第 6 条）。
    pub fn with_catalog(mut self, catalog: MessageCatalog) -> Self {
        self.catalogs.push(catalog);
        self
    }

    /// 生命周期钩子。
    pub fn with_hooks(mut self, hooks: impl Hooks) -> Self {
        self.hooks = Arc::new(hooks);
        self
    }

    /// 监听的网络类型；缺省 [`Network::platform_default`]。本平台不支持的网络在 [`Plugin::start`] 时失败。
    pub fn with_network(mut self, network: Network) -> Self {
        self.network = network;
        self
    }

    /// 运行时的日志；缺省写 stderr（`invocation.md` §6），每行带 `plugin`（清单 ID）。
    pub fn with_logger(mut self, logger: Logger) -> Self {
        self.logger = Some(logger);
        self
    }

    /// 以进程外插件的身份运行（`handshake.md`）：读环境变量、核对声明、监听、向 stdout 写握手行，然后服务到 `Stop` 完成、stdin 关闭
    /// 或收到 SIGTERM；正常停止返回 `Ok`，调用方随之以退出码 0 退出。要在 tokio 运行时里调用。
    ///
    /// stdin 关闭与 SIGTERM（`lifecycle.md` §7.7）：从收到起最多 3 秒，到时不论收尾是否完成都以退出码 0 结束进程（含正在处理的
    /// `Stop`）。stdout 只写握手行这一行（§2）：插件自己不要再往 stdout 写东西。
    pub async fn serve(self) -> Result<(), FrameworkError> {
        let logger = self.logger.clone().unwrap_or_else(Logger::stderr);
        let launch = Launch::from_env().inspect_err(|error| refuse(&logger, error))?;
        let running = self.start(launch).await?;
        let line = format!("{}\n", running.handshake_line());
        let written = {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(line.as_bytes()).and_then(|()| stdout.flush())
        };
        if let Err(error) = written {
            // 宿主读不到握手行就连不上：收尾（删掉私有目录）后报错退出
            let error = FrameworkError::new(ERROR_CODE_UNAVAILABLE, format!("The handshake line could not be written: {error}."));
            refuse(&running.runtime.logger, &error);
            running.shutdown();
            let _ = running.wait().await;
            return Err(error);
        }
        let directory = running.private_directory.clone();
        #[cfg(unix)]
        watch_terminate(running.runtime.exit.clone(), running.runtime.logger.clone(), directory.clone());
        watch_stdin(running.runtime.exit.clone(), directory);
        running.wait().await
    }

    /// 核对声明、监听端点并开始服务，不写握手行、不看 stdin：[`Plugin::serve`] 用它，测试与自己管理进程的宿主也可以直接用。
    ///
    /// 不合的声明在 stderr（或 [`Plugin::with_logger`] 给的输出）记一条 `error` 日志后返回错误，不监听（`handshake.md` §1）。
    pub async fn start(self, launch: Launch) -> Result<Running, FrameworkError> {
        let logger = self.logger.clone().unwrap_or_else(Logger::stderr);
        self.open(launch, logger.clone()).await.inspect_err(|error| refuse(&logger, error))
    }

    async fn open(self, launch: Launch, logger: Logger) -> Result<Running, FrameworkError> {
        let manifest = manifest::read(&self.manifest)?;
        validate(&manifest, &self.registry, &self.subscriptions, &self.catalogs)?;
        let invoker = Arc::new(Invoker::new(Arc::clone(&self.registry), &self.pipeline)?);
        let logger = logger.with_field("plugin", manifest.id.as_str());
        let grants = Arc::new(Grants::new(manifest.permissions.clone()));
        let tenants = Arc::new(Tenants::default());
        let host = Arc::new(
            Host::new(launch.host_address.clone(), &manifest.id, &launch.session, Arc::clone(&grants), Arc::clone(&tenants))
                .map_err(|message| FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, message))?,
        );
        let listener = endpoint::listen(self.network)
            .await
            .map_err(|error| FrameworkError::new(ERROR_CODE_UNAVAILABLE, format!("The plugin could not listen on a {} endpoint: {error}.", self.network)))?;

        let (state, _) = watch::channel(Snapshot { state: LifecycleState::Declared, error: None });
        let (calls, _) = watch::channel(Calls { open: false, active: 0 });
        let runtime = Arc::new(Runtime {
            manifest_text: self.manifest,
            manifest,
            invoker,
            subscriptions: self.subscriptions,
            catalogs: self.catalogs,
            hooks: self.hooks,
            host,
            grants,
            tenants,
            logger,
            launch,
            state,
            calls,
            calls_cancel: CancellationToken::new(),
            lifecycle: tokio::sync::Mutex::new(()),
            started: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            stopped: OnceCell::new(),
            shutdown: CancellationToken::new(),
            exit: CancellationToken::new(),
            undeclared: Mutex::new(HashSet::new()),
        });
        runtime.report_launch(&listener.endpoint);

        // 每个 RPC（含 Handshake、流式调用的第一帧之前）先经会话令牌的拦截器，令牌不对的调用碰不到服务实现（handshake.md §4）
        let service = PluginServer::with_interceptor(PluginService::new(Arc::clone(&runtime)), SessionGuard::new(&runtime.launch.session));
        let shutdown = runtime.shutdown.clone();
        let server = tokio::spawn(tonic::transport::Server::builder().serve_with_incoming_shutdown(
            service,
            ReceiverStream::new(listener.incoming),
            shutdown.cancelled(),
        ));
        // stdin 关闭、SIGTERM 与 Running::shutdown 都只触发 exit：在这里按 lifecycle.md §7.7 收尾，最多 3 秒
        let exiting = Arc::clone(&runtime);
        tokio::spawn(async move {
            exiting.exit.cancelled().await;
            if !exiting.shutdown.is_cancelled() {
                if tokio::time::timeout(EXIT_LIMIT, exiting.terminate(Instant::now() + EXIT_LIMIT)).await.is_err() {
                    exiting.logger.warning("the shutdown did not finish within 3 seconds; exiting anyway");
                }
                exiting.shutdown.cancel();
            }
        });
        Ok(Running { runtime, endpoint: listener.endpoint, server, accept: listener.accept, private_directory: listener.private_directory })
    }
}

impl fmt::Debug for Plugin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Plugin")
            .field("registry", &self.registry)
            .field("subscriptions", &self.subscriptions.iter().map(|(subscription, _)| subscription).collect::<Vec<_>>())
            .field("catalogs", &self.catalogs.iter().map(MessageCatalog::name).collect::<Vec<_>>())
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

/// 拒绝启动时的 `error` 日志（`handshake.md` §1）：宿主看到的是进程在握手之前退出，原因只在 stderr。
fn refuse(logger: &Logger, error: &FrameworkError) {
    logger.log(Level::Error, "the plugin cannot start", &[("code", Value::from(error.code())), ("error", Value::from(error.to_string()))]);
}

/// 在监听之前核对声明（见 [`Plugin`]）。
fn validate(
    manifest: &Manifest,
    registry: &Registry,
    subscriptions: &[(Subscription, Arc<dyn EventHandler>)],
    catalogs: &[MessageCatalog],
) -> Result<(), FrameworkError> {
    let id = &manifest.id;
    if manifest.kind != Kind::Plugin || manifest.isolation != Some(Isolation::OutOfProcess) {
        return Err(FrameworkError::new(ERROR_CODE_INVALID_PLUGIN, format!("The manifest '{id}' must describe a plugin with the isolation 'out-of-process'.")));
    }
    if !manifest.contributes.pipeline.is_empty() {
        return Err(FrameworkError::new(
            ERROR_CODE_INVALID_PLUGIN,
            format!("The plugin '{id}' must not declare 'contributes.pipeline': only wasm plugins can contribute middleware."),
        ));
    }
    // 握手写的是清单的 primitives，宿主要求它与自己的相同；本 SDK 只实现一个原语层大版本
    if positive(SPEC_VERSION) != Some(manifest.primitives) {
        return Err(FrameworkError::new(
            ERROR_CODE_MISMATCH,
            format!("The manifest '{id}' declares the primitives version {}, but this SDK implements version {SPEC_VERSION}.", manifest.primitives),
        ));
    }
    // 登记表里的 ID 不会重复，所以这里只核对都在清单里
    if let Some(descriptor) = registry.list().into_iter().find(|descriptor| !manifest.operations().iter().any(|declared| declared == descriptor.id())) {
        return Err(FrameworkError::new(
            ERROR_CODE_MISMATCH,
            format!("The operation '{}' is not declared in 'contributes.operations' of '{id}'.", descriptor.id()),
        ));
    }
    let mut declared = HashSet::new();
    for (subscription, _) in subscriptions {
        if !is_topic(&subscription.topic) {
            return Err(FrameworkError::new(ERROR_CODE_MISMATCH, format!("The subscription topic '{}' of '{id}' is not a valid topic.", subscription.topic)));
        }
        if subscription.message_type.as_deref() == Some("") {
            return Err(FrameworkError::new(ERROR_CODE_MISMATCH, format!("The subscription '{}' of '{id}' has an empty message type.", subscription.topic)));
        }
        // 按（主题，类型）对去重而不看方式：同一对不能以两种方式各声明一次（handshake.md §3 第 5 条）
        if !declared.insert((subscription.topic.as_str(), subscription.message_type.as_deref())) {
            return Err(FrameworkError::new(
                ERROR_CODE_MISMATCH,
                format!(
                    "The subscription '{}' with the message type '{}' is declared more than once by '{id}'.",
                    subscription.topic,
                    subscription.message_type.as_deref().unwrap_or_default()
                ),
            ));
        }
    }
    let mut names = HashSet::new();
    for catalog in catalogs {
        catalog.validate()?;
        // 运行时自己的消息键宿主已经有，用 SDK 名的目录会把它们随握手再发一份（handshake.md §3 第 6 条）
        if catalog.name() == SDK_NAME {
            return Err(FrameworkError::new(
                "LOCALIZATION_VALIDATION_002",
                format!("The message catalog name '{SDK_NAME}' of '{id}' is reserved for the runtime's own messages, which the host already has."),
            ));
        }
        if !names.insert(catalog.name()) {
            return Err(FrameworkError::new(
                "LOCALIZATION_VALIDATION_002",
                format!("The message catalog name '{}' of '{id}' is used more than once.", catalog.name()),
            ));
        }
    }
    Ok(())
}

/// 宿主下发的启动参数（`handshake.md` §1 的环境变量）。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Launch {
    /// 宿主支持的协议版本，升序。
    pub protocol_versions: Vec<u32>,
    /// 宿主的原语层大版本。
    pub primitives: Option<u32>,
    /// 本次启动的会话令牌；调试输出里不显示。
    pub session: String,
    /// 回调用的宿主端点：`XIHAN_PLUGIN_HOST_ADDRESS` 里按宿主的顺序第一个 network 在 [`Network::supported`] 里的项，之后不换
    /// （`handshake.md` §4）；变量缺失、不合法或没有能连的项时为 `None`，插件照常服务，回调以 `plugins.host.unavailable` 失败。
    pub host_address: Option<Endpoint>,
    /// 实例 ID。
    pub instance: Option<String>,
    /// 宿主默认区域（BCP 47）。
    pub locale: Option<String>,
}

impl Launch {
    /// 只有会话令牌的启动参数：协议版本是本运行时支持的版本，没有回调端点。测试与自己管理进程的宿主用。
    pub fn new(session: impl Into<String>) -> Self {
        Self { protocol_versions: PROTOCOL_VERSIONS.to_vec(), primitives: None, session: session.into(), host_address: None, instance: None, locale: None }
    }

    /// 设置回调用的宿主端点。
    pub fn with_host_address(mut self, endpoint: Endpoint) -> Self {
        self.host_address = Some(endpoint);
        self
    }

    /// 读本进程的环境变量，同 [`Launch::from_lookup`]。
    pub fn from_env() -> Result<Self, FrameworkError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// 按名字取环境变量：`XIHAN_PLUGIN_SESSION` 必填，且只能是可见 ASCII（它要放进 gRPC 元数据）；协议版本与原语层可以没有，有就必须
    /// 合法（逗号分隔的正整数、正整数），不合时返回 `PRIMITIVES_VALIDATION_001`。`XIHAN_PLUGIN_HOST_ADDRESS` 按 [`select_host_endpoint`]
    /// 选回调端点，缺失或不合法不是启动错误（`handshake.md` §4）。
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, FrameworkError> {
        let invalid =
            |name: &str, expected: &str| FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, format!("The environment variable {name} must be {expected}."));
        let present = |name: &str| lookup(name).filter(|value| !value.is_empty());
        let session = present(ENV_SESSION).ok_or_else(|| invalid(ENV_SESSION, "set by the host"))?;
        if !session.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(invalid(ENV_SESSION, "visible ASCII text"));
        }
        let protocol_versions = match present(ENV_PROTOCOL_VERSIONS) {
            Some(text) => {
                text.split(',').map(positive).collect::<Option<Vec<_>>>().ok_or_else(|| invalid(ENV_PROTOCOL_VERSIONS, "a comma-separated list of versions"))?
            }
            None => Vec::new(),
        };
        let primitives = present(ENV_PRIMITIVES).map(|text| positive(&text).ok_or_else(|| invalid(ENV_PRIMITIVES, "a major version"))).transpose()?;
        let host_address = lookup(ENV_HOST_ADDRESS).and_then(|text| select_host_endpoint(&text, Network::supported()));
        Ok(Self { protocol_versions, primitives, session, host_address, instance: present(ENV_INSTANCE), locale: present(ENV_LOCALE) })
    }
}

impl fmt::Debug for Launch {
    // 会话令牌不写进调试输出
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Launch")
            .field("protocol_versions", &self.protocol_versions)
            .field("primitives", &self.primitives)
            .field("host_address", &self.host_address)
            .field("instance", &self.instance)
            .field("locale", &self.locale)
            .finish_non_exhaustive()
    }
}

/// 正在服务的插件：[`Plugin::start`] 返回，[`Running::wait`] 等到停止。
pub struct Running {
    runtime: Arc<Runtime>,
    endpoint: Endpoint,
    server: JoinHandle<Result<(), tonic::transport::Error>>,
    accept: JoinHandle<()>,
    private_directory: Option<PathBuf>,
}

impl Running {
    /// 插件监听的端点。
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// 握手行，不含结尾的 `\n`（`handshake.md` §2）。
    pub fn handshake_line(&self) -> String {
        self.endpoint.handshake_line()
    }

    /// 当前的生命周期状态。
    pub fn state(&self) -> LifecycleState {
        self.runtime.state()
    }

    /// 请求收尾退出，与 stdin 关闭相同（`lifecycle.md` §7.7）：可服务的插件进入 `Draining`、立即触发全部在途调用的取消令牌并等它们
    /// 结束，启动成功过的调一次停止钩子（`reason` 为 `shutdown`），最多 3 秒，然后停止服务。
    pub fn shutdown(&self) {
        self.runtime.exit.cancel();
    }

    /// 等到服务停止（`Stop` 完成、stdin 关闭或 [`Running::shutdown`]），删掉 Unix 域套接字的私有目录；状态是 `Draining` 时记为 `Stopped`。
    pub async fn wait(self) -> Result<(), FrameworkError> {
        let Self { runtime, server, accept, private_directory, .. } = self;
        let mut server = server;
        let limit = runtime.shutdown.clone();
        let outcome = tokio::select! {
            outcome = &mut server => outcome,
            () = async move { limit.cancelled().await; tokio::time::sleep(SHUTDOWN_LIMIT).await } => {
                // 优雅关闭没有在时限内完成（有连接迟迟不关）：直接停掉服务端
                server.abort();
                runtime.logger.warning("the gRPC server did not shut down in time; stopping it");
                Ok(Ok(()))
            }
        };
        accept.abort();
        // 服务端出错退出时同样结束健康推送；收尾的任务看到服务已停止，不再收尾
        runtime.shutdown.cancel();
        runtime.exit.cancel();
        if let Some(directory) = private_directory {
            let _ = std::fs::remove_dir_all(directory);
        }
        runtime.transition(LifecycleState::Stopped, None);
        runtime.logger.information("stopped");
        match outcome {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(FrameworkError::new(ERROR_CODE_UNAVAILABLE, format!("The plugin server failed: {error}.")).with_cause(error)),
            Err(error) => Err(FrameworkError::new(ERROR_CODE_UNAVAILABLE, format!("The plugin server stopped abnormally: {error}."))),
        }
    }
}

impl fmt::Debug for Running {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Running").field("endpoint", &self.endpoint).field("state", &self.state()).finish_non_exhaustive()
    }
}

/// 收尾请求的 3 秒时限（`lifecycle.md` §7.7）：在操作系统线程里计时，到时删掉 Unix 域套接字的私有目录、以退出码 0 结束进程，不论
/// 运行时的任务是否还卡着（单线程运行时里阻塞的处理器会连带卡住运行时自己的定时器）。只在 [`Plugin::serve`] 里用：它代表整个插件进程。
fn exit_after_limit(directory: Option<PathBuf>) {
    let spawned = std::thread::Builder::new().name("xihan-plugin-exit".to_owned()).spawn(move || {
        std::thread::sleep(EXIT_LIMIT);
        if let Some(directory) = directory {
            let _ = std::fs::remove_dir_all(directory);
        }
        std::process::exit(0);
    });
    // 线程起不来时只剩运行时里的收尾；宿主仍会在 5 秒后强制终止进程
    drop(spawned);
}

/// stdin 关闭即收尾退出（`handshake.md` §1、`lifecycle.md` §7.7）：独立线程读到 EOF 或出错就触发 `exit`，并开始 3 秒的时限。
///
/// 用操作系统线程而不是运行时的阻塞池：阻塞池里读不完的 stdin 会让运行时在退出时一直等它。
fn watch_stdin(exit: CancellationToken, directory: Option<PathBuf>) {
    let spawned = std::thread::Builder::new().name("xihan-plugin-stdin".to_owned()).spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = [0u8; 1024];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        exit_after_limit(directory);
        exit.cancel();
    });
    // 线程起不来时无法发现 stdin 关闭；宿主仍会在 Stop 超时后终止进程
    drop(spawned);
}

/// SIGTERM 与 stdin 关闭一样是立即收尾退出的请求（`lifecycle.md` §5、§7.7）。
#[cfg(unix)]
fn watch_terminate(exit: CancellationToken, logger: Logger, directory: Option<PathBuf>) {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut signal) => {
            tokio::spawn(async move {
                if signal.recv().await.is_some() {
                    logger.information("SIGTERM received; stopping");
                    exit_after_limit(directory);
                    exit.cancel();
                }
            });
        }
        Err(error) => logger.log(Level::Warning, "SIGTERM cannot be watched", &[("reason", Value::from(error.to_string()))]),
    }
}

/// 生命周期状态与失败原因。
#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub(crate) state: LifecycleState,
    pub(crate) error: Option<Error>,
}

/// 在途调用：`open` 在插件可服务后打开、停止或收尾时关上，`active` 是还没结束的调用与投递数。
#[derive(Clone, Copy, Debug)]
struct Calls {
    open: bool,
    active: usize,
}

/// 一次运行的共享状态。
pub(crate) struct Runtime {
    pub(crate) manifest: Manifest,
    pub(crate) manifest_text: String,
    pub(crate) invoker: Arc<Invoker>,
    pub(crate) subscriptions: Vec<(Subscription, Arc<dyn EventHandler>)>,
    pub(crate) catalogs: Vec<MessageCatalog>,
    pub(crate) hooks: Arc<dyn Hooks>,
    pub(crate) host: Arc<Host>,
    pub(crate) grants: Arc<Grants>,
    pub(crate) tenants: Arc<Tenants>,
    pub(crate) logger: Logger,
    pub(crate) launch: Launch,
    state: watch::Sender<Snapshot>,
    calls: watch::Sender<Calls>,
    /// 全部在途调用的上下文令牌的父令牌：`Stop` 的宽限到了、或收到收尾请求时触发（RFC-0003，只经令牌通知，不中止处理器）。
    calls_cancel: CancellationToken,
    /// `Handshake`、`Configure`、`Start` 与 `Stop` 的次序核对和迁移串行执行（`lifecycle.md` §7.1）：重复的同一 RPC 在前一个结束之后才看状态。
    pub(crate) lifecycle: tokio::sync::Mutex<()>,
    /// 启动钩子成功过：只有这时才调停止钩子（`lifecycle.md` §7.5）。
    pub(crate) started: AtomicBool,
    /// `Stop` 或收尾已经开始：再来的 `Stop` 以 `FailedPrecondition` 拒绝，不再开放调用。
    stopping: AtomicBool,
    /// 停止钩子的结果：进程里最多调一次，`Stop` 与收尾同时要时共用这一次的结果。
    stopped: OnceCell<crate::primitives::Result<()>>,
    /// 停止 gRPC 服务端，结束健康推送。
    pub(crate) shutdown: CancellationToken,
    /// 收尾退出的请求：stdin 关闭、SIGTERM、[`Running::shutdown`]。
    pub(crate) exit: CancellationToken,
    /// 已经告警过的未声明健康项，每个名字只告警一次；条目数以钩子报过的名字为上限。
    undeclared: Mutex<HashSet<String>>,
}

/// 在途调用的凭据：丢弃时计数减一，等在途调用在计数归零时结束。
pub(crate) struct CallGuard {
    runtime: Arc<Runtime>,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        self.runtime.calls.send_modify(|calls| calls.active -= 1);
    }
}

impl Runtime {
    /// 启动时的日志与提示：宿主的原语层或协议版本与本插件不符时握手会被拒绝，没有能连的回调端点时回调都会失败，先在这里记下。
    fn report_launch(&self, endpoint: &Endpoint) {
        let launch = &self.launch;
        let mut fields = vec![("endpoint", Value::from(endpoint.to_string()))];
        if let Some(host) = self.host.endpoint() {
            fields.push(("host", Value::from(host.to_string())));
        }
        if let Some(instance) = &launch.instance {
            fields.push(("instance", Value::from(instance.as_str())));
        }
        if let Some(locale) = &launch.locale {
            fields.push(("locale", Value::from(locale.as_str())));
        }
        self.logger.log(Level::Information, "listening", &fields);
        if self.host.endpoint().is_none() {
            // handshake.md §4：照常服务，启动时记一条 warning，回调都以 plugins.host.unavailable 失败、不发请求
            let supported: Vec<Value> = Network::supported().iter().map(|network| Value::from(network.as_str())).collect();
            self.logger.log(
                Level::Warning,
                "XIHAN_PLUGIN_HOST_ADDRESS has no supported callback endpoint; callbacks to the host will fail",
                &[("supported", Value::from(supported))],
            );
        }
        if launch.primitives.is_some_and(|primitives| primitives != self.manifest.primitives) {
            self.logger.log(
                Level::Warning,
                "the host's primitives version differs from the manifest; the host will reject the handshake",
                &[("host", Value::from(launch.primitives)), ("plugin", Value::from(self.manifest.primitives))],
            );
        }
        if !launch.protocol_versions.is_empty() && negotiate(&launch.protocol_versions, PROTOCOL_VERSIONS).is_none() {
            self.logger.log(
                Level::Warning,
                "the host supports no protocol version of this plugin; the handshake will be refused",
                &[("host", Value::from(launch.protocol_versions.clone())), ("plugin", Value::from(PROTOCOL_VERSIONS.to_vec()))],
            );
        }
    }

    /// 当前的生命周期状态。
    pub(crate) fn state(&self) -> LifecycleState {
        self.state.borrow().state
    }

    /// 当前状态与失败原因。
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.state.borrow().clone()
    }

    /// 订阅状态的变化（健康推送用）。
    pub(crate) fn watch_state(&self) -> watch::Receiver<Snapshot> {
        self.state.subscribe()
    }

    /// 按合法迁移（原语层的迁移表，`lifecycle.md` §1）改状态；不合法时不改、返回假。
    pub(crate) fn transition(&self, to: LifecycleState, error: Option<Error>) -> bool {
        self.state.send_if_modified(|snapshot| {
            if !snapshot.state.can_transition_to(to) {
                return false;
            }
            snapshot.state = to;
            snapshot.error = error;
            true
        })
    }

    /// 插件可服务之后开始接受调用；`Stop` 或收尾已经开始时不再开放。
    pub(crate) fn open_calls(&self) {
        if !self.stopping.load(Ordering::Acquire) {
            self.calls.send_modify(|calls| calls.open = true);
        }
    }

    /// 一次调用或投递的取消令牌：宿主取消这次 RPC、`Stop` 的宽限到了或收到收尾请求时触发。
    pub(crate) fn call_token(&self) -> CancellationToken {
        self.calls_cancel.child_token()
    }

    /// 进入一个调用：只有 `Ready` 与 `Degraded` 可服务（`lifecycle.md` §7.3），其余状态为 `PLUGINS_UNAVAILABLE_001` +
    /// `plugins.process.unavailable`（`state` 是此刻的状态名）。
    pub(crate) fn enter(self: &Arc<Self>) -> Result<CallGuard, Error> {
        let state = self.state();
        // 计数与开关在同一次修改里判断：停止关上开关之后不会再有调用进来，等计数归零就等到了全部在途调用
        let entered = state.is_serving()
            && self.calls.send_if_modified(|calls| {
                if !calls.open {
                    return false;
                }
                calls.active += 1;
                true
            });
        if entered { Ok(CallGuard { runtime: Arc::clone(self) }) } else { Err(unavailable(&self.manifest.id, state)) }
    }

    /// 等全部在途调用结束。
    async fn idle(&self) {
        let mut calls = self.calls.subscribe();
        let _ = calls.wait_for(|calls| calls.active == 0).await;
    }

    /// 受理一次 `Stop`（`lifecycle.md` §7.1、§7.5）：`Declared`、`Draining`、`Stopped` 与已经开始停止时为假，调用方以
    /// `FailedPrecondition` 拒绝；否则可服务的（含 `Started`）进入 `Draining`，不再接受新调用。调用方持有次序锁。
    pub(crate) fn accept_stop(&self) -> bool {
        let state = self.state();
        if matches!(state, LifecycleState::Declared | LifecycleState::Draining | LifecycleState::Stopped) || self.stopping.swap(true, Ordering::AcqRel) {
            return false;
        }
        if matches!(state, LifecycleState::Started | LifecycleState::Ready | LifecycleState::Degraded) {
            self.transition(LifecycleState::Draining, None);
        }
        self.calls.send_modify(|calls| calls.open = false);
        true
    }

    /// 受理之后的停止（`lifecycle.md` §7.5）：启动钩子没成功过的直接返回 `Ok`；否则等在途调用结束，最长到宽限，宽限到了触发它们的
    /// 取消令牌、不再等，然后调停止钩子（`reason` 原样交给作者）。返回停止钩子的结果；调用方应答之后停止服务，进程以退出码 0 退出。
    pub(crate) async fn stop(&self, reason: &str, grace: Duration) -> crate::primitives::Result<()> {
        if !self.started.load(Ordering::Acquire) {
            return Ok(());
        }
        let deadline = Instant::now() + grace;
        if tokio::time::timeout_at(deadline, self.idle()).await.is_err() {
            self.calls_cancel.cancel();
            self.logger.log(Level::Warning, "in-flight calls did not finish within the grace period; they are cancelled", &[("reason", Value::from(reason))]);
        }
        self.stop_hook(reason, deadline.saturating_duration_since(Instant::now())).await
    }

    /// 收尾（stdin 关闭、SIGTERM，`lifecycle.md` §7.7）：可服务的进入 `Draining`（其他状态不迁移），立即触发全部在途调用的取消令牌
    /// 并等它们结束；启动钩子成功过、停止钩子还没调过的调一次（`reason` 为 `shutdown`）。时限由调用方计（3 秒），不拿次序锁：卡住的
    /// 配置或启动钩子拿着它。
    async fn terminate(&self, limit: Instant) {
        self.stopping.store(true, Ordering::Release);
        if self.state().is_serving() {
            self.transition(LifecycleState::Draining, None);
        }
        self.calls.send_modify(|calls| calls.open = false);
        self.calls_cancel.cancel();
        self.idle().await;
        if self.started.load(Ordering::Acquire) {
            let _ = self.stop_hook(REASON_SHUTDOWN, limit.saturating_duration_since(Instant::now())).await;
        }
    }

    /// 调停止钩子，进程里最多一次：再来的（`Stop` 与收尾同时）等同一次的结果。失败原样返回，panic 为 `PLUGINS_UNEXPECTED_001` +
    /// `plugins.process.failed`（`phase` 为 `stop`，`reason` 为 `panic`）。
    async fn stop_hook(&self, reason: &str, grace: Duration) -> crate::primitives::Result<()> {
        self.stopped
            .get_or_init(|| async {
                let (hooks, reason) = (Arc::clone(&self.hooks), reason.to_owned());
                let outcome = self.joined(PHASE_STOP, tokio::spawn(async move { hooks.stop(&reason, grace).await }).await);
                if let Err(error) = &outcome {
                    self.log_failure("the stop hook failed", error);
                }
                outcome
            })
            .await
            .clone()
    }

    /// 钩子任务的结果（`lifecycle.md` §7.6）：panic 为这个阶段的失败，`reason` 是 `panic`，panic 的内容只写进 stderr。
    pub(crate) fn joined(&self, phase: &str, joined: Result<crate::primitives::Result<()>, tokio::task::JoinError>) -> crate::primitives::Result<()> {
        match joined {
            Ok(outcome) => outcome,
            Err(error) if error.is_panic() => {
                let text = panic_reason(error.into_panic().as_ref());
                self.logger.log(Level::Error, "a lifecycle hook panicked", &[("phase", Value::from(phase)), ("panic", Value::from(text))]);
                Err(failed(&self.manifest.id, phase, REASON_PANIC))
            }
            // 钩子任务只会在运行时关闭时被取消
            Err(_) => Err(failed(&self.manifest.id, phase, "cancelled")),
        }
    }

    /// 记一条带错误码与消息的错误日志。
    pub(crate) fn log_failure(&self, message: &str, error: &Error) {
        self.logger.log(Level::Error, message, &[("code", Value::from(error.code())), ("error", Value::from(error.message()))]);
    }

    /// `Start` 之后的就绪（`lifecycle.md` §7.3）：有健康回调时求一次，按结果进入 `Ready`、`Degraded` 或 `Failed`（报其他状态的按
    /// `Ready`，钩子 panic 的按这一次的结果 `Degraded`）；没有健康回调即进入 `Ready`。
    pub(crate) fn become_ready(&self) {
        let (state, error) = match self.ask_health() {
            None => (LifecycleState::Ready, None),
            Some(reported) => match reported.state {
                LifecycleState::Degraded | LifecycleState::Failed => (reported.state, reported.error),
                _ => (LifecycleState::Ready, reported.error),
            },
        };
        self.transition(state, error);
        if self.state().is_serving() {
            self.open_calls();
        }
    }

    /// 当前健康（`lifecycle.md` §7.4）：只有 `Ready` 与 `Degraded` 期间求健康回调，报的 `Ready`、`Degraded`、`Failed` 照办，其他状态
    /// 不改变状态；其余状态报本地状态与失败原因，没有健康项、不调回调。
    pub(crate) fn health(&self) -> Health {
        let snapshot = self.snapshot();
        if !snapshot.state.is_serving() {
            return Health { state: snapshot.state, items: Vec::new(), error: snapshot.error };
        }
        let Some(reported) = self.ask_health() else {
            return Health { state: snapshot.state, items: Vec::new(), error: snapshot.error };
        };
        // 钩子 panic 时这一次报 Degraded，状态不变：只有回调报的状态改变状态
        if reported.panicked {
            return Health { state: LifecycleState::Degraded, items: Vec::new(), error: reported.error };
        }
        if matches!(reported.state, LifecycleState::Ready | LifecycleState::Degraded | LifecycleState::Failed) && reported.state != snapshot.state {
            // 迁移不合法（状态已经变了，如刚进入 Draining）时不改
            self.transition(reported.state, reported.error.clone());
        }
        let state = self.state();
        Health { state, items: reported.items, error: reported.error.or(snapshot.error) }
    }

    /// 求一次健康回调：没有回调为 `None`；健康项只留清单 `contributes.health` 声明过的，未声明的名字在进程里只告警一次；回调 panic
    /// 时是 `Degraded`、`PLUGINS_UNEXPECTED_001` + `plugins.process.failed`（`phase` 为 `health`）、没有健康项。
    fn ask_health(&self) -> Option<Reported> {
        // 钩子是同步的：panic 之后只丢弃它的返回值，不再用它里面的状态
        let reported = match catch_unwind(AssertUnwindSafe(|| self.hooks.health())) {
            Ok(reported) => reported?,
            Err(payload) => {
                let text = panic_reason(payload.as_ref());
                self.logger.log(Level::Error, "a lifecycle hook panicked", &[("phase", Value::from(PHASE_HEALTH)), ("panic", Value::from(text))]);
                let error = failed(&self.manifest.id, PHASE_HEALTH, REASON_PANIC);
                return Some(Reported { state: LifecycleState::Degraded, items: Vec::new(), error: Some(error), panicked: true });
            }
        };
        let declared = self.manifest.health();
        let mut items = reported.items;
        items.retain(|item| {
            let kept = declared.contains(&item.name);
            if !kept && self.undeclared.lock().unwrap_or_else(PoisonError::into_inner).insert(item.name.clone()) {
                self.logger.log(Level::Warning, "the health item is not declared in contributes.health", &[("item", Value::from(item.name.as_str()))]);
            }
            kept
        });
        Some(Reported { state: reported.state, items, error: reported.error, panicked: false })
    }
}

/// 健康回调的一次结果，健康项已按清单过滤。
struct Reported {
    state: LifecycleState,
    items: Vec<super::types::HealthItem>,
    error: Option<Error>,
    panicked: bool,
}

/// panic 内容的文本（只写进 stderr 日志）：`&str`、`String` 或 [`FrameworkError`]，其余写成固定的说明。
pub(crate) fn panic_reason(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_owned();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    if let Some(error) = payload.downcast_ref::<FrameworkError>() {
        return error.to_string();
    }
    "the hook panicked".to_owned()
}
