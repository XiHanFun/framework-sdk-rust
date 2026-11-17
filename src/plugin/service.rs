//! `xihan.plugin.v1.Plugin` 服务的实现（`handshake.md` §3、`lifecycle.md`、`invocation.md` §1–§4）。
//!
//! 会话令牌在进入这里之前已由拦截器核对。生命周期顺序不对的调用（`Configure` 早于握手、`Start` 早于 `Configure`、重复握手）以
//! gRPC `FailedPrecondition` 拒绝：它们是协议层的错误，不是业务失败。

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use super::convert::{context_of, envelope_of, error_status, ok_status, proto_health, status_of};
use super::host::HostHandle;
use super::log::Level;
use super::messages::failed;
use super::runtime::{Runtime, panic_reason};
use super::time::parse_grpc_timeout;
use super::types::{Binding, Configuration, Event, SubscriptionMode};
use super::v1::plugin_server::Plugin;
use super::{PROTOCOL_VERSIONS, SDK_LANGUAGE, negotiate, v1};
use crate::invocation::{CancellationToken, Context};
use crate::operation::{self, ERROR_CODE_UNEXPECTED, MESSAGE_UNHANDLED_PANIC};
use crate::primitives::{Error, LifecycleState, SDK_NAME, SDK_VERSION};

/// `WatchHealth` 重新取健康的间隔：钩子报的健康项变化最迟这么久之后推送。
const HEALTH_POLL: Duration = Duration::from_secs(1);

/// `Plugin` 服务。
pub(crate) struct PluginService {
    runtime: Arc<Runtime>,
}

impl PluginService {
    pub(crate) fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    /// 为一次调用或投递构造上下文，放入宿主客户端；返回的令牌在宿主取消这次 RPC（future 被丢弃）时触发。
    fn context(&self, metadata: Option<&v1::InvocationMetadata>, grpc_deadline: Option<SystemTime>) -> (Context, CancelOnDrop) {
        let token = CancellationToken::new();
        let ctx = context_of(metadata, grpc_deadline, &token);
        if let Some(host) = &self.runtime.host {
            ctx.insert_item(HostHandle(Arc::clone(host)));
        }
        (ctx, CancelOnDrop(token))
    }
}

/// 宿主取消 RPC 时 tonic 丢弃处理它的 future：丢弃这个守卫即触发上下文的取消令牌，让在独立任务里继续运行的处理器收到取消
/// （RFC-0003 规则 1：只经令牌通知，不中止处理器）。
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// gRPC 调用自己的截止时间（`grpc-timeout` 头）。
fn grpc_deadline<T>(request: &Request<T>) -> Option<SystemTime> {
    let timeout = request.metadata().get("grpc-timeout")?.to_str().ok()?;
    SystemTime::now().checked_add(parse_grpc_timeout(timeout)?)
}

/// 生命周期顺序不对的调用。
fn out_of_order(rpc: &str, state: LifecycleState) -> Status {
    Status::failed_precondition(format!("{rpc} is not allowed in the state {state}."))
}

