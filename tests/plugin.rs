//! 进程外插件运行时（RFC-0017，ADR-0237）：回放向量 `plugin/handshake-lines.json`、`plugin/host-addresses.json`、
//! `plugin/protocol-versions.json` 与 `plugin/callback-status.json`（经假的宿主端到端；`deadlines.json`、`plugin-messages.json` 与
//! 回调映射的全部用例在 crate 内的单元测试回放），再以扮演宿主的 gRPC 客户端核对：启动前的核对、握手、会话令牌、调用次序、配置节、
//! 就绪、健康、停止与收尾、调用与投递的核对顺序、上下文还原、错误的写出与读入、回调的本地核对、元数据与状态映射。

mod common;

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

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
use xihan_framework::primitives::{self, BAGGAGE_TENANT, Catalog, Error, LifecycleState, Result};

/// 测试里宿主下发的会话令牌。
const TOKEN: &str = "c2Vzc2lvbi10b2tlbi1mb3ItdGhlLXRlc3RzLW9ubHk";

/// 被测插件的清单 ID。
const ID: &str = "xihan.tests.orders";

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
struct HostAddresses {
    valid: Vec<ValidAddresses>,
    invalid: Vec<String>,
    selection: Vec<Selection>,
}

#[derive(Deserialize)]
struct ValidAddresses {
    value: String,
    entries: Vec<Entry>,
}

#[derive(Debug, Deserialize, PartialEq)]
struct Entry {
    network: String,
    address: String,
}

#[derive(Deserialize)]
struct Selection {
    value: Option<String>,
    supported: Vec<String>,
    selected: Option<Entry>,
}

fn entry(endpoint: &Endpoint) -> Entry {
    Entry { network: endpoint.network().as_str().to_owned(), address: endpoint.address().to_owned() }
}

#[test]
fn host_addresses_follow_the_vectors() {
    let vectors: HostAddresses = common::vector("plugin/host-addresses.json");
    for case in &vectors.valid {
        let endpoints = plugin::parse_host_addresses(&case.value).unwrap_or_else(|| panic!("{}", case.value));
        assert_eq!(endpoints.iter().map(entry).collect::<Vec<_>>(), case.entries, "{}", case.value);
    }
    for value in &vectors.invalid {
        assert_eq!(plugin::parse_host_addresses(value), None, "{value:?}");
    }
    for case in &vectors.selection {
        let supported: Vec<Network> = case.supported.iter().map(|name| Network::parse(name).unwrap()).collect();
        // 变量缺失按空串传入
        let selected = plugin::select_host_endpoint(case.value.as_deref().unwrap_or_default(), &supported);
        assert_eq!(selected.as_ref().map(entry), case.selected, "{:?} {:?}", case.value, case.supported);
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

fn environment(pairs: &[(&str, &str)]) -> std::result::Result<Launch, xihan_framework::primitives::FrameworkError> {
    let map: HashMap<String, String> = pairs.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
    Launch::from_lookup(move |name| map.get(name).cloned())
}

#[test]
fn launch_reads_the_environment() {
    let launch = environment(&[
        (plugin::ENV_SESSION, TOKEN),
        (plugin::ENV_PROTOCOL_VERSIONS, "1,2"),
        (plugin::ENV_PRIMITIVES, "1"),
        (plugin::ENV_HOST_ADDRESS, r"npipe \\.\pipe\xihan-host-1f3a;unix /tmp/xihan-host-1f3a/host.sock;tcp 127.0.0.1:5000"),
        (plugin::ENV_INSTANCE, "orders#1"),
        (plugin::ENV_LOCALE, "zh-CN"),
    ])
    .unwrap();
    assert_eq!((launch.protocol_versions.as_slice(), launch.primitives, launch.session.as_str()), (&[1, 2][..], Some(1), TOKEN));
    // 按宿主的顺序取第一个本平台能连的：Windows 上是命名管道，Unix 上是 Unix 域套接字
    let expected = if cfg!(windows) {
        Endpoint::new(Network::NamedPipe, r"\\.\pipe\xihan-host-1f3a")
    } else if cfg!(unix) {
        Endpoint::new(Network::Unix, "/tmp/xihan-host-1f3a/host.sock")
    } else {
        Endpoint::new(Network::Tcp, "127.0.0.1:5000")
    };
    assert_eq!(launch.host_address, Some(expected));
    assert_eq!((launch.instance.as_deref(), launch.locale.as_deref()), (Some("orders#1"), Some("zh-CN")));
    assert!(!format!("{launch:?}").contains(TOKEN), "the session token stays out of debug output");

    // 回调端点缺失、不合法或没有能连的项不是启动错误：没有回调端点
    for value in [None, Some("tcp 10.0.0.1:5000"), Some("unix /tmp/a;tcp 0.0.0.0:1"), Some(""), Some("udp 127.0.0.1:1")] {
        let mut pairs = vec![(plugin::ENV_SESSION, TOKEN)];
        pairs.extend(value.map(|value| (plugin::ENV_HOST_ADDRESS, value)));
        assert_eq!(environment(&pairs).unwrap().host_address, None, "{value:?}");
    }
    for pairs in [
        &[][..],
        &[(plugin::ENV_SESSION, "")],
        &[(plugin::ENV_SESSION, "token with spaces")],
        &[(plugin::ENV_SESSION, TOKEN), (plugin::ENV_PROTOCOL_VERSIONS, "1,x")],
        &[(plugin::ENV_SESSION, TOKEN), (plugin::ENV_PROTOCOL_VERSIONS, "01")],
        &[(plugin::ENV_SESSION, TOKEN), (plugin::ENV_PRIMITIVES, "one")],
    ] {
        assert_eq!(environment(pairs).unwrap_err().code(), "PRIMITIVES_VALIDATION_001", "{pairs:?}");
    }
}

/// 下沉到原语层的项从旧路径照常可用：`plugin::LOCALES` 与 `plugin::is_whitespace` 就是原语层的那一项；`MessageCatalog` 的公开面
/// 不变，它是带目录名、要求有 `en-US` 的 `Catalog`。
#[test]
fn locale_helpers_are_the_primitives_ones() {
    assert_eq!(plugin::LOCALES, primitives::LOCALES);
    assert!(plugin::is_whitespace('\u{85}') && !plugin::is_whitespace('\u{200B}'));
    let check: fn(char) -> bool = plugin::is_whitespace;
    assert!(check('\u{3000}'));

    let english = r#"{"orders.quantity.invalid":"The quantity {quantity} is not valid."}"#;
    let chinese = r#"{"orders.quantity.invalid":"数量 {quantity} 不合法。"}"#;
    let catalog = MessageCatalog::from_json("orders", [("en-US", english), ("zh-CN", chinese)]).unwrap();
    assert_eq!((catalog.name(), catalog.locales().collect::<Vec<_>>()), ("orders", vec!["en-US", "zh-CN"]));
    assert_eq!(catalog.templates("zh-CN").collect::<Vec<_>>(), [("orders.quantity.invalid", "数量 {quantity} 不合法。")]);
    assert_eq!(catalog.to_catalog(), Catalog::from_json([("en-US", english), ("zh-CN", chinese)]).unwrap());
    assert_eq!(
        catalog,
        MessageCatalog::new("orders").with("en-US", "orders.quantity.invalid", "The quantity {quantity} is not valid.").with(
            "zh-CN",
            "orders.quantity.invalid",
            "数量 {quantity} 不合法。"
        )
    );

    // 握手交给宿主的目录要有 en-US（handshake.md §3 第 6 条），原语层的目录不要求；错误的说明带目录名
    let missing = MessageCatalog::from_json("orders", [("zh-CN", chinese)]).unwrap_err();
    assert_eq!((missing.code(), missing.message()), (primitives::ERROR_CODE_INVALID_CATALOG, "The message catalog 'orders' has no en-US templates."));
    let invalid = MessageCatalog::from_json("orders", [("en-US", "[]")]).unwrap_err();
    assert_eq!(invalid.code(), primitives::ERROR_CODE_INVALID_CATALOG);
    assert!(invalid.message().starts_with("The 'en-US' templates of the message catalog 'orders' are not a JSON object of strings: "), "{invalid}");
    let key = MessageCatalog::from_json("orders", [("en-US", r#"{"Orders.bad":"x"}"#)]).unwrap_err();
    assert_eq!(key.message(), "The message catalog 'orders' has an invalid message key 'Orders.bad' in 'en-US'.");

    // 运行时自己的目录与插件的目录合并成一份，交给进程默认解析器
    let mut merged = plugin::runtime_catalog().to_catalog();
    merged.extend(catalog.to_catalog());
    assert_eq!(merged.locales().count(), primitives::LOCALES.len());
    assert!(merged.template("zh-CN", plugin::MESSAGE_UNAVAILABLE).is_some() && merged.template("zh-CN", "orders.quantity.invalid").is_some());
}

/// 插件运行时里按名字编码的枚举：名字是协议里的写法，`name` 与 `as_str` 是同一个访问器；订阅方式在协议里的空同 `competing`，
/// 日志级别按严重程度排序。
#[test]
fn plugin_enums_are_encoded_by_name() {
    common::assert_named(Network::ALL, &["unix", "tcp", "npipe"], Network::name, Network::as_str);
    let levels = ["trace", "debug", "information", "warning", "error", "critical"];
    common::assert_named(plugin::Level::ALL, &levels, plugin::Level::name, plugin::Level::as_str);
    assert!(plugin::Level::ALL.windows(2).all(|pair| pair[0] < pair[1]));
    common::assert_named(SubscriptionMode::ALL, &["competing", "broadcast"], SubscriptionMode::name, SubscriptionMode::as_str);
    assert_eq!((SubscriptionMode::parse(""), "".parse::<SubscriptionMode>().ok()), (Some(SubscriptionMode::Competing), Some(SubscriptionMode::Competing)));
    assert_eq!(SubscriptionMode::default(), SubscriptionMode::Competing);
    assert_eq!("TCP".parse::<Network>().unwrap_err().to_string(), "PRIMITIVES_VALIDATION_001: 'TCP' is not a network.");
    // 握手交给宿主的目录要有模板：没有键的 en-US 不算有
    assert_eq!(MessageCatalog::from_json("orders", [("en-US", "{}")]).unwrap_err().message(), "The message catalog 'orders' has no en-US templates.");
}

/// 常量各有一个出处：插件一侧的未预期失败有不与内核同名的名字，旧名弃用、值不变；清单文件名就是 `manifest::FILE_NAME`；
/// `Plugin::start` 报的两个校验码是公开常量，名字同 .NET 的 `PluginErrors`。
#[test]
#[allow(deprecated)]
fn plugin_constants_have_one_source() {
    assert_eq!((plugin::ERROR_CODE_PLUGIN_UNEXPECTED, plugin::ERROR_CODE_UNEXPECTED), ("PLUGINS_UNEXPECTED_001", "PLUGINS_UNEXPECTED_001"));
    assert_eq!(operation::ERROR_CODE_UNEXPECTED, "KERNEL_UNEXPECTED_001", "the kernel code of the same old name is a different one");
    assert_eq!((plugin::MANIFEST_FILE_NAME, manifest::FILE_NAME), ("xihan.manifest.json", "xihan.manifest.json"));
    assert_eq!((plugin::ERROR_CODE_INVALID_SOURCE, plugin::ERROR_CODE_HANDSHAKE_MISMATCH), ("PLUGINS_VALIDATION_001", "PLUGINS_VALIDATION_004"));
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

/// 钩子怎样应答。
#[derive(Clone, Copy, Default, PartialEq)]
enum Mode {
    #[default]
    Ok,
    Fail,
    Panic,
}

/// 健康钩子怎样应答。
#[derive(Clone)]
enum HealthMode {
    /// 没有健康回调。
    Absent,
    /// 报这个健康。
    Report(Health),
    /// panic。
    Panic,
}

/// 插件这一侧记下的事与钩子的应答方式。
struct Recorder {
    configuration: Mutex<Option<(String, Vec<String>, PathBuf)>>,
    settings: Mutex<Option<Settings>>,
    deliveries: Mutex<Vec<String>>,
    stops: Mutex<Vec<String>>,
    health_calls: AtomicUsize,
    health: Mutex<HealthMode>,
    start: Mutex<Mode>,
    stop: Mutex<Mode>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            configuration: Mutex::default(),
            settings: Mutex::default(),
            deliveries: Mutex::default(),
            stops: Mutex::default(),
            health_calls: AtomicUsize::new(0),
            health: Mutex::new(HealthMode::Report(ready())),
            start: Mutex::default(),
            stop: Mutex::default(),
        }
    }
}

/// 缺省的健康：`Ready`，一个声明过的健康项与一个没声明的。
fn ready() -> Health {
    Health::new(LifecycleState::Ready)
        .with_item(HealthItem::new("main", HealthStatus::Healthy).with_description("all good"))
        .with_item(HealthItem::new("ghost", HealthStatus::Unhealthy))
}

/// 配置节的类型化读取（`lifecycle.md` §7.2）：宿主把叶子写成字符串、成员名的大小写可以不同。
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
struct Settings {
    region: Option<String>,
    limit: Option<u32>,
    enabled: Option<bool>,
    ratio: Option<f64>,
}

struct TestHooks(Arc<Recorder>);

impl Hooks for TestHooks {
    fn configure<'a>(&'a self, configuration: &'a Configuration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let section: Value = serde_json::from_str(&configuration.section).unwrap();
            if section.get("panic").is_some() {
                panic!("the secret path /etc/orders is unreadable");
            }
            if section.get("fail").is_some() {
                return Err(Error::validation("ORDERS_VALIDATION_009", "orders.configuration.invalid"));
            }
            let settings: Settings = configuration.decode().map_err(|error| Error::literal("ORDERS_VALIDATION_009", error.to_string()))?;
            *self.0.settings.lock().unwrap() = Some(settings);
            *self.0.configuration.lock().unwrap() =
                Some((configuration.section.clone(), configuration.granted_permissions.clone(), configuration.work_directory.clone()));
            Ok(())
        })
    }

    fn start(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let mode = *self.0.start.lock().unwrap();
            match mode {
                Mode::Ok => Ok(()),
                Mode::Fail => Err(Error::unavailable("ORDERS_UNAVAILABLE_002", "orders.database.down")),
                Mode::Panic => panic!("the start hook exploded"),
            }
        })
    }

    fn stop<'a>(&'a self, reason: &'a str, _: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.0.stops.lock().unwrap().push(reason.to_owned());
            let mode = *self.0.stop.lock().unwrap();
            match mode {
                Mode::Ok => Ok(()),
                Mode::Fail => Err(Error::unavailable("ORDERS_UNAVAILABLE_003", "orders.flush.failed")),
                Mode::Panic => panic!("the stop hook exploded"),
            }
        })
    }

    fn health(&self) -> Option<Health> {
        self.0.health_calls.fetch_add(1, Ordering::SeqCst);
        // 先放开锁再 panic：panic 不能毒化测试里的锁
        let mode = self.0.health.lock().unwrap().clone();
        match mode {
            HealthMode::Absent => None,
            HealthMode::Report(health) => Some(health),
            HealthMode::Panic => panic!("the health hook exploded"),
        }
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
    /// 在另造的上下文里回调：`deadline` 是 50 毫秒后到期、等它到期再回调，`cancel` 是已被取消的，`short` 是 300 毫秒后到期、立即回调。
    abandon: Option<String>,
}

