//! 进程外插件运行时（RFC-0017）：回放向量 `plugin/handshake-lines.json` 与 `plugin/protocol-versions.json`，再以扮演宿主的
//! gRPC 客户端核对会话令牌、生命周期、调用与上下文还原、错误编码、事件投递、回调的权限与租户、排空与停止。

mod common;

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tonic::codegen::http::Uri;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use tonic::{Code, Request, Response, Status};
use xihan_framework::BoxFuture;
use xihan_framework::invocation::Context;
use xihan_framework::manifest::{self, Isolation, Kind};
use xihan_framework::operation::{self, Contract, EXPOSE_HTTP, Registry, Schema};
use xihan_framework::pipeline;
use xihan_framework::plugin::v1::host_server::{Host as HostRpc, HostServer};
use xihan_framework::plugin::v1::plugin_client::PluginClient;
use xihan_framework::plugin::{
    self, Configuration, Endpoint, Envelope, Event, Health, HealthItem, HealthStatus, Hooks, Launch, Logger, MessageCatalog, Network, Plugin, Running,
    Subscription, SubscriptionMode, event_handler_fn, v1,
};
use xihan_framework::primitives::{BAGGAGE_TENANT, Error, LifecycleState, Result};

/// 测试里宿主下发的会话令牌。
const TOKEN: &str = "c2Vzc2lvbi10b2tlbi1mb3ItdGhlLXRlc3RzLW9ubHk";

/// 被测插件的清单。
const MANIFEST: &str = r#"{
  "$schema": "https://spec.xihanfun.com/manifest/v1.json",
  "id": "xihan.tests.orders",
  "version": "1.0.0",
  "primitives": "1",
  "stability": "experimental",
  "kind": "plugin",
  "isolation": "out-of-process",
  "contributes": {
    "operations": [ "orders.create", "orders.slow", "orders.relay", "orders.publish", "orders.panic" ],
    "config": { "section": "XiHan:Orders" },
    "health": [ "main" ]
  },
  "permissions": [ "capability:kernel.pipeline", "capability:messaging.queue" ],
  "entry": "orders"
}"#;

// ---------------------------------------------------------------------------
// 向量与启动参数
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct HandshakeLines {
    valid: Vec<ValidLine>,
    invalid: Vec<String>,
}

#[derive(Deserialize)]
struct ValidLine {
    line: String,
    network: String,
    address: String,
}

#[test]
fn handshake_lines_follow_the_vectors() {
    let vectors: HandshakeLines = common::vector("plugin/handshake-lines.json");
    for case in &vectors.valid {
        let endpoint = plugin::parse_handshake_line(&case.line).unwrap_or_else(|| panic!("{}", case.line));
        assert_eq!((endpoint.network().as_str(), endpoint.address()), (case.network.as_str(), case.address.as_str()), "{}", case.line);
        assert_eq!(endpoint.handshake_line(), case.line);
    }
    for line in &vectors.invalid {
        assert_eq!(plugin::parse_handshake_line(line), None, "{line:?}");
    }
}

#[derive(Deserialize)]
struct ProtocolVersions {
    cases: Vec<VersionCase>,
}

#[derive(Deserialize)]
struct VersionCase {
    host: Vec<u32>,
    plugin: Vec<u32>,
    selected: Option<u32>,
    error: Option<String>,
}

#[test]
fn protocol_versions_follow_the_vectors() {
    let vectors: ProtocolVersions = common::vector("plugin/protocol-versions.json");
    for case in &vectors.cases {
        let selected = plugin::negotiate(&case.host, &case.plugin);
        assert_eq!(selected, case.selected, "{:?} {:?}", case.host, case.plugin);
        assert_eq!(selected.is_none(), case.error.as_deref() == Some("PLUGINS_VALIDATION_003"));
    }
}

#[test]
fn launch_reads_the_environment() {
    let environment = |pairs: &[(&str, &str)]| {
        let map: HashMap<String, String> = pairs.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
        Launch::from_lookup(move |name| map.get(name).cloned())
    };
    let launch = environment(&[
        (plugin::ENV_SESSION, TOKEN),
        (plugin::ENV_PROTOCOL_VERSIONS, "1,2"),
        (plugin::ENV_PRIMITIVES, "1"),
        (plugin::ENV_HOST_ADDRESS, "tcp 127.0.0.1:5000"),
        (plugin::ENV_INSTANCE, "orders#1"),
        (plugin::ENV_LOCALE, "zh-CN"),
    ])
    .unwrap();
    assert_eq!((launch.protocol_versions.as_slice(), launch.primitives, launch.session.as_str()), (&[1, 2][..], Some(1), TOKEN));
    assert_eq!(launch.host_address, Some(Endpoint::new(Network::Tcp, "127.0.0.1:5000")));
    assert_eq!((launch.instance.as_deref(), launch.locale.as_deref()), (Some("orders#1"), Some("zh-CN")));
    assert!(!format!("{launch:?}").contains(TOKEN), "the session token stays out of debug output");

    for pairs in [
        &[][..],
        &[(plugin::ENV_SESSION, "")],
        &[(plugin::ENV_SESSION, "token with spaces")],
        &[(plugin::ENV_SESSION, TOKEN), (plugin::ENV_PROTOCOL_VERSIONS, "1,x")],
        &[(plugin::ENV_SESSION, TOKEN), (plugin::ENV_PROTOCOL_VERSIONS, "01")],
        &[(plugin::ENV_SESSION, TOKEN), (plugin::ENV_PRIMITIVES, "one")],
        &[(plugin::ENV_SESSION, TOKEN), (plugin::ENV_HOST_ADDRESS, "tcp 10.0.0.1:5000")],
    ] {
        assert_eq!(environment(pairs).unwrap_err().code(), "PRIMITIVES_VALIDATION_001", "{pairs:?}");
    }
}