#[tonic::async_trait]
impl Plugin for PluginService {
    /// 握手（`handshake.md` §3）：选宿主列表里最高的共同版本，没有即 `FailedPrecondition`；交出清单原文、操作声明（契约与 Schema）、
    /// 订阅与消息目录（插件的，加上运行时自己的），状态到 `Resolved`。
    async fn handshake(&self, request: Request<v1::HandshakeRequest>) -> Result<Response<v1::HandshakeResponse>, Status> {
        let request = request.into_inner();
        let runtime = &self.runtime;
        let Some(version) = negotiate(&request.protocol_versions, PROTOCOL_VERSIONS) else {
            return Err(Status::failed_precondition(format!(
                "No common protocol version: the host supports {:?}, the plugin supports {:?}.",
                request.protocol_versions, PROTOCOL_VERSIONS
            )));
        };
        let state = runtime.state();
        if state != LifecycleState::Declared {
            return Err(out_of_order("Handshake", state));
        }
        if request.primitives != runtime.manifest.primitives {
            runtime.logger.log(
                Level::Warning,
                "the host's primitives version differs from the manifest; the host will reject the handshake",
                &[("host", Value::from(request.primitives)), ("plugin", Value::from(runtime.manifest.primitives))],
            );
        }
        let operations = runtime
            .invoker
            .registry()
            .list()
            .iter()
            .map(|descriptor| {
                let contract = descriptor.contract();
                v1::OperationDeclaration {
                    id: contract.id.clone(),
                    stability: contract.stability.name().to_owned(),
                    expose: contract.expose.clone(),
                    permission: contract.permission.clone().unwrap_or_default(),
                    policy: contract.policy.clone().unwrap_or_default(),
                    request_schema: descriptor.schema().map(|schema| schema.request.clone()).unwrap_or_default(),
                    response_schema: descriptor.schema().map(|schema| schema.response.clone()).unwrap_or_default(),
                    // 操作契约（RFC-0011）还没有流式操作：登记表里都是一元操作
                    streaming: false,
                }
            })
            .collect();
        let subscriptions = runtime
            .subscriptions
            .iter()
            .map(|(subscription, _)| v1::Subscription {
                topic: subscription.topic.clone(),
                message_type: subscription.message_type.clone().unwrap_or_default(),
                mode: subscription.mode.as_str().to_owned(),
            })
            .collect();
        let catalogs = runtime
            .catalogs
            .iter()
            .map(|catalog| v1::MessageCatalog {
                name: catalog.name().to_owned(),
                locales: catalog
                    .locales()
                    .map(|locale| v1::LocaleTemplates {
                        locale: locale.to_owned(),
                        templates: catalog.templates(locale).map(|(key, template)| (key.to_owned(), template.to_owned())).collect(),
                    })
                    .collect(),
            })
            .collect();
        runtime.transition(LifecycleState::Resolved, None);
        let host = request.host.as_ref();
        runtime.logger.log(
            Level::Information,
            "handshake completed",
            &[
                ("protocolVersion", Value::from(version)),
                ("host", Value::from(host.map(|host| host.sdk.as_str()).unwrap_or_default())),
                ("instance", Value::from(request.instance_id)),
            ],
        );
        Ok(Response::new(v1::HandshakeResponse {
            protocol_version: version,
            primitives: runtime.manifest.primitives,
            manifest: runtime.manifest_text.clone(),
            sdk: Some(v1::SdkInfo { name: SDK_NAME.to_owned(), version: SDK_VERSION.to_owned(), language: SDK_LANGUAGE.to_owned() }),
            operations,
            subscriptions,
            catalogs,
        }))
    }