#[derive(Serialize)]
struct Relayed {
    payload: String,
}

#[derive(Deserialize)]
struct Publish {
    #[serde(default = "default_topic")]
    topic: String,
    tenant: Option<String>,
    envelopes: Vec<Option<String>>,
}

fn default_topic() -> String {
    "orders.events".to_owned()
}

#[derive(Serialize)]
struct Published {
    outcomes: Vec<Value>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Empty {}

/// 处理器里的 panic：调用器的 Observe 阶段把它翻译成未处理失败。
fn explode() -> Result<Empty> {
    panic!("boom")
}

/// 一个结果写成测试里比较的形态：`"ok"`，或错误的码、键与细节。
fn outcome(result: Result<()>) -> Value {
    match result {
        Ok(()) => json!("ok"),
        Err(error) => json!({"code": error.code(), "key": error.message_key(), "details": error.details()}),
    }
}

/// 换上回调上下文的租户：带了就派生，不带就原样。
fn with_tenant(ctx: &Context, tenant: Option<String>) -> Context {
    match tenant {
        Some(tenant) => ctx.with_baggage(BAGGAGE_TENANT, tenant),
        None => ctx.clone(),
    }
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
                    if request.quantity == 99 {
                        return Err(Error::literal("ORDERS_CONFLICT_001", "The order changed."));
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
                    // 宿主取消、停止的宽限到了或收尾时令牌触发，处理器照常返回自己的结果（RFC-0003 规则 1）
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
                    let host = plugin::host(ctx).expect("the runtime puts the host client into every call");
                    let callback = match request.abandon.as_deref() {
                        Some("deadline") => {
                            let expired = Context::builder().timeout(Duration::from_millis(50)).build();
                            expired.done().await;
                            expired
                        }
                        Some("cancel") => {
                            let token = xihan_framework::invocation::CancellationToken::new();
                            token.cancel();
                            Context::builder().cancellation(&token).build()
                        }
                        Some("short") => Context::builder().timeout(Duration::from_millis(300)).build(),
                        _ => with_tenant(ctx, request.tenant),
                    };
                    let payload = host.invoke(&callback, &request.operation, "{}").await?;
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
                    let host = plugin::host(ctx).expect("the runtime puts the host client into every call");
                    let envelopes = request
                        .envelopes
                        .into_iter()
                        .enumerate()
                        .map(|(index, tenant)| {
                            let envelope = Envelope::new(request.topic.clone(), format!("e{index}"), "urn:test", "orders.created");
                            match tenant {
                                Some(tenant) => envelope.with_extension(plugin::TENANT_EXTENSION, tenant),
                                None => envelope,
                            }
                        })
                        .collect();
                    let outcomes = host.publish(&with_tenant(ctx, request.tenant), envelopes).await;
                    Ok(Published { outcomes: outcomes.into_iter().map(outcome).collect() })
                })
            }),
        )
        .unwrap();
    registry.add(Contract::new("orders.panic"), operation::handler_fn(|_: &Context, _: Empty| Box::pin(async move { explode() }))).unwrap();

    let first = Arc::clone(recorder);
    let broadcast = Arc::clone(recorder);
    let second = Arc::clone(recorder);
    Plugin::new(MANIFEST)
        .with_operations(registry)
        .with_subscription(
            Subscription::new("orders.events"),
            event_handler_fn(move |ctx: &Context, event: Event| {
                first.deliveries.lock().unwrap().push(format!("{} {} first {} {:?}", event.envelope.id, event.mode, event.attempt, ctx.tenant()));
                if event.envelope.id == "panic" {
                    panic!("the event handler exploded");
                }
                let rejected = event.envelope.id.starts_with("fail");
                Box::pin(async move { if rejected { Err(Error::validation("ORDERS_VALIDATION_002", "orders.event.rejected")) } else { Ok(()) } })
            }),
        )
        .with_subscription(
            Subscription::new("orders.events").with_message_type("orders.cleared").with_mode(SubscriptionMode::Broadcast),
            event_handler_fn(move |_: &Context, event: Event| {
                broadcast.deliveries.lock().unwrap().push(format!("{} {} broadcast", event.envelope.id, event.mode));
                Box::pin(async { Ok(()) })
            }),
        )
        .with_subscription(
            Subscription::new("orders.events").with_message_type("orders.created"),
            event_handler_fn(move |_: &Context, event: Event| {
                second.deliveries.lock().unwrap().push(format!("{} {} second", event.envelope.id, event.mode));
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

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// 给每个调用加会话令牌的客户端拦截器：每个值各加一次 `xihan-plugin-session`，空表示不带。
#[derive(Clone)]
struct Session(Vec<MetadataValue<Ascii>>);

impl Interceptor for Session {
    fn call(&mut self, mut request: Request<()>) -> std::result::Result<Request<()>, Status> {
        for token in &self.0 {
            request.metadata_mut().append(plugin::SESSION_METADATA_KEY, token.clone());
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

/// 带这些会话令牌值的客户端。
async fn client_with(endpoint: &Endpoint, tokens: &[&str]) -> Client {
    PluginClient::with_interceptor(channel(endpoint).await, Session(tokens.iter().map(|token| token.parse().unwrap()).collect()))
}

/// 带对了会话令牌的客户端。
async fn connect(endpoint: &Endpoint) -> Client {
    client_with(endpoint, &[TOKEN]).await
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

/// 起一个插件并握手、配置、启动；宿主授予清单的全部权限。
async fn serving(recorder: &Arc<Recorder>, log: &Buffer, launch: Launch) -> (Running, Client) {
    let running = orders_plugin(recorder, log).start(launch).await.unwrap();
    let mut client = connect(running.endpoint()).await;
    open(&mut client, &[plugin::PERMISSION_PIPELINE, plugin::PERMISSION_MESSAGING]).await;
    (running, client)
}

async fn invoke(client: &mut Client, operation: &str, payload: &str, metadata: Option<v1::InvocationMetadata>) -> v1::InvokeResponse {
    client.invoke(v1::InvokeRequest { operation: operation.to_owned(), payload: payload.as_bytes().to_vec(), metadata }).await.unwrap().into_inner()
}

/// 一个失败的应答里的错误。
fn failure(status: Option<&v1::Status>) -> &v1::Error {
    let status = status.expect("a response carries a status");
    assert!(!status.ok, "the call must fail");
    status.error.as_ref().expect("a failure carries an error")
}

/// 失败的码、键与 details（按 JSON 值）；带消息键的错误 `text` 必须为空（`invocation.md` §3）。
fn keyed(status: Option<&v1::Status>) -> (String, String, Value) {
    let error = failure(status);
    assert_eq!(error.text, "", "an error with a message key leaves text empty: {error:?}");
    let details = if error.details.is_empty() { Value::Null } else { serde_json::from_str(&error.details).unwrap() };
    (error.code.clone(), error.message_key.clone(), details)
}

fn payload(response: &v1::InvokeResponse) -> Value {
    let status = response.status.as_ref().unwrap();
    assert!(status.ok, "{:?}", status.error);
    serde_json::from_slice(&response.payload).unwrap()
}

async fn health(client: &mut Client) -> v1::HealthResponse {
    client.health(v1::HealthRequest {}).await.unwrap().into_inner()
}

async fn stop(client: &mut Client, running: Running) {
    let stopped = client.stop(v1::StopRequest { grace_seconds: 5, reason: "unload".to_owned() }).await.unwrap().into_inner();
    assert!(stopped.status.unwrap().ok);
    tokio::time::timeout(Duration::from_secs(5), running.wait()).await.expect("the plugin stops within the grace period").unwrap();
}

/// 一次投递。
fn delivery(id: &str, topic: &str, event_type: &str, mode: &str) -> v1::HandleEventRequest {
    v1::HandleEventRequest {
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
    }
}

// ---------------------------------------------------------------------------
// 启动与握手（handshake.md §1–§4）
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
        // primitives 必须等于本 SDK 实现的原语层大版本
        (Plugin::new(MANIFEST.replace(r#""primitives": "1""#, r#""primitives": "2""#)), "PLUGINS_VALIDATION_004"),
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
        // 运行时自己的目录名保留：它的键宿主已经有
        (orders_plugin(&recorder, &log).with_catalog(plugin::runtime_catalog()), "LOCALIZATION_VALIDATION_002"),
        (Plugin::new("{}"), "KERNEL_CAPABILITY_006"),
    ];
    for (plugin, code) in cases {
        let rejected = Buffer::default();
        let error = plugin.with_network(Network::Tcp).with_logger(Logger::new(rejected.clone())).start(Launch::new(TOKEN)).await.unwrap_err();
        assert_eq!(error.code(), code, "{error}");
        // 不监听、不写握手行，在 stderr 记一条 error 日志（handshake.md §1）
        let lines = rejected.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!((lines[0]["level"].as_str(), lines[0]["code"].as_str()), (Some("error"), Some(code)));
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
    // 恰好一个值才算：两个值即使有一个对（不论先后）、两个都对也拒绝
    let wrong: &[&[&str]] = &[&[], &["not-the-session-token"], &[&TOKEN[1..]], &[TOKEN, "x"], &["x", TOKEN], &[TOKEN, TOKEN]];
    for tokens in wrong {
        let mut client = client_with(running.endpoint(), tokens).await;
        assert_eq!(client.handshake(handshake_request(vec![1])).await.unwrap_err().code(), Code::Unauthenticated, "{tokens:?}");
        assert_eq!(client.health(v1::HealthRequest {}).await.unwrap_err().code(), Code::Unauthenticated);
        assert_eq!(client.watch_health(v1::HealthRequest {}).await.unwrap_err().code(), Code::Unauthenticated);
        let invoke = client.invoke(v1::InvokeRequest { operation: "orders.create".to_owned(), payload: Vec::new(), metadata: None }).await;
        assert_eq!(invoke.unwrap_err().code(), Code::Unauthenticated);
        // 令牌先于次序核对（握手之前的 Configure 与 Stop 本来是 FailedPrecondition）与流式调用的第一帧
        assert_eq!(client.configure(configure_request("{}", &[])).await.unwrap_err().code(), Code::Unauthenticated);
        assert_eq!(client.stop(v1::StopRequest { grace_seconds: 1, reason: "unload".to_owned() }).await.unwrap_err().code(), Code::Unauthenticated);
        let frames = tokio_stream::iter(vec![v1::StreamFrame { frame: Some(v1::stream_frame::Frame::Chunk(b"{}".to_vec())) }]);
        assert_eq!(client.invoke_stream(frames).await.unwrap_err().code(), Code::Unauthenticated);
    }
    assert_eq!(running.state(), LifecycleState::Declared, "refused calls do not run");

    let mut client = connect(running.endpoint()).await;
    let refused = client.handshake(handshake_request(vec![2, 3])).await.unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition, "no common protocol version");
    client.handshake(handshake_request(vec![1])).await.unwrap();
    assert_eq!(running.state(), LifecycleState::Resolved);
    running.shutdown();
    tokio::time::timeout(Duration::from_secs(5), running.wait()).await.unwrap().unwrap();
}

#[tokio::test]
async fn the_handshake_declares_what_the_plugin_serves() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    // 清单原文开头的 BOM 不随握手发送（handshake.md §3 第 3 条）
    let orders = orders_plugin(&recorder, &log);
    let running = Plugin::new(format!("\u{FEFF}{MANIFEST}"))
        .with_operations(registry_with("orders.create"))
        .with_network(Network::Tcp)
        .with_logger(Logger::new(log.clone()))
        .start(Launch::new(TOKEN))
        .await
        .unwrap();
    let mut bom_client = connect(running.endpoint()).await;
    assert_eq!(bom_client.handshake(handshake_request(vec![1])).await.unwrap().into_inner().manifest, MANIFEST);
    running.shutdown();
    running.wait().await.unwrap();

    let running = orders.start(Launch::new(TOKEN)).await.unwrap();
    assert!(running.handshake_line().starts_with("xihan-plugin/1 tcp 127.0.0.1:"));
    let mut client = connect(running.endpoint()).await;
    let handshake = client.handshake(handshake_request(vec![1, 2])).await.unwrap().into_inner();
    // primitives 写清单的值，manifest 是清单原文
    assert_eq!((handshake.protocol_version, handshake.primitives, handshake.manifest.as_str()), (1, 1, MANIFEST));
    let sdk = handshake.sdk.unwrap();
    assert_eq!((sdk.name.as_str(), sdk.language.as_str()), ("framework-sdk-rust", "rust"));
    // 只声明一元操作，按 ID 排序，带契约与 Schema
    let ids: Vec<&str> = handshake.operations.iter().map(|operation| operation.id.as_str()).collect();
    assert_eq!(ids, ["orders.create", "orders.panic", "orders.publish", "orders.relay", "orders.slow"]);
    assert!(handshake.operations.iter().all(|operation| !operation.streaming));
    let create = &handshake.operations[0];
    assert_eq!((create.stability.as_str(), create.expose.as_slice(), create.permission.as_str()), ("stable", &["http".to_owned()][..], "orders.write"));
    assert_eq!((create.request_schema.as_str(), create.response_schema.as_str()), (r#"{"type":"object","required":["quantity"]}"#, r#"{"type":"object"}"#));
    // 订阅按声明顺序，方式总是写出
    let subscriptions: Vec<(&str, &str, &str)> = handshake
        .subscriptions
        .iter()
        .map(|subscription| (subscription.topic.as_str(), subscription.message_type.as_str(), subscription.mode.as_str()))
        .collect();
    assert_eq!(
        subscriptions,
        [("orders.events", "", "competing"), ("orders.events", "orders.cleared", "broadcast"), ("orders.events", "orders.created", "competing")]
    );
    // 只发插件自己的目录，运行时的目录宿主已有
    let catalogs: Vec<(&str, usize)> = handshake.catalogs.iter().map(|catalog| (catalog.name.as_str(), catalog.locales.len())).collect();
    assert_eq!(catalogs, [("orders", 1)]);
    running.shutdown();
    running.wait().await.unwrap();
}

// ---------------------------------------------------------------------------
// 生命周期（lifecycle.md §7）
// ---------------------------------------------------------------------------

/// 一次被拒的调用的 gRPC 状态码。
fn precondition<T>(result: std::result::Result<Response<T>, Status>) -> Code {
    result.map(drop).unwrap_err().code()
}

#[tokio::test]
async fn lifecycle_calls_follow_the_order_table() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = connect(running.endpoint()).await;

    // 握手之前：Configure、Start、Stop 都不在表里；Health 报本地状态、不调健康回调
    assert_eq!(precondition(client.configure(configure_request("{}", &[])).await), Code::FailedPrecondition);
    assert_eq!(precondition(client.start(v1::StartRequest {}).await), Code::FailedPrecondition);
    assert_eq!(precondition(client.stop(v1::StopRequest { grace_seconds: 1, reason: "unload".to_owned() }).await), Code::FailedPrecondition);
    let declared = health(&mut client).await;
    assert_eq!((declared.state, declared.items.len()), (LifecycleState::Declared.value(), 0));

    client.handshake(handshake_request(vec![1])).await.unwrap();
    assert_eq!(precondition(client.handshake(handshake_request(vec![1])).await), Code::FailedPrecondition, "a second Handshake");
    assert_eq!(precondition(client.start(v1::StartRequest {}).await), Code::FailedPrecondition, "Start without Configure");
    let resolved = health(&mut client).await;
    assert_eq!((resolved.state, resolved.items.len()), (LifecycleState::Resolved.value(), 0));

    let configured = client.configure(configure_request(r#"{"region":"cn"}"#, &[])).await.unwrap().into_inner();
    assert!(configured.status.unwrap().ok);
    assert_eq!(precondition(client.configure(configure_request("{}", &[])).await), Code::FailedPrecondition, "a second Configure");
    assert_eq!(precondition(client.handshake(handshake_request(vec![1])).await), Code::FailedPrecondition);
    let after_configure = health(&mut client).await;
    assert_eq!((after_configure.state, after_configure.items.len()), (LifecycleState::Configured.value(), 0));
    assert_eq!(recorder.health_calls.load(Ordering::SeqCst), 0, "the health hook is not called before Start");

    // Start 应答时已经求过一次健康：之后是 Ready
    let started = client.start(v1::StartRequest {}).await.unwrap().into_inner();
    assert!(started.status.unwrap().ok);
    assert_eq!(recorder.health_calls.load(Ordering::SeqCst), 1);
    assert_eq!(running.state(), LifecycleState::Ready);
    assert_eq!(precondition(client.start(v1::StartRequest {}).await), Code::FailedPrecondition, "a second Start");
    stop(&mut client, running).await;
}

#[tokio::test]
async fn configuration_sections_must_be_objects() {
    for (section, reason) in [("[]", "the configuration is not a JSON object"), (r#"{"a":"#, "the configuration is not a JSON object")] {
        let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
        let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
        let mut client = connect(running.endpoint()).await;
        client.handshake(handshake_request(vec![1])).await.unwrap();
        let configured = client.configure(configure_request(section, &[])).await.unwrap().into_inner();
        assert_eq!(
            keyed(configured.status.as_ref()),
            ("PLUGINS_UNEXPECTED_001".to_owned(), "plugins.process.failed".to_owned(), json!({"id": ID, "phase": "configure", "reason": reason}))
        );
        assert!(recorder.configuration.lock().unwrap().is_none(), "the configure hook is not called");
        let failed = health(&mut client).await;
        assert_eq!((failed.state, failed.items.len()), (LifecycleState::Failed.value(), 0));
        // 启动钩子没成功过：不调停止钩子，应答 ok，退出
        stop(&mut client, running).await;
        assert!(recorder.stops.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn configuration_sections_are_read_like_the_host_writes_them() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = connect(running.endpoint()).await;
    client.handshake(handshake_request(vec![1])).await.unwrap();
    // 宿主写来的叶子都是字符串、成员名的大小写与字段不同（lifecycle.md §2、§7.2）
    let section = r#"{"Region":"cn","LIMIT":"12","enabled":"TRUE","Ratio":"-2.5e-1","extra":"x"}"#;
    let configured = client.configure(configure_request(section, &[plugin::PERMISSION_PIPELINE])).await.unwrap().into_inner();
    assert!(configured.status.unwrap().ok);
    assert_eq!(
        recorder.settings.lock().unwrap().clone(),
        Some(Settings { region: Some("cn".to_owned()), limit: Some(12), enabled: Some(true), ratio: Some(-0.25) })
    );
    let (raw, granted, work_directory) = recorder.configuration.lock().unwrap().clone().unwrap();
    assert_eq!((raw.as_str(), granted.as_slice(), work_directory), (section, &[plugin::PERMISSION_PIPELINE.to_owned()][..], std::env::temp_dir()));
    running.shutdown();
    running.wait().await.unwrap();

    // 空的配置节等同 {}；不合 JSON 数字文法的字符串读不成数
    for (section, expected) in [("", Ok("{}")), (r#"{"limit":" 12"}"#, Err("$.limit")), (r#"{"enabled":"yes"}"#, Err("$.enabled"))] {
        let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
        let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
        let mut client = connect(running.endpoint()).await;
        client.handshake(handshake_request(vec![1])).await.unwrap();
        let status = client.configure(configure_request(section, &[])).await.unwrap().into_inner().status.unwrap();
        match expected {
            Ok(raw) => {
                assert!(status.ok, "{section:?}");
                assert_eq!(recorder.configuration.lock().unwrap().as_ref().unwrap().0, raw);
            }
            Err(path) => assert!(failure(Some(&status)).text.contains(path), "{section:?}: {status:?}"),
        }
        running.shutdown();
        running.wait().await.unwrap();
    }
}

#[tokio::test]
async fn hook_failures_leave_the_plugin_failed() {
    // 配置钩子返回失败：原样带回，进入 Failed
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = connect(running.endpoint()).await;
    client.handshake(handshake_request(vec![1])).await.unwrap();
    let configured = client.configure(configure_request(r#"{"fail":true}"#, &[])).await.unwrap().into_inner();
    assert_eq!(keyed(configured.status.as_ref()), ("ORDERS_VALIDATION_009".to_owned(), "orders.configuration.invalid".to_owned(), Value::Null));
    assert_eq!(running.state(), LifecycleState::Failed);
    let late = invoke(&mut client, "orders.create", r#"{"quantity":1}"#, None).await;
    assert_eq!(keyed(late.status.as_ref()).2, json!({"id": ID, "state": "Failed"}));
    stop(&mut client, running).await;
    assert!(recorder.stops.lock().unwrap().is_empty(), "a plugin that never started has nothing to release");

    // 配置钩子 panic：reason 是 panic，panic 的内容只进 stderr（lifecycle.md §7.6）
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = connect(running.endpoint()).await;
    client.handshake(handshake_request(vec![1])).await.unwrap();
    let configured = client.configure(configure_request(r#"{"panic":true}"#, &[])).await.unwrap().into_inner();
    assert_eq!(keyed(configured.status.as_ref()).2, json!({"id": ID, "phase": "configure", "reason": "panic"}));
    assert!(log.text().contains("/etc/orders"), "the panic text goes to stderr");
    stop(&mut client, running).await;

    // 启动钩子失败与 panic
    for (mode, expected) in [
        (Mode::Fail, ("ORDERS_UNAVAILABLE_002".to_owned(), "orders.database.down".to_owned(), Value::Null)),
        (Mode::Panic, ("PLUGINS_UNEXPECTED_001".to_owned(), "plugins.process.failed".to_owned(), json!({"id": ID, "phase": "start", "reason": "panic"}))),
    ] {
        let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
        *recorder.start.lock().unwrap() = mode;
        let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
        let mut client = connect(running.endpoint()).await;
        client.handshake(handshake_request(vec![1])).await.unwrap();
        assert!(client.configure(configure_request("{}", &[])).await.unwrap().into_inner().status.unwrap().ok);
        let started = client.start(v1::StartRequest {}).await.unwrap().into_inner();
        assert_eq!(keyed(started.status.as_ref()), expected);
        assert_eq!(running.state(), LifecycleState::Failed);
        stop(&mut client, running).await;
        assert!(recorder.stops.lock().unwrap().is_empty(), "the start hook never succeeded");
    }
}

#[tokio::test]
async fn start_settles_the_state_by_the_health_hook() {
    for (mode, expected) in [
        (HealthMode::Absent, LifecycleState::Ready),
        (HealthMode::Report(Health::new(LifecycleState::Degraded)), LifecycleState::Degraded),
        (HealthMode::Report(Health::new(LifecycleState::Failed)), LifecycleState::Failed),
        (HealthMode::Report(Health::new(LifecycleState::Draining)), LifecycleState::Ready),
        (HealthMode::Panic, LifecycleState::Degraded),
    ] {
        let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
        *recorder.health.lock().unwrap() = mode;
        let (running, mut client) = serving(&recorder, &log, Launch::new(TOKEN)).await;
        assert_eq!(running.state(), expected);
        let response = invoke(&mut client, "orders.create", r#"{"quantity":1}"#, None).await;
        if expected.is_serving() {
            assert!(response.status.unwrap().ok);
        } else {
            assert_eq!(
                keyed(response.status.as_ref()),
                ("PLUGINS_UNAVAILABLE_001".to_owned(), "plugins.process.unavailable".to_owned(), json!({"id": ID, "state": "Failed"}))
            );
        }
        // 启动成功过、之后进入 Failed 的同样调停止钩子
        stop(&mut client, running).await;
        assert_eq!(recorder.stops.lock().unwrap().as_slice(), ["unload"]);
    }
}

#[tokio::test]
async fn health_reports_follow_the_hook_while_serving() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let (running, mut client) = serving(&recorder, &log, Launch::new(TOKEN)).await;
    let calls = || recorder.health_calls.load(Ordering::SeqCst);

    // 每次 Health 求一次回调；没声明的健康项丢掉，同名只告警一次
    let before = calls();
    let reported = health(&mut client).await;
    assert_eq!(calls(), before + 1);
    assert_eq!(reported.state, LifecycleState::Ready.value());
    let items: Vec<(&str, i32, &str)> = reported.items.iter().map(|item| (item.name.as_str(), item.status, item.description.as_str())).collect();
    assert_eq!(items, [("main", 1, "all good")]);
    health(&mut client).await;
    assert_eq!(log.lines().iter().filter(|line| line["item"] == "ghost" && line["level"] == "warning").count(), 1);

    // 回调 panic：这一次的结果是 Degraded、没有健康项，状态与回调报了 Degraded 一样迁到 Degraded；之后的结果照常决定状态
    *recorder.health.lock().unwrap() = HealthMode::Panic;
    let panicked = health(&mut client).await;
    assert_eq!((panicked.state, panicked.items.len()), (LifecycleState::Degraded.value(), 0));
    let error = panicked.error.unwrap();
    assert_eq!((error.code.as_str(), error.message_key.as_str(), error.text.as_str()), ("PLUGINS_UNEXPECTED_001", "plugins.process.failed", ""));
    assert_eq!(serde_json::from_str::<Value>(&error.details).unwrap(), json!({"id": ID, "phase": "health", "reason": "panic"}));
    assert_eq!(running.state(), LifecycleState::Degraded);
    *recorder.health.lock().unwrap() = HealthMode::Report(ready());
    assert_eq!(health(&mut client).await.state, LifecycleState::Ready.value());
    assert_eq!(running.state(), LifecycleState::Ready);

    // 回调报的 Degraded、Ready 照办；Failed 之后不再回来，也不再调回调
    *recorder.health.lock().unwrap() = HealthMode::Report(Health::new(LifecycleState::Degraded));
    assert_eq!(health(&mut client).await.state, LifecycleState::Degraded.value());
    *recorder.health.lock().unwrap() = HealthMode::Report(Health::new(LifecycleState::Ready));
    assert_eq!(health(&mut client).await.state, LifecycleState::Ready.value());
    let error = Error::unavailable("ORDERS_UNAVAILABLE_002", "orders.database.down");
    *recorder.health.lock().unwrap() = HealthMode::Report(Health::new(LifecycleState::Failed).with_error(error));
    let failed = health(&mut client).await;
    assert_eq!((failed.state, failed.error.unwrap().code.as_str()), (LifecycleState::Failed.value(), "ORDERS_UNAVAILABLE_002"));
    *recorder.health.lock().unwrap() = HealthMode::Report(Health::new(LifecycleState::Ready));
    let after = calls();
    assert_eq!(health(&mut client).await.state, LifecycleState::Failed.value());
    assert_eq!(calls(), after, "a failed plugin does not call the health hook");
    stop(&mut client, running).await;
}

#[tokio::test]
async fn a_plugin_runs_from_handshake_to_stop() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = connect(running.endpoint()).await;
    client.handshake(handshake_request(vec![1])).await.unwrap();

    // 未声明的操作先报 Unimplemented；Start 之前的调用不可用（invocation.md §1）
    let unknown = client.invoke(v1::InvokeRequest { operation: "orders.missing".to_owned(), payload: Vec::new(), metadata: None }).await;
    assert_eq!(unknown.unwrap_err().code(), Code::Unimplemented);
    let early = invoke(&mut client, "orders.create", r#"{"quantity":1}"#, None).await;
    assert_eq!(
        keyed(early.status.as_ref()),
        ("PLUGINS_UNAVAILABLE_001".to_owned(), "plugins.process.unavailable".to_owned(), json!({"id": ID, "state": "Resolved"}))
    );

    assert!(client.configure(configure_request(r#"{"region":"cn"}"#, &[])).await.unwrap().into_inner().status.unwrap().ok);
    let configured = invoke(&mut client, "orders.create", r#"{"quantity":1}"#, None).await;
    assert_eq!(keyed(configured.status.as_ref()).2, json!({"id": ID, "state": "Configured"}));
    assert!(client.start(v1::StartRequest {}).await.unwrap().into_inner().status.unwrap().ok);
    assert_eq!(running.state(), LifecycleState::Ready);

    // WatchHealth 先推当前状态
    let mut watch = client.watch_health(v1::HealthRequest {}).await.unwrap().into_inner();
    assert_eq!(watch.message().await.unwrap().unwrap().state, LifecycleState::Ready.value());

    stop(&mut client, running).await;
    // 停止时 WatchHealth 推一次 Draining，应答 Stop 后结束
    let mut last = None;
    while let Ok(Some(message)) = watch.message().await {
        last = Some(message.state);
    }
    assert_eq!(last, Some(LifecycleState::Draining.value()));
    assert_eq!(recorder.stops.lock().unwrap().as_slice(), ["unload"]);

    // 日志是一行一个 JSON 对象，每行都带清单 ID；没有回调端点时启动记一条 warning（handshake.md §4）
    let lines = log.lines();
    assert!(lines.iter().all(|line| line["plugin"] == ID && line["level"].is_string() && line["message"].is_string()));
    let messages: Vec<&str> = lines.iter().map(|line| line["message"].as_str().unwrap()).collect();
    for expected in ["listening", "handshake completed", "configured", "started", "stop requested", "stopped"] {
        assert!(messages.contains(&expected), "{expected} in {messages:?}");
    }
    assert!(lines.iter().any(|line| line["level"] == "warning" && line["message"].as_str().unwrap().contains("no supported callback endpoint")));
}

#[tokio::test]
async fn stop_drains_in_flight_calls_up_to_the_grace() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let (running, mut client) = serving(&recorder, &log, Launch::new(TOKEN)).await;

    let mut slow_client = client.clone();
    let slow = tokio::spawn(async move { invoke(&mut slow_client, "orders.slow", r#"{"millis":400}"#, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut stop_client = client.clone();
    let stopping = tokio::spawn(async move { stop_client.stop(v1::StopRequest { grace_seconds: 5, reason: "shutdown".to_owned() }).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // 排空中：新调用被拒，停止中再 Stop 不在次序表里，在途调用照常完成，Stop 等它完成才应答
    let refused = invoke(&mut client, "orders.create", r#"{"quantity":1}"#, None).await;
    assert_eq!(keyed(refused.status.as_ref()).2["state"], "Draining");
    let again = client.stop(v1::StopRequest { grace_seconds: 5, reason: "unload".to_owned() }).await;
    assert_eq!(again.unwrap_err().code(), Code::FailedPrecondition);
    assert!(!stopping.is_finished(), "Stop waits for the call in flight");
    assert_eq!(payload(&slow.await.unwrap()), json!({"cancelled": false}));
    assert!(stopping.await.unwrap().unwrap().into_inner().status.unwrap().ok);
    tokio::time::timeout(Duration::from_secs(5), running.wait()).await.unwrap().unwrap();
    assert_eq!(recorder.stops.lock().unwrap().as_slice(), ["shutdown"]);

    // 宽限到了：触发在途调用的取消令牌、不再等，再调停止钩子
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let (running, mut client) = serving(&recorder, &log, Launch::new(TOKEN)).await;
    let mut slow_client = client.clone();
    let slow = tokio::spawn(async move { invoke(&mut slow_client, "orders.slow", r#"{"millis":20000}"#, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    let stopped = client.stop(v1::StopRequest { grace_seconds: 1, reason: "unload".to_owned() }).await.unwrap().into_inner();
    assert!(stopped.status.unwrap().ok);
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert_eq!(payload(&slow.await.unwrap()), json!({"cancelled": true}));
    tokio::time::timeout(Duration::from_secs(5), running.wait()).await.unwrap().unwrap();
    assert_eq!(recorder.stops.lock().unwrap().as_slice(), ["unload"]);
}

#[tokio::test]
async fn stop_reports_the_stop_hook_and_still_exits() {
    for (mode, expected) in [
        (Mode::Fail, ("ORDERS_UNAVAILABLE_003".to_owned(), "orders.flush.failed".to_owned(), Value::Null)),
        (Mode::Panic, ("PLUGINS_UNEXPECTED_001".to_owned(), "plugins.process.failed".to_owned(), json!({"id": ID, "phase": "stop", "reason": "panic"}))),
    ] {
        let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
        *recorder.stop.lock().unwrap() = mode;
        let (running, mut client) = serving(&recorder, &log, Launch::new(TOKEN)).await;
        // grace_seconds 为 0 按 5 秒；没有在途调用时不等
        let stopped = client.stop(v1::StopRequest { grace_seconds: 0, reason: "restart".to_owned() }).await.unwrap().into_inner();
        assert_eq!(keyed(stopped.status.as_ref()), expected);
        tokio::time::timeout(Duration::from_secs(5), running.wait()).await.unwrap().unwrap();
        // 停止钩子最多一次：收尾时不再调
        assert_eq!(recorder.stops.lock().unwrap().as_slice(), ["restart"]);
    }

    // 握手之后直接 Stop（探测启动）：不调停止钩子，应答 ok
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = connect(running.endpoint()).await;
    client.handshake(handshake_request(vec![1])).await.unwrap();
    let stopped = client.stop(v1::StopRequest { grace_seconds: 0, reason: "unload".to_owned() }).await.unwrap().into_inner();
    assert!(stopped.status.unwrap().ok);
    tokio::time::timeout(Duration::from_secs(5), running.wait()).await.unwrap().unwrap();
    assert!(recorder.stops.lock().unwrap().is_empty());
}

#[tokio::test]
async fn shutdown_cancels_in_flight_calls_and_calls_the_stop_hook_once() {
    // 收尾（与 stdin 关闭、SIGTERM 相同）：可服务的进入 Draining，立即触发在途调用的取消令牌，停止钩子的 reason 是 shutdown
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let (running, client) = serving(&recorder, &log, Launch::new(TOKEN)).await;
    let mut slow_client = client.clone();
    let slow = tokio::spawn(async move { invoke(&mut slow_client, "orders.slow", r#"{"millis":20000}"#, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    running.shutdown();
    tokio::time::timeout(Duration::from_secs(3), running.wait()).await.expect("the shutdown finishes within 3 seconds").unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(payload(&slow.await.unwrap()), json!({"cancelled": true}));
    assert_eq!(recorder.stops.lock().unwrap().as_slice(), ["shutdown"]);

    // 正在处理的 Stop（宽限很长）同样受收尾的约束：在途调用立即取消，停止钩子只调一次
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let (running, mut client) = serving(&recorder, &log, Launch::new(TOKEN)).await;
    let mut slow_client = client.clone();
    let slow = tokio::spawn(async move { invoke(&mut slow_client, "orders.slow", r#"{"millis":20000}"#, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stopping = tokio::spawn(async move { client.stop(v1::StopRequest { grace_seconds: 30, reason: "unload".to_owned() }).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    running.shutdown();
    tokio::time::timeout(Duration::from_secs(3), running.wait()).await.expect("the shutdown finishes within 3 seconds").unwrap();
    assert_eq!(payload(&slow.await.unwrap()), json!({"cancelled": true}));
    assert_eq!(recorder.stops.lock().unwrap().len(), 1);
    let _ = stopping.await;

    // 启动之前收尾：不调停止钩子
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    running.shutdown();
    tokio::time::timeout(Duration::from_secs(3), running.wait()).await.unwrap().unwrap();
    assert!(recorder.stops.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// 调用与投递（invocation.md §1–§4）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invocations_restore_the_context_and_carry_errors() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let (running, mut client) = serving(&recorder, &log, Launch::new(TOKEN)).await;

    let metadata = v1::InvocationMetadata {
        trace_parent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_owned(),
        trace_state: "vendor=1".to_owned(),
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
    // 没有元数据、traceparent 或 deadline 不合文法：新的追踪，没有截止时间（客户端没设 gRPC 截止时间）
    let invalid = v1::InvocationMetadata { trace_parent: "00-xyz".to_owned(), deadline: "2026-11-18T03:25:00".to_owned(), ..Default::default() };
    for metadata in [None, Some(invalid)] {
        let fresh = payload(&invoke(&mut client, "orders.create", r#"{"quantity":1}"#, metadata).await);
        assert_eq!((fresh["tenant"].clone(), fresh["deadline"].clone()), (Value::Null, json!(false)));
        assert_eq!(fresh["trace_id"].as_str().unwrap().len(), 32);
        assert_ne!(fresh["trace_id"], "0af7651916cd43dd8448eb211c80319c");
    }

    // 业务失败走 Status.error：带键的错误 text 留空、details 是 JSON 对象；自由文本错误写 text；gRPC 状态是 OK
    let rejected = invoke(&mut client, "orders.create", r#"{"quantity":0}"#, None).await;
    assert_eq!(failure(rejected.status.as_ref()).details, r#"{"quantity":0}"#);
    assert_eq!(keyed(rejected.status.as_ref()), ("ORDERS_VALIDATION_001".to_owned(), "orders.quantity.invalid".to_owned(), json!({"quantity": 0})));
    let literal = invoke(&mut client, "orders.create", r#"{"quantity":99}"#, None).await;
    let error = failure(literal.status.as_ref());
    assert_eq!(
        (error.code.as_str(), error.message_key.as_str(), error.text.as_str(), error.details.as_str()),
        ("ORDERS_CONFLICT_001", "", "The order changed.", "")
    );
    let invalid = invoke(&mut client, "orders.create", r#"{"quantity":"2"}"#, None).await;
    assert_eq!(
        keyed(invalid.status.as_ref()),
        ("KERNEL_VALIDATION_001".to_owned(), "kernel.operation.invalid_payload".to_owned(), json!({"id": "orders.create", "reason": "$.quantity"}))
    );
    // 空载荷等同 {}
    let panicked = invoke(&mut client, "orders.panic", "", None).await;
    assert_eq!(keyed(panicked.status.as_ref()).1, "kernel.invocation.unhandled_exception");
    stop(&mut client, running).await;
}

#[tokio::test]
async fn streams_are_refused_after_the_first_frame() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = connect(running.endpoint()).await;
    client.handshake(handshake_request(vec![1])).await.unwrap();
    let start = |operation: &str| v1::StreamFrame {
        frame: Some(v1::stream_frame::Frame::Start(v1::InvokeRequest { operation: operation.to_owned(), payload: Vec::new(), metadata: None })),
    };
    // 不论就绪与否、操作是否声明：第一帧是 start 为 Unimplemented，其余与没有帧为 InvalidArgument
    for ready in [false, true] {
        if ready {
            assert!(client.configure(configure_request("{}", &[])).await.unwrap().into_inner().status.unwrap().ok);
            assert!(client.start(v1::StartRequest {}).await.unwrap().into_inner().status.unwrap().ok);
        }
        let chunk = v1::StreamFrame { frame: Some(v1::stream_frame::Frame::Chunk(b"{}".to_vec())) };
        assert_eq!(client.invoke_stream(tokio_stream::iter(vec![chunk])).await.unwrap_err().code(), Code::InvalidArgument);
        assert_eq!(client.invoke_stream(tokio_stream::iter(Vec::new())).await.unwrap_err().code(), Code::InvalidArgument);
        assert_eq!(client.invoke_stream(tokio_stream::iter(vec![v1::StreamFrame { frame: None }])).await.unwrap_err().code(), Code::InvalidArgument);
        for operation in ["orders.create", "orders.missing"] {
            assert_eq!(client.invoke_stream(tokio_stream::iter(vec![start(operation)])).await.unwrap_err().code(), Code::Unimplemented, "{operation}");
        }
    }
    stop(&mut client, running).await;
}

#[tokio::test]
async fn events_are_checked_then_delivered_to_every_matching_subscription() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = connect(running.endpoint()).await;
    client.handshake(handshake_request(vec![1])).await.unwrap();

    // 次序（invocation.md §4）：信封与方式 → 订阅 → 可服务
    let mut without_envelope = delivery("x", "orders.events", "orders.created", "competing");
    without_envelope.envelope = None;
    assert_eq!(client.handle_event(without_envelope).await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(client.handle_event(delivery("x", "orders.events", "orders.created", "fanout")).await.unwrap_err().code(), Code::InvalidArgument);
    let unknown = client.handle_event(delivery("x", "orders.other", "orders.created", "")).await.unwrap().into_inner();
    let unsubscribed = json!({"id": ID, "topic": "orders.other", "type": "orders.created", "mode": "competing"});
    assert_eq!(keyed(unknown.status.as_ref()), ("KERNEL_NOTFOUND_001".to_owned(), "plugins.event.unsubscribed".to_owned(), unsubscribed));
    let early = client.handle_event(delivery("x", "orders.events", "orders.created", "competing")).await.unwrap().into_inner();
    assert_eq!(keyed(early.status.as_ref()).2, json!({"id": ID, "state": "Resolved"}));
    assert!(recorder.deliveries.lock().unwrap().is_empty());

    assert!(client.configure(configure_request("{}", &[])).await.unwrap().into_inner().status.unwrap().ok);
    assert!(client.start(v1::StartRequest {}).await.unwrap().into_inner().status.unwrap().ok);
    let mut outcomes = Vec::new();
    for request in [
        // （t，空）与（t，orders.created）都是竞争订阅：按声明顺序依次交给两者
        delivery("a", "orders.events", "orders.created", "competing"),
        delivery("b", "orders.events", "orders.cleared", "broadcast"),
        delivery("c", "orders.events", "orders.cleared", ""),
        delivery("d", "orders.events", "orders.created", "broadcast"),
        // 第一个失败即结果，后面的不再执行
        delivery("fail-e", "orders.events", "orders.created", "competing"),
        delivery("panic", "orders.events", "orders.other", "competing"),
    ] {
        let status = client.handle_event(request).await.unwrap().into_inner().status.unwrap();
        outcomes.push(if status.ok { json!("ok") } else { json!([keyed(Some(&status)).1, keyed(Some(&status)).2]) });
    }
    assert_eq!(
        outcomes,
        [
            json!("ok"),
            json!("ok"),
            json!("ok"),
            json!(["plugins.event.unsubscribed", {"id": ID, "topic": "orders.events", "type": "orders.created", "mode": "broadcast"}]),
            json!(["orders.event.rejected", null]),
            json!(["kernel.invocation.unhandled_exception", null]),
        ]
    );
    assert_eq!(
        recorder.deliveries.lock().unwrap().as_slice(),
        [
            "a competing first 2 Some(\"t1\")",
            "a competing second",
            "b broadcast broadcast",
            "c competing first 2 Some(\"t1\")",
            "fail-e competing first 2 Some(\"t1\")",
            "panic competing first 2 Some(\"t1\")",
        ]
    );
    stop(&mut client, running).await;
}

#[tokio::test]
async fn the_platform_endpoint_is_private() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).with_network(Network::platform_default()).start(Launch::new(TOKEN)).await.unwrap();
    assert_eq!(running.endpoint().network(), Network::platform_default());
    // 自己生成的地址不含空白（handshake.md §2）
    assert!(!running.endpoint().address().chars().any(plugin::is_whitespace));
    assert_eq!(plugin::parse_handshake_line(&running.handshake_line()).as_ref(), Some(running.endpoint()));
    #[cfg(unix)]
    let directory = {
        use std::os::unix::fs::PermissionsExt;
        let socket = std::path::Path::new(running.endpoint().address());
        let directory = socket.parent().unwrap().to_path_buf();
        assert_eq!(std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::symlink_metadata(socket).unwrap().permissions().mode() & 0o777, 0o600);
        directory
    };
    let mut client = connect(running.endpoint()).await;
    client.handshake(handshake_request(vec![1])).await.unwrap();
    stop(&mut client, running).await;
    #[cfg(unix)]
    assert!(!directory.exists(), "the private directory is removed after stopping");
}

// ---------------------------------------------------------------------------
// 回调（invocation.md §5）
// ---------------------------------------------------------------------------

/// 回调服务收到的一次请求：会话令牌、`grpc-timeout` 头与请求。
type Received<T> = (Option<String>, Option<String>, T);

/// 扮演宿主的回调服务：记下收到的请求，按操作 ID 或主题应答。
#[derive(Default)]
struct FakeHost {
    invokes: Mutex<Vec<Received<v1::InvokeRequest>>>,
    publishes: Mutex<Vec<Received<v1::PublishRequest>>>,
}

struct FakeHostService(Arc<FakeHost>);

fn header(metadata: &tonic::metadata::MetadataMap, name: &str) -> Option<String> {
    metadata.get(name).map(|value| value.to_str().unwrap().to_owned())
}

/// `status.<码>` 或 `status.<码>.quiet`（消息为空）：宿主以这个 gRPC 状态失败。
fn scripted_status(name: &str) -> Option<Status> {
    let rest = name.strip_prefix("status.")?;
    let (code, message) = match rest.strip_suffix(".quiet") {
        Some(code) => (code, ""),
        None => (rest, "boom"),
    };
    Some(Status::new(Code::from_i32(code.parse().unwrap()), message))
}

#[tonic::async_trait]
impl HostRpc for FakeHostService {
    async fn invoke(&self, request: Request<v1::InvokeRequest>) -> std::result::Result<Response<v1::InvokeResponse>, Status> {
        let (session, timeout) = (header(request.metadata(), plugin::SESSION_METADATA_KEY), header(request.metadata(), "grpc-timeout"));
        let request = request.into_inner();
        let operation = request.operation.clone();
        self.0.invokes.lock().unwrap().push((session, timeout, request));
        if let Some(status) = scripted_status(&operation) {
            return Err(status);
        }
        let failed = |error: Option<v1::Error>| Ok(Response::new(v1::InvokeResponse { status: Some(v1::Status { ok: false, error }), payload: Vec::new() }));
        match operation.as_str() {
            "host.echo" => {
                Ok(Response::new(v1::InvokeResponse { status: Some(v1::Status { ok: true, error: None }), payload: br#"{"echoed":true}"#.to_vec() }))
            }
            "host.conflict" => failed(Some(v1::Error {
                code: "ORDERS_CONFLICT_001".to_owned(),
                message_key: "orders.conflict".to_owned(),
                text: String::new(),
                details: r#"{"id":"o1"}"#.to_owned(),
                inner: None,
            })),
            "host.badcode" => failed(Some(v1::Error { code: "bad".to_owned(), message_key: "orders.conflict".to_owned(), ..Default::default() })),
            "host.noerror" => failed(None),
            "host.slow" => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Ok(Response::new(v1::InvokeResponse { status: Some(v1::Status { ok: true, error: None }), payload: b"{}".to_vec() }))
            }
            _ => Err(Status::unimplemented("no such operation")),
        }
    }

    async fn publish(&self, request: Request<v1::PublishRequest>) -> std::result::Result<Response<v1::PublishResponse>, Status> {
        let (session, timeout) = (header(request.metadata(), plugin::SESSION_METADATA_KEY), header(request.metadata(), "grpc-timeout"));
        let request = request.into_inner();
        let topic = request.envelopes.first().map(|envelope| envelope.topic.clone()).unwrap_or_default();
        let count = request.envelopes.len();
        self.0.publishes.lock().unwrap().push((session, timeout, request));
        if let Some(status) = scripted_status(&topic) {
            return Err(status);
        }
        let results = match topic.as_str() {
            "short" => vec![v1::Status { ok: true, error: None }; count - 1],
            "noerror" => vec![v1::Status { ok: false, error: None }; count],
            _ => vec![v1::Status { ok: true, error: None }; count],
        };
        Ok(Response::new(v1::PublishResponse { results }))
    }
}

/// 在回环 TCP 上起回调服务，返回 `XIHAN_PLUGIN_HOST_ADDRESS` 的写法：前面一项本平台连不上的，最后是回环 TCP（`handshake.md` §4）。
async fn fake_host(host: &Arc<FakeHost>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = HostServer::new(FakeHostService(Arc::clone(host)));
    tokio::spawn(tonic::transport::Server::builder().serve_with_incoming(service, tokio_stream::wrappers::TcpListenerStream::new(listener)));
    let unreachable = if cfg!(windows) { "unix /tmp/xihan-host/host.sock" } else { r"npipe \\.\pipe\xihan-host" };
    format!("{unreachable};tcp {address}")
}

/// 用宿主下发的环境变量起插件。
fn launch_with(host_address: &str) -> Launch {
    environment(&[(plugin::ENV_SESSION, TOKEN), (plugin::ENV_HOST_ADDRESS, host_address)]).unwrap()
}

/// 宿主交给插件的调用元数据：租户 t1、追踪、截止时间与 Baggage。
fn handed() -> v1::InvocationMetadata {
    v1::InvocationMetadata {
        trace_parent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_owned(),
        trace_state: "vendor=1".to_owned(),
        deadline: "2099-01-01T00:00:00Z".to_owned(),
        baggage: HashMap::from([("xihan.tenant".to_owned(), "t1".to_owned()), ("xihan.correlation".to_owned(), "c1".to_owned())]),
    }
}

/// `grpc-timeout` 头的时长（`<数字><单位>`）。
fn grpc_timeout(value: &str) -> Duration {
    let (digits, unit) = value.split_at(value.len() - 1);
    let amount: u64 = digits.parse().unwrap();
    match unit {
        "H" => Duration::from_secs(amount * 3600),
        "M" => Duration::from_secs(amount * 60),
        "S" => Duration::from_secs(amount),
        "m" => Duration::from_millis(amount),
        "u" => Duration::from_micros(amount),
        _ => Duration::from_nanos(amount),
    }
}

#[tokio::test]
async fn callbacks_carry_the_call_context_and_the_session() {
    let host = Arc::new(FakeHost::default());
    let address = fake_host(&host).await;
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let (running, mut client) = serving(&recorder, &log, launch_with(&address)).await;

    // 回调带会话令牌、当前 Span 的 traceparent、按 O 格式写的截止时间、已登记的 Baggage，trace_state 为空
    let relayed = payload(&invoke(&mut client, "orders.relay", r#"{"operation":"host.echo"}"#, Some(handed())).await);
    assert_eq!(relayed, json!({"payload": r#"{"echoed":true}"#}));
    {
        let invokes = host.invokes.lock().unwrap();
        let (session, timeout, request) = invokes.last().unwrap();
        assert_eq!(session.as_deref(), Some(TOKEN));
        assert!(timeout.is_some(), "the gRPC deadline follows the context");
        let metadata = request.metadata.as_ref().unwrap();
        assert_eq!(metadata.trace_parent, "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01");
        assert_eq!((metadata.deadline.as_str(), metadata.trace_state.as_str()), ("2099-01-01T00:00:00.0000000+00:00", ""));
        assert_eq!(metadata.baggage, HashMap::from([("xihan.tenant".to_owned(), "t1".to_owned()), ("xihan.correlation".to_owned(), "c1".to_owned())]));
    }

    // 生效的截止时间取 deadline 与 gRPC 截止时间中较早的：回调的 deadline 与 grpc-timeout 都是 30 秒左右，不是 2099 年
    let mut request =
        Request::new(v1::InvokeRequest { operation: "orders.relay".to_owned(), payload: br#"{"operation":"host.echo"}"#.to_vec(), metadata: Some(handed()) });
    request.set_timeout(Duration::from_secs(30));
    assert!(client.invoke(request).await.unwrap().into_inner().status.unwrap().ok);
    {
        let invokes = host.invokes.lock().unwrap();
        let (_, timeout, request) = invokes.last().unwrap();
        let remaining = grpc_timeout(timeout.as_deref().unwrap());
        assert!(remaining <= Duration::from_secs(30) && remaining > Duration::from_secs(20), "{remaining:?}");
        let deadline = &request.metadata.as_ref().unwrap().deadline;
        // UTC、七位小数、+00:00
        assert!(!deadline.starts_with("2099") && deadline.len() == 33 && deadline.ends_with("+00:00") && deadline.as_bytes()[19] == b'.', "{deadline}");
    }
    stop(&mut client, running).await;
}

#[tokio::test]
async fn callbacks_are_checked_locally_before_sending() {
    let host = Arc::new(FakeHost::default());
    let address = fake_host(&host).await;
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(launch_with(&address)).await.unwrap();
    let mut client = connect(running.endpoint()).await;
    // 宿主只授予了发布：调用宿主的操作在本地就被拒
    open(&mut client, &[plugin::PERMISSION_MESSAGING]).await;
    let relayed = invoke(&mut client, "orders.relay", r#"{"operation":"host.echo"}"#, None).await;
    assert_eq!(
        keyed(relayed.status.as_ref()),
        ("PLUGINS_DENIED_002".to_owned(), "plugins.callback.denied".to_owned(), json!({"id": ID, "permission": plugin::PERMISSION_PIPELINE}))
    );
    assert!(host.invokes.lock().unwrap().is_empty());

    // 租户：只能是宿主此刻交给插件的；越界的在本地拒绝、不发出
    let foreign = invoke(&mut client, "orders.publish", r#"{"tenant":"t2","envelopes":[null,null]}"#, Some(handed())).await;
    let denied = json!({"code": "PLUGINS_DENIED_002", "key": "plugins.callback.tenant_denied", "details": {"id": ID, "tenant": "t2"}});
    assert_eq!(payload(&foreign), json!({"outcomes": [denied, denied]}), "the tenant check covers the whole batch");
    assert!(host.publishes.lock().unwrap().is_empty());
    // 信封的 xihantenant 不在本地核对，交给宿主逐项判定
    let enveloped = invoke(&mut client, "orders.publish", r#"{"envelopes":[null,"t1","t9"]}"#, Some(handed())).await;
    assert_eq!(payload(&enveloped), json!({"outcomes": ["ok", "ok", "ok"]}));
    {
        let publishes = host.publishes.lock().unwrap();
        let (session, _, request) = publishes.last().unwrap();
        assert_eq!(session.as_deref(), Some(TOKEN));
        assert_eq!(request.envelopes.iter().map(|envelope| envelope.id.as_str()).collect::<Vec<_>>(), ["e0", "e1", "e2"]);
        assert_eq!(request.envelopes[2].extensions.get(plugin::TENANT_EXTENSION).map(String::as_str), Some("t9"));
        assert_eq!((request.envelopes[0].spec_version.as_str(), request.envelopes[0].data_content_type.as_str()), ("1.0", "application/json"));
        assert_eq!(request.metadata.as_ref().unwrap().baggage.get("xihan.tenant").map(String::as_str), Some("t1"));
    }
    // 调用结束之后租户不再是交给插件的租户：不带元数据的调用里带 t1 同样被拒
    let after = invoke(&mut client, "orders.publish", r#"{"tenant":"t1","envelopes":[null]}"#, None).await;
    assert_eq!(payload(&after)["outcomes"][0]["key"], "plugins.callback.tenant_denied");
    stop(&mut client, running).await;
}

#[tokio::test]
async fn callbacks_without_a_supported_endpoint_fail_without_sending() {
    // 只有本平台连不上的项、或变量不合法：照常服务，回调以 plugins.host.unavailable 失败
    let unreachable = if cfg!(windows) { "unix /tmp/xihan-host/host.sock" } else { r"npipe \\.\pipe\xihan-host" };
    for address in [unreachable, "tcp 10.0.0.1:5000"] {
        let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
        let (running, mut client) = serving(&recorder, &log, launch_with(address)).await;
        let reason = json!({"id": ID, "reason": "no supported callback endpoint"});
        let relayed = invoke(&mut client, "orders.relay", r#"{"operation":"host.echo"}"#, None).await;
        assert_eq!(keyed(relayed.status.as_ref()), ("PLUGINS_UNAVAILABLE_001".to_owned(), "plugins.host.unavailable".to_owned(), reason.clone()));
        let published = payload(&invoke(&mut client, "orders.publish", r#"{"envelopes":[null,null]}"#, None).await);
        let unavailable = json!({"code": "PLUGINS_UNAVAILABLE_001", "key": "plugins.host.unavailable", "details": reason});
        assert_eq!(published, json!({"outcomes": [unavailable, unavailable]}));
        assert!(log.lines().iter().any(|line| line["level"] == "warning" && line["message"].as_str().unwrap().contains("no supported callback endpoint")));
        stop(&mut client, running).await;
    }
}

#[tokio::test]
async fn callbacks_check_the_endpoint_first_and_do_not_send_abandoned_calls() {
    // 先看回调端点：没有可用端点时即使权限也没授予，也报 plugins.host.unavailable（invocation.md §5）
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(Launch::new(TOKEN)).await.unwrap();
    let mut client = connect(running.endpoint()).await;
    open(&mut client, &[]).await;
    let relayed = invoke(&mut client, "orders.relay", r#"{"operation":"host.echo"}"#, None).await;
    assert_eq!(keyed(relayed.status.as_ref()).1, "plugins.host.unavailable");
    stop(&mut client, running).await;

    let host = Arc::new(FakeHost::default());
    let address = fake_host(&host).await;
    // 再看权限与租户，最后才看上下文的截止时间与取消
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let running = orders_plugin(&recorder, &log).start(launch_with(&address)).await.unwrap();
    let mut client = connect(running.endpoint()).await;
    open(&mut client, &[plugin::PERMISSION_MESSAGING]).await;
    let denied = invoke(&mut client, "orders.relay", r#"{"operation":"host.echo","abandon":"cancel"}"#, None).await;
    assert_eq!(keyed(denied.status.as_ref()).1, "plugins.callback.denied");
    stop(&mut client, running).await;

    // 上下文的截止时间已过按 DeadlineExceeded、已被取消按「Cancelled，取消信号已触发」，都不发请求
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let (running, mut client) = serving(&recorder, &log, launch_with(&address)).await;
    let expired = invoke(&mut client, "orders.relay", r#"{"operation":"host.echo","abandon":"deadline"}"#, None).await;
    assert_eq!(keyed(expired.status.as_ref()), ("KERNEL_TIMEOUT_001".to_owned(), "kernel.invocation.deadline_exceeded".to_owned(), Value::Null));
    let cancelled = invoke(&mut client, "orders.relay", r#"{"operation":"host.echo","abandon":"cancel"}"#, None).await;
    assert_eq!(keyed(cancelled.status.as_ref()), ("KERNEL_CANCELLED_001".to_owned(), "kernel.invocation.cancelled".to_owned(), Value::Null));
    assert!(host.invokes.lock().unwrap().is_empty(), "abandoned callbacks are not sent");
    // 发出之后到了截止时间：tonic 以 Cancelled 报的 Timeout expired 按 DeadlineExceeded
    let late = invoke(&mut client, "orders.relay", r#"{"operation":"host.slow","abandon":"short"}"#, None).await;
    assert_eq!(keyed(late.status.as_ref()), ("KERNEL_TIMEOUT_001".to_owned(), "kernel.invocation.deadline_exceeded".to_owned(), Value::Null));
    assert_eq!(host.invokes.lock().unwrap().len(), 1);
    stop(&mut client, running).await;
}

#[tokio::test]
async fn callbacks_to_an_unreachable_host_are_unavailable() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    // 绑定后立即关掉的端口：连上去一定失败，按 Unavailable 映射
    let address = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let (running, mut client) = serving(&recorder, &log, launch_with(&format!("tcp {address}"))).await;
    let relayed = invoke(&mut client, "orders.relay", r#"{"operation":"host.echo"}"#, None).await;
    let (code, key, details) = keyed(relayed.status.as_ref());
    assert_eq!((code.as_str(), key.as_str(), details["id"].as_str()), ("PLUGINS_UNAVAILABLE_001", "plugins.host.unavailable", Some(ID)));
    assert!(details["reason"].as_str().unwrap().starts_with("Unavailable: "), "{details}");
    stop(&mut client, running).await;
}

#[derive(Deserialize)]
struct CallbackStatuses {
    cases: Vec<CallbackCase>,
}

#[derive(Debug, Deserialize)]
struct CallbackCase {
    rpc: String,
    #[serde(rename = "statusCode")]
    status_code: i32,
    message: String,
    #[serde(rename = "contextCancelled")]
    context_cancelled: bool,
    code: String,
    key: String,
    details: Value,
}

/// 回放 `plugin/callback-status.json` 里上下文没有取消的用例：假的宿主以那个 gRPC 状态应答，插件的处理器把映射出的错误原样返回。
/// 向量的插件 ID 与操作换成本测试的（`id` 是清单 ID，`Host.Invoke` 的 `phase` 与 `kernel.operation.*` 的 `id` 是操作 ID）。
#[tokio::test]
async fn callback_statuses_follow_the_vectors_end_to_end() {
    let vectors: Value = common::vector("plugin/callback-status.json");
    let (vector_id, vector_operation) = (vectors["pluginId"].as_str().unwrap().to_owned(), vectors["operation"].as_str().unwrap().to_owned());
    let cases: CallbackStatuses = serde_json::from_value(vectors).unwrap();
    let host = Arc::new(FakeHost::default());
    let address = fake_host(&host).await;
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let (running, mut client) = serving(&recorder, &log, launch_with(&address)).await;
    let mut replayed = 0;
    for case in cases.cases.iter().filter(|case| !case.context_cancelled) {
        let name = format!("status.{}{}", case.status_code, if case.message.is_empty() { ".quiet" } else { "" });
        assert!(case.message.is_empty() || case.message == "boom", "the fake host answers with 'boom': {case:?}");
        let mut details = case.details.clone();
        for value in details.as_object_mut().into_iter().flat_map(|members| members.values_mut()) {
            if *value == json!(vector_id) {
                *value = json!(ID);
            } else if *value == json!(vector_operation) {
                *value = json!(name);
            }
        }
        let expected = json!({"code": case.code, "key": case.key, "details": details});
        let actual = match case.rpc.as_str() {
            "invoke" => {
                let response = invoke(&mut client, "orders.relay", &json!({"operation": name}).to_string(), None).await;
                let (code, key, details) = keyed(response.status.as_ref());
                json!({"code": code, "key": key, "details": details})
            }
            _ => {
                let response = invoke(&mut client, "orders.publish", &json!({"topic": name, "envelopes": [null, null]}).to_string(), None).await;
                let outcomes = payload(&response)["outcomes"].clone();
                // RPC 失败时每一项都是同一个错误
                assert_eq!(outcomes[0], outcomes[1], "{case:?}");
                outcomes[0].clone()
            }
        };
        assert_eq!(actual, expected, "{case:?}");
        replayed += 1;
    }
    assert_eq!(replayed, cases.cases.iter().filter(|case| !case.context_cancelled).count());
    stop(&mut client, running).await;
}

#[tokio::test]
async fn host_results_are_read_by_the_rules() {
    let host = Arc::new(FakeHost::default());
    let address = fake_host(&host).await;
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let (running, mut client) = serving(&recorder, &log, launch_with(&address)).await;

    // 宿主的业务失败按 §3 读入：键与 details 原样
    let conflict = invoke(&mut client, "orders.relay", r#"{"operation":"host.conflict"}"#, None).await;
    assert_eq!(keyed(conflict.status.as_ref()), ("ORDERS_CONFLICT_001".to_owned(), "orders.conflict".to_owned(), json!({"id": "o1"})));
    // 错误码不合文法：整个错误换成 plugins.process.failed（phase 为 invoke）
    let bad = invoke(&mut client, "orders.relay", r#"{"operation":"host.badcode"}"#, None).await;
    assert_eq!(keyed(bad.status.as_ref()).2, json!({"id": ID, "phase": "invoke", "reason": "invalid error code 'bad'"}));
    // ok 为假却没有 error
    let missing = invoke(&mut client, "orders.relay", r#"{"operation":"host.noerror"}"#, None).await;
    assert_eq!(keyed(missing.status.as_ref()).2, json!({"id": ID, "phase": "host.noerror", "reason": "the host reported a failure without an error"}));
    let published = payload(&invoke(&mut client, "orders.publish", r#"{"topic":"noerror","envelopes":[null]}"#, None).await);
    assert_eq!(published["outcomes"][0]["details"], json!({"id": ID, "phase": "publish", "reason": "the host reported a failure without an error"}));
    // 结果数与信封数不同：每一项都是 plugins.process.failed
    let short = payload(&invoke(&mut client, "orders.publish", r#"{"topic":"short","envelopes":[null,null]}"#, None).await);
    let mismatch = json!({"code": "PLUGINS_UNEXPECTED_001", "key": "plugins.process.failed",
                          "details": {"id": ID, "phase": "publish", "reason": "the host returned 1 results for 2 envelopes"}});
    assert_eq!(short, json!({"outcomes": [mismatch, mismatch]}));
    stop(&mut client, running).await;
}

#[tokio::test]
async fn cancelled_calls_signal_the_token() {
    let (recorder, log) = (Arc::new(Recorder::default()), Buffer::default());
    let (running, mut client) = serving(&recorder, &log, Launch::new(TOKEN)).await;

    // gRPC 截止时间到了，客户端放弃调用：插件里的令牌触发，处理器提前返回
    let mut request = Request::new(v1::InvokeRequest { operation: "orders.slow".to_owned(), payload: br#"{"millis":10000}"#.to_vec(), metadata: None });
    request.set_timeout(Duration::from_millis(200));
    let started = Instant::now();
    // 客户端、tonic 服务端与插件的截止令牌各按 grpc-timeout 计时，先到的一方结束调用：插件的令牌先到时处理器照常返回自己的结果，
    // 调用以它结束（RFC-0003 规则 1）
    match client.invoke(request).await {
        Err(status) => assert!(matches!(status.code(), Code::DeadlineExceeded | Code::Cancelled), "{status:?}"),
        Ok(response) => assert_eq!(payload(&response.into_inner()), json!({"cancelled": true})),
    }
    // 停止等的是处理器真正结束：令牌触发后它很快返回，Stop 不必等满 10 秒
    stop(&mut client, running).await;
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
}
