//! 插件进程的组装与运行：读清单与环境变量、核对声明、监听并写握手行、按生命周期状态机服务到停止（`handshake.md`、`lifecycle.md`）。

use std::collections::HashSet;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;

use super::catalog::{MessageCatalog, runtime_catalog};
use super::endpoint::{self, Endpoint, Network};
use super::host::{Grants, Host, Tenants};
use super::log::{Level, Logger};
use super::messages::{ERROR_CODE_UNAVAILABLE, failed, unavailable};
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
use crate::primitives::{ERROR_CODE_INVALID_ARGUMENT, Error, FrameworkError, LifecycleState};

/// 发现时被拒的错误码（RFC-0017「错误码」）：清单不是进程外插件、写了 `contributes.pipeline`。
const ERROR_CODE_INVALID_PLUGIN: &str = "PLUGINS_VALIDATION_001";
/// 握手响应与清单不一致的错误码：声明的操作不在清单里、订阅重复或不合法。
const ERROR_CODE_MISMATCH: &str = "PLUGINS_VALIDATION_004";

/// 没有宿主给的宽限时（stdin 关闭、SIGTERM、`Stop` 的 `grace_seconds` 为 0）等在途调用的时间。
const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(5);
/// 收到停止之后最多再等 gRPC 服务端优雅关闭这么久：`Stop` 的应答要送出去，之后不让留着的连接拖住进程退出。
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(2);

/// 一个进程外插件：清单、操作、管道、订阅、消息目录与生命周期钩子；[`Plugin::serve`] 运行它。
///
/// 构建方法不核对，[`Plugin::start`]（[`Plugin::serve`] 里调用）在监听之前一次核对，不合的返回 [`FrameworkError`]：
///
/// - 清单读不出（`KERNEL_CAPABILITY_006`），不是 `kind: plugin`、`isolation: out-of-process` 或写了 `contributes.pipeline`
///   （进程外插件不能贡献中间件，RFC-0017「范围」）为 `PLUGINS_VALIDATION_001`；
/// - 登记的操作不在清单 `contributes.operations` 里、订阅的主题不合文法或（主题，类型）对重复为 `PLUGINS_VALIDATION_004`
///   （`handshake.md` §3 第 4、5 条，宿主会以同一个码拒绝握手）；
/// - 消息目录不合法或没有 `en-US`、目录名重复为 `LOCALIZATION_VALIDATION_002`；
/// - 管道排序失败为 `KERNEL_PIPELINE_00x`，环境变量缺失或不合法为 `PRIMITIVES_VALIDATION_001`。
pub struct Plugin {
    manifest: String,
    registry: Arc<Registry>,
    pipeline: pipeline::Builder,
    subscriptions: Vec<(Subscription, Arc<dyn EventHandler>)>,
    catalogs: Vec<MessageCatalog>,
    hooks: Arc<dyn Hooks>,
    network: Network,
    stop_grace: Duration,
    logger: Option<Logger>,
}

impl Plugin {
    /// 用清单的 JSON 文本创建；握手时原样交给宿主，所以必须与插件目录里的 `xihan.manifest.json` 一致（`handshake.md` §3 第 3 条）。
    pub fn new(manifest: impl Into<String>) -> Self {
        Self {
            manifest: manifest.into(),
            registry: Arc::new(Registry::new()),
            pipeline: pipeline::Builder::new(),
            subscriptions: Vec::new(),
            catalogs: Vec::new(),
            hooks: Arc::new(NoHooks),
            network: Network::platform_default(),
            stop_grace: DEFAULT_STOP_GRACE,
            logger: None,
        }
    }