    /// 配置（`lifecycle.md` §2）：把配置节、授予的权限、绑定与工作目录交给钩子；成功到 `Configured`，失败到 `Failed`。
    async fn configure(&self, request: Request<v1::ConfigureRequest>) -> Result<Response<v1::ConfigureResponse>, Status> {
        let request = request.into_inner();
        let runtime = &self.runtime;
        let state = runtime.state();
        if state != LifecycleState::Resolved {
            return Err(out_of_order("Configure", state));
        }
        runtime.grants.grant(request.granted_permissions.clone());
        let configuration = Configuration {
            section: if request.configuration.is_empty() { "{}".to_owned() } else { request.configuration },
            granted_permissions: request.granted_permissions,
            bindings: request.bindings.into_iter().map(|binding| Binding { name: binding.name, resource_type: binding.r#type, value: binding.value }).collect(),
            work_directory: request.work_directory.into(),
            host: runtime.host.clone(),
        };
        let hooks = Arc::clone(&runtime.hooks);
        let outcome = runtime.joined("Configure", tokio::spawn(async move { hooks.configure(&configuration).await }).await);
        match &outcome {
            Ok(()) => {
                runtime.mark_configured();
                runtime.transition(LifecycleState::Configured, None);
                runtime.logger.information("configured");
            }
            Err(error) => {
                runtime.transition(LifecycleState::Failed, Some(error.clone()));
                runtime.log_failure("configure failed", error);
            }
        }
        Ok(Response::new(v1::ConfigureResponse { status: Some(status_of(&outcome)) }))
    }

    /// 启动（`lifecycle.md` §2）：钩子成功后到 `Started`，随即报 `Ready` 并开始接受调用（§1：插件可直接在 `Start` 后报 `Ready`）。
    async fn start(&self, _: Request<v1::StartRequest>) -> Result<Response<v1::StartResponse>, Status> {
        let runtime = &self.runtime;
        let state = runtime.state();
        if state != LifecycleState::Configured {
            return Err(out_of_order("Start", state));
        }
        let hooks = Arc::clone(&runtime.hooks);
        let outcome = runtime.joined("Start", tokio::spawn(async move { hooks.start().await }).await);
        match &outcome {
            Ok(()) => {
                runtime.transition(LifecycleState::Started, None);
                runtime.transition(LifecycleState::Ready, None);
                runtime.open_calls();
                runtime.logger.log(
                    Level::Information,
                    "started",
                    &[("operations", Value::from(runtime.invoker.registry().len())), ("subscriptions", Value::from(runtime.subscriptions.len()))],
                );
            }
            Err(error) => {
                runtime.transition(LifecycleState::Failed, Some(error.clone()));
                runtime.log_failure("start failed", error);
            }
        }
        Ok(Response::new(v1::StartResponse { status: Some(status_of(&outcome)) }))
    }

    /// 停止（`lifecycle.md` §5）：排空、在宽限内等在途调用、调用停止钩子，应答之后停止服务、进程退出。
    ///
    /// `grace_seconds` 为 0 时按 [`Plugin::with_stop_grace`](super::Plugin::with_stop_grace) 的宽限。任何状态都可以停止，含握手之后的
    /// 探测停止（§3）。
    async fn stop(&self, request: Request<v1::StopRequest>) -> Result<Response<v1::StopResponse>, Status> {
        let request = request.into_inner();
        let runtime = &self.runtime;
        let grace = if request.grace_seconds > 0 { Duration::from_secs(u64::from(request.grace_seconds)) } else { runtime.stop_grace() };
        runtime.logger.log(
            Level::Information,
            "stop requested",
            &[("reason", Value::from(request.reason.as_str())), ("graceSeconds", Value::from(grace.as_secs()))],
        );
        let outcome = runtime.drain(&request.reason, grace).await;
        // 优雅关闭会等这次 RPC 的应答送出再断开连接
        runtime.shutdown.cancel();
        Ok(Response::new(v1::StopResponse { status: Some(status_of(&outcome)) }))
    }

    /// 健康探测（`lifecycle.md` §4）。
    async fn health(&self, _: Request<v1::HealthRequest>) -> Result<Response<v1::HealthResponse>, Status> {
        Ok(Response::new(proto_health(&self.runtime.health())))
    }

    type WatchHealthStream = ReceiverStream<Result<v1::HealthResponse, Status>>;

    /// 健康推送（`lifecycle.md` §4）：先推当前健康，之后状态变化时立即推、健康项每秒重新取一次、有变化才推；停止时推最后一条并结束，
    /// 不让长流拖住优雅关闭。
    async fn watch_health(&self, _: Request<v1::HealthRequest>) -> Result<Response<Self::WatchHealthStream>, Status> {
        let runtime = Arc::clone(&self.runtime);
        let (sender, receiver) = mpsc::channel(4);
        tokio::spawn(async move {
            let mut states = runtime.watch_state();
            let mut last = runtime.health();
            if sender.send(Ok(proto_health(&last))).await.is_err() {
                return;
            }
            let mut ticker = tokio::time::interval(HEALTH_POLL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let ending = tokio::select! {
                    () = sender.closed() => return,
                    () = runtime.shutdown.cancelled() => true,
                    changed = states.changed() => changed.is_err(),
                    _ = ticker.tick() => false,
                };
                let current = runtime.health();
                if current != last {
                    if sender.send(Ok(proto_health(&current))).await.is_err() {
                        return;
                    }
                    last = current;
                }
                if ending {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    /// 调用一个一元操作（`invocation.md` §1）：未声明的操作以 `Unimplemented` 结束；未就绪或排空中为 `PLUGINS_UNAVAILABLE_001`；
    /// 其余经调用器与完整管道执行，结果放进 `Status`。
    ///
    /// 调用在独立任务里执行：宿主取消 RPC 时任务不被中止，只是上下文的令牌触发，处理器照常返回的结果被丢弃。
    async fn invoke(&self, request: Request<v1::InvokeRequest>) -> Result<Response<v1::InvokeResponse>, Status> {
        let grpc_deadline = grpc_deadline(&request);
        let request = request.into_inner();
        let runtime = &self.runtime;
        if runtime.invoker.registry().get(&request.operation).is_none() {
            return Err(Status::unimplemented(format!("The operation '{}' is not provided by this plugin.", request.operation)));
        }
        let call = match runtime.enter() {
            Ok(call) => call,
            Err(error) => return Ok(Response::new(v1::InvokeResponse { status: Some(error_status(&error)), payload: Vec::new() })),
        };
        let (ctx, _cancel) = self.context(request.metadata.as_ref(), grpc_deadline);
        // 这次 RPC 进行中，它的租户是宿主交给插件的租户：回调与发布只能带它（invocation.md §5）
        let _tenant = runtime.tenants.enter(ctx.tenant());
        let invoker = Arc::clone(&runtime.invoker);
        let task = tokio::spawn(async move {
            let _call = call;
            invoker.invoke_owned(ctx, operation::Request::new(request.operation, request.payload)).await
        });
        let outcome = match task.await {
            Ok(outcome) => outcome,
            // 排在 Observe 阶段内置步骤之前的 panic 到了这里：同样是未处理失败（RFC-0011 规则 5），不让 RPC 以传输错误结束
            Err(error) => {
                let reason = if error.is_panic() { panic_reason(error.into_panic().as_ref()) } else { "the invocation was cancelled".to_owned() };
                runtime.logger.log(Level::Error, "the invocation panicked", &[("reason", Value::from(reason))]);
                Err(Error::unexpected(ERROR_CODE_UNEXPECTED, MESSAGE_UNHANDLED_PANIC))
            }
        };
        Ok(Response::new(match outcome {
            Ok(payload) => v1::InvokeResponse { status: Some(ok_status()), payload },
            Err(error) => v1::InvokeResponse { status: Some(error_status(&error)), payload: Vec::new() },
        }))
    }

    type InvokeStreamStream = ReceiverStream<Result<v1::StreamFrame, Status>>;

    /// 流式调用（`invocation.md` §1）：第一帧不是 `start` 以 `InvalidArgument` 结束；操作契约（RFC-0011）还没有流式操作，
    /// 登记表里没有可以这样调用的操作，所以合法的流一律 `Unimplemented`。
    async fn invoke_stream(&self, request: Request<Streaming<v1::StreamFrame>>) -> Result<Response<Self::InvokeStreamStream>, Status> {
        let mut frames = request.into_inner();
        match frames.message().await?.and_then(|frame| frame.frame) {
            Some(v1::stream_frame::Frame::Start(start)) => {
                Err(Status::unimplemented(format!("The streaming operation '{}' is not provided by this plugin.", start.operation)))
            }
            _ => Err(Status::invalid_argument("The first frame of a stream must be 'start'.")),
        }
    }

    /// 投递一条消息（`invocation.md` §4）：按（主题，方式）找订阅，再看类型；找不到以这种方式声明的订阅时返回错误，不改交给另一种方式。
    async fn handle_event(&self, request: Request<v1::HandleEventRequest>) -> Result<Response<v1::HandleEventResponse>, Status> {
        let grpc_deadline = grpc_deadline(&request);
        let request = request.into_inner();
        let runtime = &self.runtime;
        let call = match runtime.enter() {
            Ok(call) => call,
            Err(error) => return Ok(Response::new(v1::HandleEventResponse { status: Some(error_status(&error)) })),
        };
        let envelope = envelope_of(request.envelope.unwrap_or_default());
        let Some(mode) = SubscriptionMode::parse(&request.mode) else {
            let error = failed(&runtime.manifest.id, "HandleEvent", format!("the delivery mode '{}' is neither 'competing' nor 'broadcast'", request.mode));
            return Ok(Response::new(v1::HandleEventResponse { status: Some(error_status(&error)) }));
        };
        let Some(handler) =
            runtime.subscriptions.iter().find(|(subscription, _)| subscription.matches(&envelope, mode)).map(|(_, handler)| Arc::clone(handler))
        else {
            let error = failed(
                &runtime.manifest.id,
                "HandleEvent",
                format!("no subscription for the topic '{}' and the type '{}' in the mode '{mode}'", envelope.topic, envelope.event_type),
            );
            return Ok(Response::new(v1::HandleEventResponse { status: Some(error_status(&error)) }));
        };
        let (ctx, _cancel) = self.context(request.metadata.as_ref(), grpc_deadline);
        let _tenant = runtime.tenants.enter(ctx.tenant());
        let event = Event { envelope, attempt: request.attempt, mode };
        let task = tokio::spawn(async move {
            let _call = call;
            handler.handle(&ctx, event).await
        });
        let outcome = match task.await {
            Ok(outcome) => outcome,
            Err(error) => {
                let reason = if error.is_panic() { panic_reason(error.into_panic().as_ref()) } else { "the delivery was cancelled".to_owned() };
                runtime.logger.log(Level::Error, "the event handler panicked", &[("reason", Value::from(reason))]);
                Err(Error::unexpected(ERROR_CODE_UNEXPECTED, MESSAGE_UNHANDLED_PANIC))
            }
        };
        Ok(Response::new(v1::HandleEventResponse { status: Some(status_of(&outcome)) }))
    }
}
