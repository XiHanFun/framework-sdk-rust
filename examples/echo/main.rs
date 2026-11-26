//! 回声插件 echo：用本 SDK 写的进程外插件（RFC-0017），也是黑盒运行器的一致性测试对象（`tests/plugin_conformance.rs`）。
//!
//! - 操作：`echoes.create`（回声文本，附上配置里的问候语与上下文里的租户、主体、关联 ID）、`echoes.relay`（经 `Host.Invoke`
//!   回调宿主的另一个操作）、`echoes.publish`（经 `Host.Publish` 发布一条消息）、`echoes.crash`（故意让进程异常退出，测试时排除）、
//!   `events.list`（列出收到的事件）；
//! - 订阅：`echo.events` 竞争消费全部类型，同一主题的 `echo.cleared` 另以广播订阅（同一条消息以两种方式各到一次，按投递的方式区分）；
//! - 健康项 `echo`，配置节 `XiHan:Echo` 的 `greeting`，消息目录 `echo`（en-US、zh-CN）。
//!
//! 构建 `cargo build --example echo --features plugin`，把 `target/debug/examples/echo`（Windows 上是 `echo.exe`）与本目录的
//! `xihan.manifest.json` 放进同一个目录，就是一个插件目录。

use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use xihan_framework::BoxFuture;
use xihan_framework::invocation::{Context, new_trace_id};
use xihan_framework::operation::{self, Contract, EXPOSE_HTTP, EXPOSE_MCP, Registry, Schema};
use xihan_framework::pipeline;
use xihan_framework::plugin::{
    self, Configuration, Envelope, Event, Health, HealthItem, HealthStatus, Hooks, Level, Logger, MANIFEST_FILE_NAME, MessageCatalog, Plugin, Subscription,
    SubscriptionMode, event_handler_fn,
};
use xihan_framework::primitives::json::Json;
use xihan_framework::primitives::{Error, FrameworkError, LifecycleState, Result, Stability, set_default_messages};

/// 事件的来源（CloudEvents `source`）。
const SOURCE: &str = "urn:xihan:samples:echo-rust";

/// `echoes.create` 的请求。
#[derive(Deserialize)]
struct CreateRequest {
    text: String,
}

/// `echoes.create` 的响应：回声文本、配置里的问候语与上下文里的租户、主体、关联 ID。
#[derive(Serialize)]
struct CreateResponse {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    greeting: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tenant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    principal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    correlation: Option<String>,
}

/// `echoes.relay` 的请求：经宿主回调调用 `operation`，把宿主的响应原样返回。
#[derive(Deserialize)]
struct RelayRequest {
    operation: String,
    payload: Option<Json>,
}

/// `echoes.publish` 的请求：经宿主发布一条 `type` 类型的消息到 `topic`。
#[derive(Deserialize)]
struct PublishRequest {
    topic: String,
    #[serde(rename = "type")]
    event_type: String,
    data: Option<Json>,
}

/// `echoes.publish` 的响应：发布的消息 ID。
#[derive(Serialize)]
struct PublishResponse {
    id: String,
}

/// 没有成员的请求或响应。
#[derive(Clone, Deserialize, Serialize)]
struct Empty {}

/// `events.list` 的响应：收到的事件 ID，每次投递的「ID 方式」，与按收到先后列出的信封（黑盒运行器的用例 `event-envelope` 读它，
/// `invocation.md` §7）。
#[derive(Serialize)]
struct ListResponse {
    ids: Vec<String>,
    deliveries: Vec<String>,
    envelopes: Vec<ReceivedEnvelope>,
}

/// 处理器在一次投递里拿到的信封：ID 与 `dataschema`（没有时写 `null`）。
#[derive(Clone, Serialize)]
struct ReceivedEnvelope {
    id: String,
    #[serde(rename = "dataSchema")]
    data_schema: Option<String>,
}

/// 配置节 `XiHan:Echo`。
#[derive(Deserialize)]
struct Settings {
    greeting: Option<String>,
}