    /// 读清单文件创建，通常是工作目录（即插件目录）里的 [`MANIFEST_FILE_NAME`](super::MANIFEST_FILE_NAME)；开头的 UTF-8 BOM 跳过。
    ///
    /// 文件不存在、读不出或清单不合法时返回与 [`manifest::read_file`] 相同的错误（`KERNEL_CAPABILITY_006`）。
    pub fn from_manifest_file(path: impl AsRef<Path>) -> Result<Self, FrameworkError> {
        let path = path.as_ref();
        manifest::read_file(path)?;
        let data = std::fs::read(path).map_err(|error| {
            FrameworkError::new(manifest::ERROR_CODE_INVALID_MANIFEST, format!("The manifest file '{}' could not be read.", path.display())).with_cause(error)
        })?;
        // 上面已按清单读过：文本是合法的 UTF-8
        Ok(Self::new(String::from_utf8_lossy(data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&data)).into_owned()))
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

    /// 加一个事件订阅与它的处理器。
    pub fn with_subscription(mut self, subscription: Subscription, handler: impl EventHandler) -> Self {
        self.subscriptions.push((subscription, Arc::new(handler)));
        self
    }

    /// 加一个交给宿主的消息目录；运行时自己的目录（SDK 名，10 种区域）总是附在后面。
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

    /// 宿主没有给宽限时等在途调用的时间（stdin 关闭、SIGTERM、`grace_seconds` 为 0）；缺省 5 秒。
    pub fn with_stop_grace(mut self, grace: Duration) -> Self {
        self.stop_grace = grace;
        self
    }

    /// 运行时的日志；缺省写 stderr（`invocation.md` §6），每行带 `plugin`（清单 ID）。
    pub fn with_logger(mut self, logger: Logger) -> Self {
        self.logger = Some(logger);
        self
    }

    /// 以进程外插件的身份运行（`handshake.md`）：读环境变量、监听、向 stdout 写握手行，然后服务到 `Stop` 完成、stdin 关闭或收到
    /// SIGTERM；正常停止返回 `Ok`。要在 tokio 运行时里调用。
    ///
    /// stdout 只写这一行（§2）：插件自己不要再往 stdout 写东西。
    pub async fn serve(self) -> Result<(), FrameworkError> {
        let running = self.start(Launch::from_env()?).await?;
        let line = format!("{}\n", running.handshake_line());
        let written = {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(line.as_bytes()).and_then(|()| stdout.flush())
        };
        if let Err(error) = written {
            // 宿主读不到握手行就连不上：收尾（删掉私有目录）后报错退出
            running.shutdown();
            let _ = running.wait().await;
            return Err(FrameworkError::new(ERROR_CODE_UNAVAILABLE, format!("The handshake line could not be written: {error}.")));
        }
        watch_stdin(running.runtime.exit.clone());
        #[cfg(unix)]
        watch_terminate(running.runtime.exit.clone(), running.runtime.logger.clone());
        running.wait().await
    }

    /// 核对声明、监听端点并开始服务，不写握手行、不看 stdin：[`Plugin::serve`] 用它，测试与自己管理进程的宿主也可以直接用。
    pub async fn start(self, launch: Launch) -> Result<Running, FrameworkError> {
        let manifest = manifest::read(&self.manifest)?;
        validate(&manifest, &self.registry, &self.subscriptions, &self.catalogs)?;
        let invoker = Arc::new(Invoker::new(Arc::clone(&self.registry), &self.pipeline)?);
        let logger = self.logger.unwrap_or_else(Logger::stderr).with_field("plugin", manifest.id.as_str());
        let grants = Arc::new(Grants::new(manifest.permissions.clone()));
        let tenants = Arc::new(Tenants::default());
        let host = match &launch.host_address {
            Some(address) => Some(Arc::new(
                Host::new(address.clone(), &manifest.id, &launch.session, Arc::clone(&grants), Arc::clone(&tenants))
                    .map_err(|message| FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, message))?,
            )),
            None => None,
        };
        let listener = endpoint::listen(self.network)
            .await
            .map_err(|error| FrameworkError::new(ERROR_CODE_UNAVAILABLE, format!("The plugin could not listen on a {} endpoint: {error}.", self.network)))?;