#[test]
fn the_echo_manifest_describes_an_out_of_process_plugin() {
    let echo = manifest::read(include_str!("../examples/echo/xihan.manifest.json")).unwrap();
    assert_eq!((echo.kind, echo.isolation, echo.entry.as_deref()), (Kind::Plugin, Some(Isolation::OutOfProcess), Some("echo")));
    assert!(echo.operations().iter().any(|id| id == "echoes.crash"));
}

// ---------------------------------------------------------------------------
// 被测插件
// ---------------------------------------------------------------------------

/// 插件这一侧记下的事。
#[derive(Default)]
struct Recorder {
    configuration: Mutex<Option<(String, Vec<String>, PathBuf)>>,
    deliveries: Mutex<Vec<String>>,
    stops: Mutex<Vec<String>>,
}

struct TestHooks(Arc<Recorder>);

impl Hooks for TestHooks {
    fn configure<'a>(&'a self, configuration: &'a Configuration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let section: Value = configuration.decode().map_err(|_| Error::validation("ORDERS_VALIDATION_009", "orders.configuration.invalid"))?;
            if section.get("fail").is_some() {
                return Err(Error::validation("ORDERS_VALIDATION_009", "orders.configuration.invalid"));
            }
            *self.0.configuration.lock().unwrap() =
                Some((configuration.section.clone(), configuration.granted_permissions.clone(), configuration.work_directory.clone()));
            Ok(())
        })
    }

    fn stop<'a>(&'a self, reason: &'a str, _: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.0.stops.lock().unwrap().push(reason.to_owned());
            Ok(())
        })
    }

    fn health(&self) -> Option<Health> {
        Some(
            Health::new(LifecycleState::Ready)
                .with_item(HealthItem::new("main", HealthStatus::Healthy).with_description("all good"))
                .with_item(HealthItem::new("ghost", HealthStatus::Unhealthy)),
        )
    }
}

#[derive(Clone, Deserialize)]
struct Create {
    quantity: u32,
}

#[derive(Serialize)]
struct Created {
    quantity: u32,
    tenant: Option<String>,
    principal: Option<String>,
    correlation: Option<String>,
    unregistered: Option<String>,
    trace_id: String,
    span_id: String,
    deadline: bool,
}

#[derive(Deserialize)]
struct Slow {
    millis: u64,
}

#[derive(Serialize)]
struct Done {
    cancelled: bool,
}

#[derive(Deserialize)]
struct Relay {
    operation: String,
    tenant: Option<String>,
}

#[derive(Serialize)]
struct Relayed {
    payload: String,
}

#[derive(Deserialize)]
struct Publish {
    tenants: Vec<Option<String>>,
}