/// 插件的状态：配置的问候语与收到的事件。
#[derive(Default)]
struct Echo {
    greeting: Mutex<Option<String>>,
    deliveries: Mutex<Vec<(String, SubscriptionMode, Option<String>)>>,
}

impl Echo {
    fn greeting(&self) -> Option<String> {
        self.greeting.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn record(&self, event: &Event) {
        self.deliveries.lock().unwrap_or_else(PoisonError::into_inner).push((event.envelope.id.clone(), event.mode, event.envelope.data_schema.clone()));
    }

    fn list(&self) -> ListResponse {
        let deliveries = self.deliveries.lock().unwrap_or_else(PoisonError::into_inner);
        let mut ids: Vec<String> = deliveries.iter().map(|(id, _, _)| id.clone()).collect();
        ids.dedup();
        ListResponse {
            ids,
            deliveries: deliveries.iter().map(|(id, mode, _)| format!("{id} {mode}")).collect(),
            envelopes: deliveries.iter().map(|(id, _, data_schema)| ReceivedEnvelope { id: id.clone(), data_schema: data_schema.clone() }).collect(),
        }
    }
}

/// 生命周期钩子：读配置、报健康。
struct Lifecycle(Arc<Echo>);

impl Hooks for Lifecycle {
    fn configure<'a>(&'a self, configuration: &'a Configuration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let settings: Settings = configuration
                .decode()
                .map_err(|error| Error::validation("ECHO_VALIDATION_002", "echo.configuration.invalid").with_detail("reason", error.to_string()))?;
            *self.0.greeting.lock().unwrap_or_else(PoisonError::into_inner) = settings.greeting;
            Ok(())
        })
    }

    fn health(&self) -> Option<Health> {
        Some(Health::new(LifecycleState::Ready).with_item(HealthItem::new("echo", HealthStatus::Healthy)))
    }
}

/// 回声：文本为空是业务失败，宿主用握手时拿到的目录翻译它。
fn create(echo: &Echo, ctx: &Context, request: CreateRequest) -> Result<CreateResponse> {
    if request.text.is_empty() {
        return Err(Error::validation("ECHO_VALIDATION_001", "echo.text.empty"));
    }
    Ok(CreateResponse {
        text: request.text,
        greeting: echo.greeting(),
        tenant: ctx.tenant().map(str::to_owned),
        principal: ctx.principal().map(str::to_owned),
        correlation: ctx.correlation().map(str::to_owned),
    })
}

/// 经宿主回调调用另一个操作，把宿主的响应原样返回；宿主没有给出能连的回调端点时以 `plugins.host.unavailable` 失败。
async fn relay(ctx: &Context, request: RelayRequest) -> Result<Json> {
    let host = plugin::host(ctx).ok_or_else(|| Error::unavailable("ECHO_UNAVAILABLE_001", "echo.host.unavailable"))?;
    let payload = request.payload.map(|payload| payload.as_str().as_bytes().to_vec()).unwrap_or_default();
    let response = host.invoke(ctx, &request.operation, payload).await?;
    let text = if response.is_empty() { "{}".to_owned() } else { String::from_utf8_lossy(&response).into_owned() };
    Json::from_text(&text).map_err(|error| Error::literal("ECHO_UNEXPECTED_001", format!("The host returned a payload that is not JSON: {error}.")))
}

/// 经宿主发布一条消息。
async fn publish(ctx: &Context, request: PublishRequest) -> Result<PublishResponse> {
    let host = plugin::host(ctx).ok_or_else(|| Error::unavailable("ECHO_UNAVAILABLE_001", "echo.host.unavailable"))?;
    let id = new_trace_id();
    let mut envelope = Envelope::new(request.topic, id.as_str(), SOURCE, request.event_type);
    if let Some(data) = request.data {
        envelope = envelope.with_data(data.as_str().as_bytes().to_vec());
    }
    // 一个信封对应一个结果
    host.publish(ctx, vec![envelope]).await.remove(0)?;
    Ok(PublishResponse { id })
}