        let mut catalogs = self.catalogs;
        catalogs.push(runtime_catalog());
        let (state, _) = watch::channel(Snapshot { state: LifecycleState::Declared, error: None });
        let (calls, _) = watch::channel(Calls { open: false, active: 0 });
        let runtime = Arc::new(Runtime {
            manifest_text: self.manifest,
            manifest,
            invoker,
            subscriptions: self.subscriptions,
            catalogs,
            hooks: self.hooks,
            host,
            grants,
            tenants,
            logger,
            launch,
            stop_grace: self.stop_grace,
            state,
            calls,
            configured: AtomicBool::new(false),
            stopping: tokio::sync::Mutex::new(false),
            shutdown: CancellationToken::new(),
            exit: CancellationToken::new(),
            undeclared: Mutex::new(HashSet::new()),
        });
        runtime.report_launch(&listener.endpoint);

        // 每个 RPC（含 Handshake）先经会话令牌的拦截器，令牌不对的调用碰不到服务实现（handshake.md §4）
        let service = PluginServer::with_interceptor(PluginService::new(Arc::clone(&runtime)), SessionGuard::new(&runtime.launch.session));
        let shutdown = runtime.shutdown.clone();
        let server = tokio::spawn(tonic::transport::Server::builder().serve_with_incoming_shutdown(
            service,
            ReceiverStream::new(listener.incoming),
            shutdown.cancelled(),
        ));
        // stdin 关闭、SIGTERM 与 Running::shutdown 都只触发 exit：在这里按停止的步骤收尾
        let exiting = Arc::clone(&runtime);
        tokio::spawn(async move {
            exiting.exit.cancelled().await;
            if !exiting.shutdown.is_cancelled() {
                let grace = exiting.stop_grace;
                let _ = exiting.drain("exit", grace).await;
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
            .field("stop_grace", &self.stop_grace)
            .finish_non_exhaustive()
    }
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
        if !names.insert(catalog.name()) || catalog.name() == crate::primitives::SDK_NAME {
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
    /// 宿主回调端点。
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

    /// 设置宿主回调端点。
    pub fn with_host_address(mut self, endpoint: Endpoint) -> Self {
        self.host_address = Some(endpoint);
        self
    }

    /// 读本进程的环境变量，同 [`Launch::from_lookup`]。
    pub fn from_env() -> Result<Self, FrameworkError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// 按名字取环境变量：`XIHAN_PLUGIN_SESSION` 必填，且只能是可见 ASCII（它要放进 gRPC 元数据）；其余可以没有，有就必须合法
    /// （协议版本是逗号分隔的正整数，原语层是正整数，回调端点是 `<network> <address>`）。不合时返回 `PRIMITIVES_VALIDATION_001`。
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
        let host_address =
            present(ENV_HOST_ADDRESS).map(|text| Endpoint::parse(&text).ok_or_else(|| invalid(ENV_HOST_ADDRESS, "'<network> <address>'"))).transpose()?;
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

    /// 请求收尾退出，与 stdin 关闭相同：拒绝新调用、在宽限内等在途调用、调用停止钩子，然后停止服务。
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
        // 服务端出错退出时同样结束健康推送；收尾的任务看到服务已停止，不再排空
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

/// stdin 关闭即收尾退出（`handshake.md` §1）：独立线程读到 EOF 或出错就触发 `exit`。
///
/// 用操作系统线程而不是运行时的阻塞池：阻塞池里读不完的 stdin 会让运行时在退出时一直等它。
fn watch_stdin(exit: CancellationToken) {
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
        exit.cancel();
    });
    // 线程起不来时无法发现 stdin 关闭；宿主仍会在 Stop 超时后终止进程
    drop(spawned);
}

/// SIGTERM 与 stdin 关闭一样是立即收尾退出的请求（`lifecycle.md` §5）。
#[cfg(unix)]
fn watch_terminate(exit: CancellationToken, logger: Logger) {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut signal) => {
            tokio::spawn(async move {
                if signal.recv().await.is_some() {
                    logger.information("SIGTERM received; stopping");
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

/// 在途调用：`open` 在 `Start` 成功后打开、排空时关上，`active` 是还没结束的调用数。
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
    pub(crate) host: Option<Arc<Host>>,
    pub(crate) grants: Arc<Grants>,
    pub(crate) tenants: Arc<Tenants>,
    pub(crate) logger: Logger,
    pub(crate) launch: Launch,
    stop_grace: Duration,
    state: watch::Sender<Snapshot>,
    calls: watch::Sender<Calls>,
    configured: AtomicBool,
    /// 停止只做一次：持锁的那一个排空并调用钩子，置真之后再来的直接返回。
    stopping: tokio::sync::Mutex<bool>,
    /// 停止 gRPC 服务端，结束健康推送。
    pub(crate) shutdown: CancellationToken,
    /// 收尾退出的请求：stdin 关闭、SIGTERM、[`Running::shutdown`]。
    pub(crate) exit: CancellationToken,
    /// 已经告警过的未声明健康项，每个名字只告警一次；条目数以钩子报过的名字为上限。
    undeclared: Mutex<HashSet<String>>,
}

/// 在途调用的凭据：丢弃时计数减一，排空在计数归零时结束。
pub(crate) struct CallGuard {
    runtime: Arc<Runtime>,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        self.runtime.calls.send_modify(|calls| calls.active -= 1);
    }
}

impl Runtime {
    /// 启动时的日志与提示：宿主的原语层或协议版本与本插件不符时握手会被拒绝，先在这里记下。
    fn report_launch(&self, endpoint: &Endpoint) {
        let launch = &self.launch;
        let mut fields = vec![("endpoint", Value::from(endpoint.to_string()))];
        if let Some(instance) = &launch.instance {
            fields.push(("instance", Value::from(instance.as_str())));
        }
        if let Some(locale) = &launch.locale {
            fields.push(("locale", Value::from(locale.as_str())));
        }
        self.logger.log(Level::Information, "listening", &fields);
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

    /// 宿主没有给宽限时的停止宽限。
    pub(crate) fn stop_grace(&self) -> Duration {
        self.stop_grace
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

    /// 记下 `Configure` 已成功：停止时才调用停止钩子。
    pub(crate) fn mark_configured(&self) {
        self.configured.store(true, Ordering::Release);
    }

    /// 开始接受调用（`Start` 成功之后）。
    pub(crate) fn open_calls(&self) {
        self.calls.send_modify(|calls| calls.open = true);
    }

    /// 进入一个调用：未就绪、排空中或已失败时为 `Err(PLUGINS_UNAVAILABLE_001)`（`lifecycle.md` §2、§5）。
    pub(crate) fn enter(self: &Arc<Self>) -> Result<CallGuard, Error> {
        let state = self.state();
        // 计数与开关在同一次修改里判断：排空关上开关之后不会再有调用进来，等计数归零就等到了全部在途调用
        let entered = matches!(state, LifecycleState::Started | LifecycleState::Ready | LifecycleState::Degraded)
            && self.calls.send_if_modified(|calls| {
                if !calls.open {
                    return false;
                }
                calls.active += 1;
                true
            });
        if entered { Ok(CallGuard { runtime: Arc::clone(self) }) } else { Err(unavailable(&self.manifest.id, state)) }
    }

    /// 停止（`lifecycle.md` §5）：进入 `Draining`、拒绝新调用，在宽限内等在途调用完成，再调用停止钩子释放资源；只做一次。
    ///
    /// 返回停止钩子的结果；在途调用没有在宽限内完成只记告警。调用方随后停止服务端，进程退出。
    pub(crate) async fn drain(self: &Arc<Self>, reason: &str, grace: Duration) -> Result<(), Error> {
        let mut stopping = self.stopping.lock().await;
        if *stopping {
            return Ok(());
        }
        *stopping = true;
        let deadline = tokio::time::Instant::now() + grace;
        // 只在可服务的状态里迁到 Draining；握手之后、启动之前的探测停止（lifecycle.md §3）直接收尾
        self.transition(LifecycleState::Draining, None);
        self.calls.send_modify(|calls| calls.open = false);
        let mut calls = self.calls.subscribe();
        if tokio::time::timeout_at(deadline, calls.wait_for(|calls| calls.active == 0)).await.is_err() {
            self.logger.log(Level::Warning, "in-flight calls did not finish within the grace period", &[("reason", Value::from(reason))]);
        }
        if !self.configured.load(Ordering::Acquire) {
            return Ok(());
        }
        let hooks = Arc::clone(&self.hooks);
        let (reason_owned, remaining) = (reason.to_owned(), deadline.saturating_duration_since(tokio::time::Instant::now()));
        let mut task = tokio::spawn(async move { hooks.stop(&reason_owned, remaining).await });
        let outcome = match tokio::time::timeout_at(deadline, &mut task).await {
            Ok(joined) => self.joined("Stop", joined),
            Err(_) => {
                task.abort();
                Err(failed(&self.manifest.id, "Stop", "the stop hook did not finish within the grace period"))
            }
        };
        if let Err(error) = &outcome {
            self.log_failure("stop hook failed", error);
        }
        outcome
    }

    /// 钩子任务的结果：panic 是阶段失败（`PLUGINS_UNEXPECTED_001` + `plugins.process.failed`），原因取 panic 的内容。
    pub(crate) fn joined(&self, phase: &str, joined: Result<crate::primitives::Result<()>, tokio::task::JoinError>) -> crate::primitives::Result<()> {
        match joined {
            Ok(outcome) => outcome,
            Err(error) if error.is_panic() => Err(failed(&self.manifest.id, phase, panic_reason(error.into_panic().as_ref()))),
            Err(_) => Err(failed(&self.manifest.id, phase, "the hook was cancelled")),
        }
    }

    /// 记一条带错误码与消息的错误日志。
    pub(crate) fn log_failure(&self, message: &str, error: &Error) {
        self.logger.log(Level::Error, message, &[("code", Value::from(error.code())), ("error", Value::from(error.message()))]);
    }

    /// 当前健康（`lifecycle.md` §1、§4）：没有钩子时按运行时的状态；钩子报的 `Ready` / `Degraded` / `Failed` 在运行中按合法迁移
    /// 生效，健康项只保留清单 `contributes.health` 声明过的名字，未声明的每个名字告警一次。
    pub(crate) fn health(&self) -> Health {
        let Some(mut reported) = self.hooks.health() else {
            let snapshot = self.snapshot();
            return Health { state: snapshot.state, items: Vec::new(), error: snapshot.error };
        };
        let declared = self.manifest.health();
        reported.items.retain(|item| {
            let kept = declared.contains(&item.name);
            if !kept && self.undeclared.lock().unwrap_or_else(PoisonError::into_inner).insert(item.name.clone()) {
                self.logger.log(Level::Warning, "the health item is not declared in contributes.health", &[("item", Value::from(item.name.as_str()))]);
            }
            kept
        });
        let current = self.state();
        if matches!(current, LifecycleState::Ready | LifecycleState::Degraded)
            && matches!(reported.state, LifecycleState::Ready | LifecycleState::Degraded | LifecycleState::Failed)
            && reported.state != current
        {
            self.transition(reported.state, reported.error.clone());
        }
        let snapshot = self.snapshot();
        Health { state: snapshot.state, items: reported.items, error: reported.error.or(snapshot.error) }
    }
}

/// panic 内容的文本：`&str`、`String` 或 [`FrameworkError`]，其余写成固定的说明。
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