#[derive(Serialize)]
struct Published {
    outcomes: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Empty {}

/// 处理器里的 panic：调用器的 Observe 阶段把它翻译成未处理失败。
fn explode() -> Result<Empty> {
    panic!("boom")
}

/// 测试用的插件：在回环 TCP 上监听，日志写进 `log`。
fn orders_plugin(recorder: &Arc<Recorder>, log: &Buffer) -> Plugin {
    let registry = Arc::new(Registry::new());
    registry
        .add_with(
            Contract::new("orders.create").with_expose([EXPOSE_HTTP]).with_permission("orders.write"),
            operation::handler_fn(|ctx: &Context, request: Create| {
                Box::pin(async move {
                    if request.quantity == 0 {
                        return Err(Error::validation("ORDERS_VALIDATION_001", "orders.quantity.invalid").with_detail("quantity", 0));
                    }
                    Ok(Created {
                        quantity: request.quantity,
                        tenant: ctx.tenant().map(str::to_owned),
                        principal: ctx.principal().map(str::to_owned),
                        correlation: ctx.correlation().map(str::to_owned),
                        unregistered: ctx.baggage().get("xihan.unregistered").cloned(),
                        trace_id: ctx.trace_id().to_owned(),
                        span_id: ctx.span_id().to_owned(),
                        deadline: ctx.has_deadline(),
                    })
                })
            }),
            Some(Schema::new(r#"{"type":"object","required":["quantity"]}"#, r#"{"type":"object"}"#)),
            pipeline::APP_OWNER,
        )
        .unwrap();
    registry
        .add(
            Contract::new("orders.slow"),
            operation::handler_fn(|ctx: &Context, request: Slow| {
                Box::pin(async move {
                    // 宿主取消时令牌触发，处理器照常返回自己的结果（RFC-0003 规则 1）
                    let cancelled = tokio::select! {
                        () = tokio::time::sleep(Duration::from_millis(request.millis)) => false,
                        () = ctx.done() => true,
                    };
                    Ok(Done { cancelled })
                })
            }),
        )
        .unwrap();
    registry
        .add(
            Contract::new("orders.relay"),
            operation::handler_fn(|ctx: &Context, request: Relay| {
                Box::pin(async move {
                    let host = plugin::host(ctx).ok_or_else(|| Error::unavailable("ORDERS_UNAVAILABLE_001", "orders.host.missing"))?;
                    let ctx = match request.tenant {
                        Some(tenant) => ctx.with_baggage(BAGGAGE_TENANT, tenant),
                        None => ctx.clone(),
                    };
                    let payload = host.invoke(&ctx, &request.operation, "{}").await?;
                    Ok(Relayed { payload: String::from_utf8(payload).unwrap() })
                })
            }),
        )
        .unwrap();
    registry
        .add(
            Contract::new("orders.publish"),
            operation::handler_fn(|ctx: &Context, request: Publish| {
                Box::pin(async move {
                    let host = plugin::host(ctx).ok_or_else(|| Error::unavailable("ORDERS_UNAVAILABLE_001", "orders.host.missing"))?;
                    let envelopes = request
                        .tenants
                        .into_iter()
                        .enumerate()
                        .map(|(index, tenant)| {
                            let envelope = Envelope::new("orders.events", format!("e{index}"), "urn:test", "orders.created");
                            match tenant {
                                Some(tenant) => envelope.with_extension(plugin::TENANT_EXTENSION, tenant),
                                None => envelope,
                            }
                        })
                        .collect();
                    let outcomes = host.publish(ctx, envelopes).await;
                    Ok(Published {
                        outcomes: outcomes.into_iter().map(|outcome| outcome.err().map_or("ok".to_owned(), |error| error.code().to_owned())).collect(),
                    })
                })
            }),
        )
        .unwrap();
    registry.add(Contract::new("orders.panic"), operation::handler_fn(|_: &Context, _: Empty| Box::pin(async move { explode() }))).unwrap();

    let competing = Arc::clone(recorder);
    let broadcast = Arc::clone(recorder);
    Plugin::new(MANIFEST)
        .with_operations(registry)
        .with_subscription(
            Subscription::new("orders.events"),
            event_handler_fn(move |ctx: &Context, event: Event| {
                competing.deliveries.lock().unwrap().push(format!("{} {} {} {:?}", event.envelope.id, event.mode, event.attempt, ctx.tenant()));
                Box::pin(async { Ok(()) })
            }),
        )
        .with_subscription(
            Subscription::new("orders.events").with_message_type("orders.cleared").with_mode(SubscriptionMode::Broadcast),
            event_handler_fn(move |_: &Context, event: Event| {
                broadcast.deliveries.lock().unwrap().push(format!("{} {} broadcast-handler", event.envelope.id, event.mode));
                Box::pin(async { Ok(()) })
            }),
        )
        .with_catalog(MessageCatalog::new("orders").with("en-US", "orders.quantity.invalid", "The quantity {quantity} is not valid."))
        .with_hooks(TestHooks(Arc::clone(recorder)))
        .with_network(Network::Tcp)
        .with_logger(Logger::new(log.clone()))
}

// ---------------------------------------------------------------------------
// 扮演宿主
// ---------------------------------------------------------------------------

/// 收集日志行的输出。
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl io::Write for Buffer {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Buffer {
    fn lines(&self) -> Vec<Value> {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }
}

/// 给每个调用加会话令牌的客户端拦截器；`None` 不加。
#[derive(Clone)]
struct Session(Option<MetadataValue<Ascii>>);

impl Interceptor for Session {
    fn call(&mut self, mut request: Request<()>) -> std::result::Result<Request<()>, Status> {
        if let Some(token) = &self.0 {
            request.metadata_mut().insert(plugin::SESSION_METADATA_KEY, token.clone());
        }
        Ok(request)
    }
}

type Client = PluginClient<InterceptedService<Channel, Session>>;

/// 按闭包拨号的连接器：Unix 域套接字与命名管道用。
struct Dial<F>(F);

impl<F, Fut, T> tonic::codegen::Service<Uri> for Dial<F>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    type Response = TokioIo<T>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<TokioIo<T>>> + Send>>;

    fn poll_ready(&mut self, _: &mut TaskContext<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        let connecting = (self.0)();
        Box::pin(async move { connecting.await.map(TokioIo::new) })
    }
}

/// 连上插件端点。
async fn channel(endpoint: &Endpoint) -> Channel {
    let placeholder = tonic::transport::Endpoint::from_static("http://plugin");
    match endpoint.network() {
        Network::Tcp => tonic::transport::Endpoint::from_shared(format!("http://{}", endpoint.address())).unwrap().connect().await.unwrap(),
        #[cfg(unix)]
        Network::Unix => {
            let path = endpoint.address().to_owned();
            placeholder.connect_with_connector(Dial(move || tokio::net::UnixStream::connect(path.clone()))).await.unwrap()
        }
        #[cfg(windows)]
        Network::NamedPipe => {
            let name = endpoint.address().to_owned();
            placeholder
                .connect_with_connector(Dial(move || {
                    let name = name.clone();
                    async move { tokio::net::windows::named_pipe::ClientOptions::new().open(name) }
                }))
                .await
                .unwrap()
        }
        other => panic!("{other} endpoints are not available on this platform"),
    }
}

async fn client(endpoint: &Endpoint, token: Option<&str>) -> Client {
    PluginClient::with_interceptor(channel(endpoint).await, Session(token.map(|token| token.parse().unwrap())))
}

fn handshake_request(versions: Vec<u32>) -> v1::HandshakeRequest {
    v1::HandshakeRequest {
        protocol_versions: versions,
        primitives: 1,
        instance_id: "tests".to_owned(),
        host: Some(v1::HostInfo { framework_version: "spec".to_owned(), sdk: "tests".to_owned(), sdk_version: "0".to_owned(), application: String::new() }),
        host_endpoint: None,
        default_locale: "en-US".to_owned(),
    }
}

fn configure_request(configuration: &str, granted: &[&str]) -> v1::ConfigureRequest {
    v1::ConfigureRequest {
        configuration: configuration.to_owned(),
        granted_permissions: granted.iter().map(|permission| (*permission).to_owned()).collect(),
        bindings: Vec::new(),
        work_directory: std::env::temp_dir().display().to_string(),
    }
}

/// 握手、配置并启动。
async fn open(client: &mut Client, granted: &[&str]) {
    client.handshake(handshake_request(vec![1])).await.unwrap();
    let configured = client.configure(configure_request(r#"{"region":"cn"}"#, granted)).await.unwrap().into_inner();
    assert!(configured.status.unwrap().ok);
    let started = client.start(v1::StartRequest {}).await.unwrap().into_inner();
    assert!(started.status.unwrap().ok);
}

async fn invoke(client: &mut Client, operation: &str, payload: &str, metadata: Option<v1::InvocationMetadata>) -> v1::InvokeResponse {
    client.invoke(v1::InvokeRequest { operation: operation.to_owned(), payload: payload.as_bytes().to_vec(), metadata }).await.unwrap().into_inner()
}

/// 一个失败的应答里的错误。
fn failure(response: &v1::InvokeResponse) -> &v1::Error {
    let status = response.status.as_ref().expect("a response carries a status");
    assert!(!status.ok, "the call must fail");
    status.error.as_ref().expect("a failure carries an error")
}

fn payload(response: &v1::InvokeResponse) -> Value {
    let status = response.status.as_ref().unwrap();
    assert!(status.ok, "{:?}", status.error);
    serde_json::from_slice(&response.payload).unwrap()
}

async fn stop(client: &mut Client, running: Running) {
    let stopped = client.stop(v1::StopRequest { grace_seconds: 5, reason: "unload".to_owned() }).await.unwrap().into_inner();
    assert!(stopped.status.unwrap().ok);
    tokio::time::timeout(Duration::from_secs(5), running.wait()).await.expect("the plugin stops within the grace period").unwrap();
}

// ---------------------------------------------------------------------------
// 运行时
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_plugins_are_rejected_before_listening() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let cases: Vec<(Plugin, &str)> = vec![
        (Plugin::new(MANIFEST.replace(r#""isolation": "out-of-process""#, r#""isolation": "wasm""#)), "PLUGINS_VALIDATION_001"),
        (
            Plugin::new(MANIFEST.replace(r#""health": [ "main" ]"#, r#""health": [ "main" ], "pipeline": [ { "middleware": "audit", "stage": "observe" } ]"#)),
            "PLUGINS_VALIDATION_001",
        ),
        (orders_plugin(&recorder, &log).with_operations(registry_with("orders.unknown")), "PLUGINS_VALIDATION_004"),
        (
            orders_plugin(&recorder, &log).with_subscription(Subscription::new("Orders"), event_handler_fn(|_, _| Box::pin(async { Ok(()) }))),
            "PLUGINS_VALIDATION_004",
        ),
        (
            orders_plugin(&recorder, &log).with_subscription(
                Subscription::new("orders.events").with_mode(SubscriptionMode::Broadcast),
                event_handler_fn(|_, _| Box::pin(async { Ok(()) })),
            ),
            "PLUGINS_VALIDATION_004",
        ),
        (orders_plugin(&recorder, &log).with_catalog(MessageCatalog::new("extra").with("zh-CN", "orders.x", "x")), "LOCALIZATION_VALIDATION_002"),
        (orders_plugin(&recorder, &log).with_catalog(MessageCatalog::new("orders").with("en-US", "orders.x", "x")), "LOCALIZATION_VALIDATION_002"),
        (Plugin::new("{}"), "KERNEL_CAPABILITY_006"),
    ];
    for (plugin, code) in cases {
        let error = plugin.with_network(Network::Tcp).start(Launch::new(TOKEN)).await.unwrap_err();
        assert_eq!(error.code(), code, "{error}");
    }
    assert_eq!(Plugin::from_manifest_file("missing/xihan.manifest.json").unwrap_err().message_key(), Some(manifest::MESSAGE_FILE_MISSING));
}

fn registry_with(id: &str) -> Arc<Registry> {
    let registry = Arc::new(Registry::new());
    registry.add(Contract::new(id), operation::handler_fn(|_: &Context, _: Empty| Box::pin(async { Ok(Empty {}) }))).unwrap();
    registry
}

#[tokio::test]
async fn the_session_token_guards_every_rpc() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    for token in [None, Some("not-the-session-token"), Some(&TOKEN[1..])] {
        let mut client = client(running.endpoint(), token).await;
        assert_eq!(client.handshake(handshake_request(vec![1])).await.unwrap_err().code(), Code::Unauthenticated, "{token:?}");
        assert_eq!(client.health(v1::HealthRequest {}).await.unwrap_err().code(), Code::Unauthenticated);
        assert_eq!(client.watch_health(v1::HealthRequest {}).await.unwrap_err().code(), Code::Unauthenticated);
        let invoke = client.invoke(v1::InvokeRequest { operation: "orders.create".to_owned(), payload: Vec::new(), metadata: None }).await;
        assert_eq!(invoke.unwrap_err().code(), Code::Unauthenticated);
        assert_eq!(client.stop(v1::StopRequest { grace_seconds: 1, reason: "unload".to_owned() }).await.unwrap_err().code(), Code::Unauthenticated);
    }
    assert_eq!(running.state(), LifecycleState::Declared, "refused calls do not run");

    let mut client = client(running.endpoint(), Some(TOKEN)).await;
    let refused = client.handshake(handshake_request(vec![2, 3])).await.unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition, "no common protocol version");
    client.handshake(handshake_request(vec![1])).await.unwrap();
    assert_eq!(running.state(), LifecycleState::Resolved);
    running.shutdown();
    tokio::time::timeout(Duration::from_secs(5), running.wait()).await.unwrap().unwrap();
}

#[tokio::test]
async fn a_plugin_runs_from_handshake_to_stop() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    assert!(running.handshake_line().starts_with("xihan-plugin/1 tcp 127.0.0.1:"));
    let mut client = client(running.endpoint(), Some(TOKEN)).await;

    // 握手：清单原文、SDK、操作声明（契约与 Schema，按 ID 排序）、订阅与消息目录
    let handshake = client.handshake(handshake_request(vec![1, 2])).await.unwrap().into_inner();
    assert_eq!((handshake.protocol_version, handshake.primitives, handshake.manifest.as_str()), (1, 1, MANIFEST));
    let sdk = handshake.sdk.unwrap();
    assert_eq!((sdk.name.as_str(), sdk.language.as_str()), ("framework-sdk-rust", "rust"));
    let ids: Vec<&str> = handshake.operations.iter().map(|operation| operation.id.as_str()).collect();
    assert_eq!(ids, ["orders.create", "orders.panic", "orders.publish", "orders.relay", "orders.slow"]);
    let create = &handshake.operations[0];
    assert_eq!((create.stability.as_str(), create.expose.as_slice(), create.permission.as_str()), ("stable", &["http".to_owned()][..], "orders.write"));
    assert_eq!(
        (create.request_schema.as_str(), create.response_schema.as_str(), create.streaming),
        (r#"{"type":"object","required":["quantity"]}"#, r#"{"type":"object"}"#, false)
    );
    let subscriptions: Vec<(&str, &str, &str)> = handshake
        .subscriptions
        .iter()
        .map(|subscription| (subscription.topic.as_str(), subscription.message_type.as_str(), subscription.mode.as_str()))
        .collect();
    assert_eq!(subscriptions, [("orders.events", "", "competing"), ("orders.events", "orders.cleared", "broadcast")]);
    let catalogs: Vec<(&str, usize)> = handshake.catalogs.iter().map(|catalog| (catalog.name.as_str(), catalog.locales.len())).collect();
    assert_eq!(catalogs, [("orders", 1), ("framework-sdk-rust", 10)]);
    let runtime_catalog = &handshake.catalogs[1];
    assert!(runtime_catalog.locales.iter().all(|locale| locale.templates.contains_key(plugin::MESSAGE_UNAVAILABLE)));
    assert_eq!(client.handshake(handshake_request(vec![1])).await.unwrap_err().code(), Code::FailedPrecondition, "only one handshake");

    // 启动之前的调用不可用，顺序不对的 RPC 被拒
    let early = invoke(&mut client, "orders.create", r#"{"quantity":1}"#, None).await;
    let error = failure(&early);
    assert_eq!((error.code.as_str(), error.message_key.as_str()), (plugin::ERROR_CODE_UNAVAILABLE, plugin::MESSAGE_UNAVAILABLE));
    assert_eq!(serde_json::from_str::<Value>(&error.details).unwrap(), json!({"id": "xihan.tests.orders", "state": "Resolved"}));
    assert_eq!(error.text, "Plugin 'xihan.tests.orders' is not available (state: Resolved).");
    assert_eq!(client.start(v1::StartRequest {}).await.unwrap_err().code(), Code::FailedPrecondition);

    // 配置：钩子拿到配置节、授予的权限与工作目录
    let configured = client.configure(configure_request(r#"{"region":"cn"}"#, &[plugin::PERMISSION_PIPELINE])).await.unwrap().into_inner();
    assert!(configured.status.unwrap().ok);
    let (section, granted, work_directory) = recorder.configuration.lock().unwrap().clone().unwrap();
    assert_eq!(
        (section.as_str(), granted.as_slice(), work_directory),
        (r#"{"region":"cn"}"#, &[plugin::PERMISSION_PIPELINE.to_owned()][..], std::env::temp_dir())
    );
    assert_eq!(running.state(), LifecycleState::Configured);
    let started = client.start(v1::StartRequest {}).await.unwrap().into_inner();
    assert!(started.status.unwrap().ok);
    assert_eq!(running.state(), LifecycleState::Ready);

    // 健康：只留下清单声明的健康项；WatchHealth 先推当前状态
    let health = client.health(v1::HealthRequest {}).await.unwrap().into_inner();
    assert_eq!(health.state, LifecycleState::Ready.value());
    let items: Vec<(&str, i32, &str)> = health.items.iter().map(|item| (item.name.as_str(), item.status, item.description.as_str())).collect();
    assert_eq!(items, [("main", 1, "all good")]);
    let mut watch = client.watch_health(v1::HealthRequest {}).await.unwrap().into_inner();
    assert_eq!(watch.message().await.unwrap().unwrap().state, LifecycleState::Ready.value());

    stop(&mut client, running).await;
    // 停止时 WatchHealth 推最后一条 Draining 再结束
    let mut last = None;
    while let Ok(Some(message)) = watch.message().await {
        last = Some(message.state);
    }
    assert_eq!(last, Some(LifecycleState::Draining.value()));
    assert_eq!(recorder.stops.lock().unwrap().as_slice(), ["unload"]);

    // 日志是一行一个 JSON 对象，每行都带清单 ID；未声明的健康项只告警一次
    let lines = log.lines();
    assert!(lines.iter().all(|line| line["plugin"] == "xihan.tests.orders" && line["level"].is_string() && line["message"].is_string()));
    let messages: Vec<&str> = lines.iter().map(|line| line["message"].as_str().unwrap()).collect();
    for expected in ["listening", "handshake completed", "configured", "started", "stop requested", "stopped"] {
        assert!(messages.contains(&expected), "{expected} in {messages:?}");
    }
    assert_eq!(lines.iter().filter(|line| line["item"] == "ghost" && line["level"] == "warning").count(), 1);
}

#[tokio::test]
async fn configure_failures_leave_the_plugin_failed() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = client(running.endpoint(), Some(TOKEN)).await;
    client.handshake(handshake_request(vec![1])).await.unwrap();
    let configured = client.configure(configure_request(r#"{"fail":true}"#, &[])).await.unwrap().into_inner();
    let status = configured.status.unwrap();
    assert!(!status.ok);
    assert_eq!(status.error.unwrap().code, "ORDERS_VALIDATION_009");
    assert_eq!(running.state(), LifecycleState::Failed);
    let late = invoke(&mut client, "orders.create", r#"{"quantity":1}"#, None).await;
    assert_eq!(failure(&late).code, plugin::ERROR_CODE_UNAVAILABLE);
    stop(&mut client, running).await;
    assert!(recorder.stops.lock().unwrap().is_empty(), "a plugin that was never configured has nothing to release");
}

#[tokio::test]
async fn invocations_restore_the_context_and_carry_errors() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = client(running.endpoint(), Some(TOKEN)).await;
    open(&mut client, &[]).await;

    let metadata = v1::InvocationMetadata {
        trace_parent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_owned(),
        trace_state: String::new(),
        deadline: "2099-01-01T00:00:00Z".to_owned(),
        baggage: HashMap::from([
            ("xihan.tenant".to_owned(), "t1".to_owned()),
            ("xihan.principal".to_owned(), "u1".to_owned()),
            ("xihan.correlation".to_owned(), "c1".to_owned()),
            ("xihan.unregistered".to_owned(), "x".to_owned()),
        ]),
    };
    let created = payload(&invoke(&mut client, "orders.create", r#"{"quantity":2}"#, Some(metadata)).await);
    assert_eq!(
        created,
        json!({"quantity": 2, "tenant": "t1", "principal": "u1", "correlation": "c1", "unregistered": null,
               "trace_id": "0af7651916cd43dd8448eb211c80319c", "span_id": "b7ad6b7169203331", "deadline": true})
    );
    // 没有元数据：新的追踪、没有截止时间之外的 gRPC 截止时间（客户端没设）
    let fresh = payload(&invoke(&mut client, "orders.create", r#"{"quantity":1}"#, None).await);
    assert_eq!((fresh["tenant"].clone(), fresh["deadline"].clone()), (Value::Null, json!(false)));
    assert_eq!(fresh["trace_id"].as_str().unwrap().len(), 32);

    // 业务失败走 Status.error：码、键、细节与已解析的文案；gRPC 状态是 OK
    let rejected = invoke(&mut client, "orders.create", r#"{"quantity":0}"#, None).await;
    let error = failure(&rejected);
    assert_eq!(
        (error.code.as_str(), error.message_key.as_str(), error.details.as_str()),
        ("ORDERS_VALIDATION_001", "orders.quantity.invalid", r#"{"quantity":0}"#)
    );
    assert_eq!(error.text, "orders.quantity.invalid (quantity=0)");
    let invalid = invoke(&mut client, "orders.create", r#"{"quantity":"2"}"#, None).await;
    let error = failure(&invalid);
    assert_eq!((error.code.as_str(), error.message_key.as_str()), ("KERNEL_VALIDATION_001", "kernel.operation.invalid_payload"));
    assert_eq!(serde_json::from_str::<Value>(&error.details).unwrap(), json!({"id": "orders.create", "reason": "$.quantity"}));
    let panicked = invoke(&mut client, "orders.panic", "", None).await;
    assert_eq!(failure(&panicked).code, "KERNEL_UNEXPECTED_001");
    let unknown = client.invoke(v1::InvokeRequest { operation: "orders.missing".to_owned(), payload: Vec::new(), metadata: None }).await;
    assert_eq!(unknown.unwrap_err().code(), Code::Unimplemented);

    // 流式：第一帧不是 start 为 InvalidArgument，合法的流没有可调用的流式操作
    let frames = tokio_stream::iter(vec![v1::StreamFrame { frame: Some(v1::stream_frame::Frame::Chunk(b"{}".to_vec())) }]);
    assert_eq!(client.invoke_stream(frames).await.unwrap_err().code(), Code::InvalidArgument);
    let start = v1::InvokeRequest { operation: "orders.create".to_owned(), payload: Vec::new(), metadata: None };
    let frames = tokio_stream::iter(vec![v1::StreamFrame { frame: Some(v1::stream_frame::Frame::Start(start)) }]);
    assert_eq!(client.invoke_stream(frames).await.unwrap_err().code(), Code::Unimplemented);

    stop(&mut client, running).await;
}

#[tokio::test]
async fn events_are_routed_by_topic_mode_and_type() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = client(running.endpoint(), Some(TOKEN)).await;
    open(&mut client, &[]).await;

    let deliver = |id: &str, topic: &str, event_type: &str, mode: &str| v1::HandleEventRequest {
        envelope: Some(v1::Envelope {
            topic: topic.to_owned(),
            id: id.to_owned(),
            source: "urn:test".to_owned(),
            r#type: event_type.to_owned(),
            spec_version: "1.0".to_owned(),
            data: b"{}".to_vec(),
            ..Default::default()
        }),
        metadata: Some(v1::InvocationMetadata { baggage: HashMap::from([("xihan.tenant".to_owned(), "t1".to_owned())]), ..Default::default() }),
        attempt: 2,
        mode: mode.to_owned(),
    };
    let mut outcomes = Vec::new();
    for request in [
        deliver("a", "orders.events", "orders.created", "competing"),
        deliver("b", "orders.events", "orders.cleared", "broadcast"),
        deliver("c", "orders.events", "orders.cleared", ""),
        deliver("d", "orders.events", "orders.created", "broadcast"),
        deliver("e", "orders.other", "orders.created", "competing"),
        deliver("f", "orders.events", "orders.created", "fanout"),
    ] {
        let status = client.handle_event(request).await.unwrap().into_inner().status.unwrap();
        outcomes.push(if status.ok { "ok".to_owned() } else { status.error.unwrap().code });
    }
    assert_eq!(outcomes, ["ok", "ok", "ok", "PLUGINS_UNEXPECTED_001", "PLUGINS_UNEXPECTED_001", "PLUGINS_UNEXPECTED_001"]);
    assert_eq!(recorder.deliveries.lock().unwrap().as_slice(), ["a competing 2 Some(\"t1\")", "b broadcast broadcast-handler", "c competing 2 Some(\"t1\")"]);
    stop(&mut client, running).await;
}

#[tokio::test]
async fn stop_drains_in_flight_calls() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = client(running.endpoint(), Some(TOKEN)).await;
    open(&mut client, &[]).await;

    let mut slow_client = client.clone();
    let slow = tokio::spawn(async move { invoke(&mut slow_client, "orders.slow", r#"{"millis":400}"#, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut stop_client = client.clone();
    let stopping = tokio::spawn(async move { stop_client.stop(v1::StopRequest { grace_seconds: 5, reason: "shutdown".to_owned() }).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // 排空中：新调用被拒，在途调用照常完成，Stop 等它完成才应答
    let refused = invoke(&mut client, "orders.create", r#"{"quantity":1}"#, None).await;
    let error = failure(&refused);
    assert_eq!(serde_json::from_str::<Value>(&error.details).unwrap()["state"], "Draining");
    assert!(!stopping.is_finished(), "Stop waits for the call in flight");
    assert_eq!(payload(&slow.await.unwrap()), json!({"cancelled": false}));
    assert!(stopping.await.unwrap().unwrap().into_inner().status.unwrap().ok);
    tokio::time::timeout(Duration::from_secs(5), running.wait()).await.unwrap().unwrap();
    assert_eq!(recorder.stops.lock().unwrap().as_slice(), ["shutdown"]);
}

#[tokio::test]
async fn cancelled_calls_signal_the_token() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = client(running.endpoint(), Some(TOKEN)).await;
    open(&mut client, &[]).await;

    // gRPC 截止时间到了，客户端放弃调用：插件里的令牌触发，处理器提前返回
    let mut request = Request::new(v1::InvokeRequest { operation: "orders.slow".to_owned(), payload: br#"{"millis":10000}"#.to_vec(), metadata: None });
    request.set_timeout(Duration::from_millis(200));
    let started = std::time::Instant::now();
    // 客户端与 tonic 服务端各按 grpc-timeout 计时，先到的一方结束调用
    let code = client.invoke(request).await.unwrap_err().code();
    assert!(matches!(code, Code::DeadlineExceeded | Code::Cancelled), "{code:?}");
    // 排空等的是处理器真正结束：令牌触发后它很快返回，Stop 不必等满 10 秒
    stop(&mut client, running).await;
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
}

#[tokio::test]
async fn shutdown_works_like_closing_stdin() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = client(running.endpoint(), Some(TOKEN)).await;
    open(&mut client, &[]).await;
    running.shutdown();
    tokio::time::timeout(Duration::from_secs(5), running.wait()).await.unwrap().unwrap();
    assert_eq!(recorder.stops.lock().unwrap().as_slice(), ["exit"]);
}

#[tokio::test]
async fn the_platform_endpoint_is_private() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).with_network(Network::platform_default()).start(Launch::new(TOKEN)).await.unwrap();
    assert_eq!(running.endpoint().network(), Network::platform_default());
    #[cfg(unix)]
    let directory = {
        use std::os::unix::fs::PermissionsExt;
        let socket = std::path::Path::new(running.endpoint().address());
        let directory = socket.parent().unwrap().to_path_buf();
        assert_eq!(std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::symlink_metadata(socket).unwrap().permissions().mode() & 0o777, 0o600);
        directory
    };
    let mut client = client(running.endpoint(), Some(TOKEN)).await;
    client.handshake(handshake_request(vec![1])).await.unwrap();
    stop(&mut client, running).await;
    #[cfg(unix)]
    assert!(!directory.exists(), "the private directory is removed after stopping");
}

// ---------------------------------------------------------------------------
// 回调
// ---------------------------------------------------------------------------

/// 回调服务收到的一次 `Host.Invoke`：会话令牌、`grpc-timeout` 头与请求。
type ReceivedInvoke = (Option<String>, Option<String>, v1::InvokeRequest);

/// 扮演宿主的回调服务：记下收到的请求与会话令牌。
#[derive(Default)]
struct FakeHost {
    invokes: Mutex<Vec<ReceivedInvoke>>,
    publishes: Mutex<Vec<(Option<String>, v1::PublishRequest)>>,
}

struct FakeHostService(Arc<FakeHost>);

fn header(metadata: &tonic::metadata::MetadataMap, name: &str) -> Option<String> {
    metadata.get(name).map(|value| value.to_str().unwrap().to_owned())
}

#[tonic::async_trait]
impl HostRpc for FakeHostService {
    async fn invoke(&self, request: Request<v1::InvokeRequest>) -> std::result::Result<Response<v1::InvokeResponse>, Status> {
        let (session, timeout) = (header(request.metadata(), plugin::SESSION_METADATA_KEY), header(request.metadata(), "grpc-timeout"));
        let request = request.into_inner();
        let operation = request.operation.clone();
        self.0.invokes.lock().unwrap().push((session, timeout, request));
        match operation.as_str() {
            "host.echo" => {
                Ok(Response::new(v1::InvokeResponse { status: Some(v1::Status { ok: true, error: None }), payload: br#"{"echoed":true}"#.to_vec() }))
            }
            "host.conflict" => Ok(Response::new(v1::InvokeResponse {
                status: Some(v1::Status {
                    ok: false,
                    error: Some(v1::Error {
                        code: "ORDERS_CONFLICT_001".to_owned(),
                        message_key: "orders.conflict".to_owned(),
                        text: "The order changed.".to_owned(),
                        details: r#"{"id":"o1"}"#.to_owned(),
                        inner: None,
                    }),
                }),
                payload: Vec::new(),
            })),
            "host.denied" => Err(Status::permission_denied("Permission 'capability:kernel.pipeline' is not granted.")),
            _ => Err(Status::unimplemented("no such operation")),
        }
    }

    async fn publish(&self, request: Request<v1::PublishRequest>) -> std::result::Result<Response<v1::PublishResponse>, Status> {
        let session = header(request.metadata(), plugin::SESSION_METADATA_KEY);
        let request = request.into_inner();
        let results = request.envelopes.iter().map(|_| v1::Status { ok: true, error: None }).collect();
        self.0.publishes.lock().unwrap().push((session, request));
        Ok(Response::new(v1::PublishResponse { results }))
    }
}

/// 在回环 TCP 上起回调服务，返回它的端点。
async fn fake_host(host: &Arc<FakeHost>) -> Endpoint {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = HostServer::new(FakeHostService(Arc::clone(host)));
    tokio::spawn(tonic::transport::Server::builder().serve_with_incoming(service, tokio_stream::wrappers::TcpListenerStream::new(listener)));
    Endpoint::new(Network::Tcp, address.to_string())
}

#[tokio::test]
async fn callbacks_carry_the_session_and_only_handed_tenants() {
    let host = Arc::new(FakeHost::default());
    let endpoint = fake_host(&host).await;
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN).with_host_address(endpoint)).await.unwrap();
    let mut client = client(running.endpoint(), Some(TOKEN)).await;
    open(&mut client, &[plugin::PERMISSION_PIPELINE, plugin::PERMISSION_MESSAGING]).await;

    let handed = || v1::InvocationMetadata {
        trace_parent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_owned(),
        deadline: "2099-01-01T00:00:00Z".to_owned(),
        baggage: HashMap::from([("xihan.tenant".to_owned(), "t1".to_owned()), ("xihan.correlation".to_owned(), "c1".to_owned())]),
        ..Default::default()
    };
    // 回调带会话令牌、追踪、截止时间与宿主交给插件的租户
    let relayed = payload(&invoke(&mut client, "orders.relay", r#"{"operation":"host.echo"}"#, Some(handed())).await);
    assert_eq!(relayed, json!({"payload": r#"{"echoed":true}"#}));
    {
        let invokes = host.invokes.lock().unwrap();
        let (session, timeout, request) = invokes.last().unwrap();
        assert_eq!(session.as_deref(), Some(TOKEN));
        assert!(timeout.is_some(), "the gRPC deadline follows the context");
        let metadata = request.metadata.as_ref().unwrap();
        assert_eq!(metadata.trace_parent, "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01");
        assert_eq!(metadata.deadline, "2099-01-01T00:00:00.000Z");
        assert_eq!(metadata.baggage, HashMap::from([("xihan.tenant".to_owned(), "t1".to_owned()), ("xihan.correlation".to_owned(), "c1".to_owned())]));
    }

    // 换成别的租户：本地就拒绝，不发给宿主
    let sent = host.invokes.lock().unwrap().len();
    let foreign = invoke(&mut client, "orders.relay", r#"{"operation":"host.echo","tenant":"t2"}"#, Some(handed())).await;
    let error = failure(&foreign);
    assert_eq!((error.code.as_str(), error.message_key.as_str()), (plugin::ERROR_CODE_CALLBACK_DENIED, plugin::MESSAGE_CALLBACK_TENANT_DENIED));
    assert_eq!(serde_json::from_str::<Value>(&error.details).unwrap(), json!({"id": "xihan.tests.orders", "tenant": "t2"}));
    assert_eq!(host.invokes.lock().unwrap().len(), sent);

    // 宿主的业务失败原样回来；传输层的状态按映射表
    let conflict = invoke(&mut client, "orders.relay", r#"{"operation":"host.conflict"}"#, None).await;
    let error = failure(&conflict);
    assert_eq!((error.code.as_str(), error.message_key.as_str(), error.text.as_str()), ("ORDERS_CONFLICT_001", "orders.conflict", "The order changed."));
    assert_eq!(serde_json::from_str::<Value>(&error.details).unwrap(), json!({"id": "o1"}));
    let denied = invoke(&mut client, "orders.relay", r#"{"operation":"host.denied"}"#, None).await;
    assert_eq!((failure(&denied).code.as_str(), failure(&denied).message_key.as_str()), (plugin::ERROR_CODE_CALLBACK_DENIED, plugin::MESSAGE_CALLBACK_DENIED));
    let missing = invoke(&mut client, "orders.relay", r#"{"operation":"host.missing"}"#, None).await;
    assert_eq!(failure(&missing).code, "KERNEL_NOTFOUND_001");
    assert_eq!(serde_json::from_str::<Value>(&failure(&missing).details).unwrap(), json!({"id": "host.missing"}));

    // 发布逐项判定租户：越界的那一项在本地失败，其余照常发出
    let published = payload(&invoke(&mut client, "orders.publish", r#"{"tenants":[null,"t1","t9"]}"#, Some(handed())).await);
    assert_eq!(published, json!({"outcomes": ["ok", "ok", plugin::ERROR_CODE_CALLBACK_DENIED]}));
    {
        let publishes = host.publishes.lock().unwrap();
        let (session, request) = publishes.last().unwrap();
        assert_eq!(session.as_deref(), Some(TOKEN));
        assert_eq!(request.envelopes.iter().map(|envelope| envelope.id.as_str()).collect::<Vec<_>>(), ["e0", "e1"]);
        assert_eq!((request.envelopes[0].spec_version.as_str(), request.envelopes[0].data_content_type.as_str()), ("1.0", "application/json"));
    }
    // 调用结束之后租户不再是交给插件的租户：不带元数据的调用里带 t1 同样被拒
    let after = payload(&invoke(&mut client, "orders.publish", r#"{"tenants":["t1"]}"#, None).await);
    assert_eq!(after, json!({"outcomes": [plugin::ERROR_CODE_CALLBACK_DENIED]}));
    stop(&mut client, running).await;
}

#[tokio::test]
async fn callbacks_need_declared_and_granted_permissions() {
    let host = Arc::new(FakeHost::default());
    let endpoint = fake_host(&host).await;
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN).with_host_address(endpoint)).await.unwrap();
    let mut client = client(running.endpoint(), Some(TOKEN)).await;
    // 宿主只授予了发布：调用宿主的操作在本地就被拒
    open(&mut client, &[plugin::PERMISSION_MESSAGING]).await;
    let relayed = invoke(&mut client, "orders.relay", r#"{"operation":"host.echo"}"#, None).await;
    let error = failure(&relayed);
    assert_eq!((error.code.as_str(), error.message_key.as_str()), (plugin::ERROR_CODE_CALLBACK_DENIED, plugin::MESSAGE_CALLBACK_DENIED));
    assert_eq!(serde_json::from_str::<Value>(&error.details).unwrap()["permission"], plugin::PERMISSION_PIPELINE);
    assert!(host.invokes.lock().unwrap().is_empty());
    let published = payload(&invoke(&mut client, "orders.publish", r#"{"tenants":[null]}"#, None).await);
    assert_eq!(published, json!({"outcomes": ["ok"]}));
    stop(&mut client, running).await;
}

#[tokio::test]
async fn callbacks_to_an_unreachable_host_are_unavailable() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    // 绑定后立即关掉的端口：连上去一定失败
    let address = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN).with_host_address(Endpoint::new(Network::Tcp, address.to_string()))).await.unwrap();
    let mut client = client(running.endpoint(), Some(TOKEN)).await;
    open(&mut client, &[plugin::PERMISSION_PIPELINE]).await;
    let relayed = invoke(&mut client, "orders.relay", r#"{"operation":"host.echo"}"#, None).await;
    let error = failure(&relayed);
    assert_eq!((error.code.as_str(), error.message_key.as_str()), (plugin::ERROR_CODE_UNAVAILABLE, plugin::MESSAGE_FAILED));
    assert_eq!(serde_json::from_str::<Value>(&error.details).unwrap()["phase"], "Host.Invoke");
    stop(&mut client, running).await;
}