/// 故意让进程异常退出而不应答：宿主应记为崩溃并按退避重启（`lifecycle.md` §6）。
fn crash(logger: &Logger) -> Result<Empty> {
    logger.critical("crashing on request");
    std::process::exit(3)
}

/// 组装插件：消息目录、操作、订阅与钩子。
fn plugin(logger: Logger) -> std::result::Result<Plugin, FrameworkError> {
    let catalog = MessageCatalog::from_json("echo", [("en-US", include_str!("localization/en-US.json")), ("zh-CN", include_str!("localization/zh-CN.json"))])?;
    // 插件自己的 Error::message 也按目录解析
    set_default_messages(Some(Arc::new(catalog.to_catalog())), None);

    let echo = Arc::new(Echo::default());
    let registry = Arc::new(Registry::new());
    let state = Arc::clone(&echo);
    registry.add_with(
        Contract::new("echoes.create").with_stability(Stability::Experimental).with_expose([EXPOSE_HTTP, EXPOSE_MCP]),
        operation::handler_fn(move |ctx: &Context, request: CreateRequest| {
            let state = Arc::clone(&state);
            Box::pin(async move { create(&state, ctx, request) })
        }),
        Some(Schema::new(
            r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#,
            r#"{"type":"object","properties":{"text":{"type":"string"},"greeting":{"type":"string"},"tenant":{"type":"string"},"principal":{"type":"string"},"correlation":{"type":"string"}},"required":["text"]}"#,
        )),
        pipeline::APP_OWNER,
    )?;
    registry.add(
        Contract::new("echoes.relay").with_stability(Stability::Experimental).with_expose([EXPOSE_HTTP]),
        operation::handler_fn(|ctx: &Context, request: RelayRequest| Box::pin(relay(ctx, request))),
    )?;
    registry.add(
        Contract::new("echoes.publish").with_stability(Stability::Experimental).with_expose([EXPOSE_HTTP]),
        operation::handler_fn(|ctx: &Context, request: PublishRequest| Box::pin(publish(ctx, request))),
    )?;
    let crashing = logger.clone();
    registry.add(
        Contract::new("echoes.crash").with_stability(Stability::Internal),
        operation::handler_fn(move |_: &Context, _: Empty| {
            let logger = crashing.clone();
            Box::pin(async move { crash(&logger) })
        }),
    )?;
    let state = Arc::clone(&echo);
    registry.add(
        Contract::new("events.list").with_stability(Stability::Experimental).with_expose([EXPOSE_HTTP]),
        operation::handler_fn(move |_: &Context, _: Empty| {
            let state = Arc::clone(&state);
            Box::pin(async move { Ok(state.list()) })
        }),
    )?;

    let competing = Arc::clone(&echo);
    let broadcast = Arc::clone(&echo);
    Ok(Plugin::from_manifest_file(MANIFEST_FILE_NAME)?
        .with_operations(registry)
        .with_subscription(
            Subscription::new("echo.events"),
            event_handler_fn(move |_: &Context, event: Event| {
                competing.record(&event);
                Box::pin(async { Ok(()) })
            }),
        )
        .with_subscription(
            Subscription::new("echo.events").with_message_type("echo.cleared").with_mode(SubscriptionMode::Broadcast),
            event_handler_fn(move |_: &Context, event: Event| {
                broadcast.record(&event);
                Box::pin(async { Ok(()) })
            }),
        )
        .with_catalog(catalog)
        .with_hooks(Lifecycle(echo))
        .with_logger(logger))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let logger = Logger::stderr();
    let outcome = match plugin(logger.clone()) {
        Ok(plugin) => plugin.serve().await,
        Err(error) => Err(error),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            logger.log(Level::Critical, "the plugin failed", &[("error", error.to_string().into())]);
            ExitCode::FAILURE
        }
    }
}
