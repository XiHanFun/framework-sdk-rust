//! `xihan.plugin.v1.Plugin` 服务的实现（`handshake.md` §3、`lifecycle.md` §7、`invocation.md` §1–§4）。
//!
//! 会话令牌在进入这里之前已由拦截器核对（先于一切，含次序核对与流式调用的第一帧）。不在次序表里的调用（`lifecycle.md` §7.1）以
//! gRPC `FailedPrecondition` 拒绝、什么都不做、状态不变：它们是协议层的错误，不是业务失败。

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use super::convert::{context_of, envelope_of, error_status, ok_status, proto_health, status_of};
use super::host::HostHandle;
use super::log::Level;
use super::messages::{PHASE_CONFIGURE, PHASE_START, REASON_NOT_AN_OBJECT, failed, unsubscribed};
use super::runtime::{DEFAULT_STOP_GRACE, Runtime, panic_reason};
use super::time::parse_grpc_timeout;
use super::types::{Binding, Configuration, Event, EventHandler, Health, SubscriptionMode};
use super::v1::plugin_server::Plugin;
use super::{PROTOCOL_VERSIONS, SDK_LANGUAGE, negotiate, v1};
use crate::invocation::{CancellationToken, Context};
use crate::operation;
use crate::primitives::json::{self, Node};
use crate::primitives::{Error, LifecycleState, SDK_NAME, SDK_VERSION};

/// `WatchHealth` 在可服务期间重新求健康回调的间隔（`lifecycle.md` §7.4，宿主 `HealthInterval` 的缺省值）。
const HEALTH_INTERVAL: Duration = Duration::from_secs(10);

/// `Plugin` 服务。
pub(crate) struct PluginService {
    runtime: Arc<Runtime>,
}

impl PluginService {
    pub(crate) fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    /// 为一次调用或投递构造上下文，放入宿主客户端；返回的守卫在宿主取消这次 RPC（future 被丢弃）时触发上下文的令牌。
    /// 令牌是运行时在途调用令牌的子令牌：`Stop` 的宽限到了或收到收尾请求时同样触发。
    fn context(&self, metadata: Option<&v1::InvocationMetadata>, grpc_deadline: Option<SystemTime>) -> (Context, CancelOnDrop) {
        let token = self.runtime.call_token();
        let ctx = context_of(metadata, grpc_deadline, &token);
        ctx.insert_item(HostHandle(Arc::clone(&self.runtime.host)));
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

/// 不在次序表里的调用（`lifecycle.md` §7.1）。
fn out_of_order(rpc: &str, state: LifecycleState) -> Status {
    Status::failed_precondition(format!("{rpc} is not allowed in the state {state}."))
}

/// 配置节文本是不是 JSON 对象（`lifecycle.md` §7.2）：先过 RFC-0001 的共同 JSON 读取检查。
fn is_object(section: &str) -> bool {
    matches!(json::parse(section.as_bytes()), Ok(Node::Object(_)))
}

#[tonic::async_trait]
impl Plugin for PluginService {
    /// 握手（`handshake.md` §3）：只在 `Declared` 接受；选宿主列表里最高的共同版本，没有即 `FailedPrecondition`；交出清单的
    /// `primitives`、清单原文、一元操作的声明（契约与 Schema）、按声明顺序的订阅（方式总写 `competing` 或 `broadcast`）与插件自己的
    /// 消息目录（运行时的目录宿主已有，不发送），状态到 `Resolved`。
    async fn handshake(&self, request: Request<v1::HandshakeRequest>) -> Result<Response<v1::HandshakeResponse>, Status> {
        let request = request.into_inner();
        let runtime = &self.runtime;
        let _order = runtime.lifecycle.lock().await;
        let state = runtime.state();
        if state != LifecycleState::Declared {
            return Err(out_of_order("Handshake", state));
        }
        let Some(version) = negotiate(&request.protocol_versions, PROTOCOL_VERSIONS) else {
            return Err(Status::failed_precondition(format!(
                "No common protocol version: the host supports {:?}, the plugin supports {:?}.",
                request.protocol_versions, PROTOCOL_VERSIONS
            )));
        };
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
                    // 流式操作等流式契约的 RFC（handshake.md §3 第 4 条）：登记表里都是一元操作
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

    /// 配置（`lifecycle.md` §7.2）：只在 `Resolved` 接受。配置节为空等同 `{}`；不是 JSON 对象时不调钩子，以 `PLUGINS_UNEXPECTED_001` +
    /// `plugins.process.failed`（`phase` 为 `configure`）进入 `Failed`；否则把配置节、授予的权限、绑定与工作目录交给钩子，成功到
    /// `Configured`，失败或 panic 到 `Failed`。
    async fn configure(&self, request: Request<v1::ConfigureRequest>) -> Result<Response<v1::ConfigureResponse>, Status> {
        let request = request.into_inner();
        let runtime = &self.runtime;
        let _order = runtime.lifecycle.lock().await;
        let state = runtime.state();
        if state != LifecycleState::Resolved {
            return Err(out_of_order("Configure", state));
        }
        runtime.grants.grant(request.granted_permissions.clone());
        let section = if request.configuration.is_empty() { "{}".to_owned() } else { request.configuration };
        let outcome = if is_object(&section) {
            let configuration = Configuration {
                section,
                granted_permissions: request.granted_permissions,
                bindings: request
                    .bindings
                    .into_iter()
                    .map(|binding| Binding { name: binding.name, resource_type: binding.r#type, value: binding.value })
                    .collect(),
                work_directory: request.work_directory.into(),
                host: Arc::clone(&runtime.host),
            };
            let hooks = Arc::clone(&runtime.hooks);
            runtime.joined(PHASE_CONFIGURE, tokio::spawn(async move { hooks.configure(&configuration).await }).await)
        } else {
            Err(failed(&runtime.manifest.id, PHASE_CONFIGURE, REASON_NOT_AN_OBJECT))
        };
        match &outcome {
            Ok(()) => {
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

    /// 启动（`lifecycle.md` §7.3）：只在 `Configured` 接受。钩子成功后到 `Started`，有健康回调时立即求一次，按结果进入 `Ready`、
    /// `Degraded` 或 `Failed`，没有即 `Ready`，然后才应答 `ok`；钩子失败或 panic 到 `Failed`（`phase` 为 `start`）。
    async fn start(&self, _: Request<v1::StartRequest>) -> Result<Response<v1::StartResponse>, Status> {
        let runtime = &self.runtime;
        let _order = runtime.lifecycle.lock().await;
        let state = runtime.state();
        if state != LifecycleState::Configured {
            return Err(out_of_order("Start", state));
        }
        let hooks = Arc::clone(&runtime.hooks);
        let outcome = runtime.joined(PHASE_START, tokio::spawn(async move { hooks.start().await }).await);
        match &outcome {
            Ok(()) => {
                runtime.started.store(true, Ordering::Release);
                runtime.transition(LifecycleState::Started, None);
                runtime.become_ready();
                runtime.logger.log(
                    Level::Information,
                    "started",
                    &[
                        ("state", Value::from(runtime.state().name())),
                        ("operations", Value::from(runtime.invoker.registry().len())),
                        ("subscriptions", Value::from(runtime.subscriptions.len())),
                    ],
                );
            }
            Err(error) => {
                runtime.transition(LifecycleState::Failed, Some(error.clone()));
                runtime.log_failure("start failed", error);
            }
        }
        Ok(Response::new(v1::StartResponse { status: Some(status_of(&outcome)) }))
    }

    /// 停止（`lifecycle.md` §7.5）：`Declared`、`Draining`、`Stopped` 与已经开始停止时 `FailedPrecondition`。启动钩子没成功过的
    /// 不调停止钩子、应答 `ok`；否则可服务的进入 `Draining`、拒绝新调用，等在途调用到宽限（`grace_seconds`，0 按 5 秒），宽限到了
    /// 触发它们的取消令牌、不再等，再调停止钩子。应答之后停止服务，进程以退出码 0 退出；停止钩子的失败只写进 `status`。
    async fn stop(&self, request: Request<v1::StopRequest>) -> Result<Response<v1::StopResponse>, Status> {
        let request = request.into_inner();
        let runtime = &self.runtime;
        {
            let _order = runtime.lifecycle.lock().await;
            if !runtime.accept_stop() {
                return Err(out_of_order("Stop", runtime.state()));
            }
        }
        let grace = if request.grace_seconds > 0 { Duration::from_secs(u64::from(request.grace_seconds)) } else { DEFAULT_STOP_GRACE };
        runtime.logger.log(
            Level::Information,
            "stop requested",
            &[("reason", Value::from(request.reason.as_str())), ("graceSeconds", Value::from(grace.as_secs()))],
        );
        let outcome = runtime.stop(&request.reason, grace).await;
        // 优雅关闭会等这次 RPC 的应答送出再断开连接
        runtime.shutdown.cancel();
        Ok(Response::new(v1::StopResponse { status: Some(status_of(&outcome)) }))
    }

    /// 健康（`lifecycle.md` §7.4）：任何状态都可以调用；`Ready` 与 `Degraded` 期间每次求一次健康回调，其余报本地状态、没有健康项。
    async fn health(&self, _: Request<v1::HealthRequest>) -> Result<Response<v1::HealthResponse>, Status> {
        Ok(Response::new(proto_health(&self.runtime.health())))
    }

    type WatchHealthStream = ReceiverStream<Result<v1::HealthResponse, Status>>;

    /// 健康推送（`lifecycle.md` §7.4）：先推一次当前健康，之后状态、健康项或 `error` 变化时推送；可服务期间每 10 秒重新求一次健康回调，
    /// 状态变化时不另求回调。进入 `Draining` 时推一次，停止服务（应答 `Stop` 之后）时结束流，不让长流拖住优雅关闭。
    async fn watch_health(&self, _: Request<v1::HealthRequest>) -> Result<Response<Self::WatchHealthStream>, Status> {
        let runtime = Arc::clone(&self.runtime);
        let (sender, receiver) = mpsc::channel(4);
        tokio::spawn(async move {
            let mut states = runtime.watch_state();
            let mut last = runtime.health();
            if sender.send(Ok(proto_health(&last))).await.is_err() {
                return;
            }
            // 第一次求回调就在上面：间隔从现在算起，不立即再求一次
            let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + HEALTH_INTERVAL, HEALTH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let (current, ending) = tokio::select! {
                    () = sender.closed() => return,
                    () = runtime.shutdown.cancelled() => (local(&runtime, &last), true),
                    changed = states.changed() => (local(&runtime, &last), changed.is_err()),
                    _ = ticker.tick() => (runtime.health(), false),
                };
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

    /// 调用一个一元操作（`invocation.md` §1），依次核对：未声明的操作以 `Unimplemented` 结束；不在可服务状态为
    /// `PLUGINS_UNAVAILABLE_001` + `plugins.process.unavailable`；然后还原上下文，经调用器与完整管道执行（`payload` 为空等同 `{}`），
    /// 结果放进 `Status`。
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
        // 这次 RPC 进行中，它的租户是宿主交给插件的租户：回调只能带它（invocation.md §5）
        let _tenant = runtime.tenants.enter(ctx.tenant());
        let invoker = Arc::clone(&runtime.invoker);
        let task = tokio::spawn(async move {
            let _call = call;
            invoker.invoke_owned(ctx, operation::Request::new(request.operation, request.payload)).await
        });
        let outcome = match task.await {
            Ok(outcome) => outcome,
            // 排在 Observe 阶段内置步骤之前的 panic 到了这里：同样是未处理失败（RFC-0011 规则 5），不让 RPC 以传输错误结束
            Err(error) => Err(unhandled(runtime, error, "the invocation panicked")),
        };
        Ok(Response::new(match outcome {
            Ok(payload) => v1::InvokeResponse { status: Some(ok_status()), payload },
            Err(error) => v1::InvokeResponse { status: Some(error_status(&error)), payload: Vec::new() },
        }))
    }

    type InvokeStreamStream = ReceiverStream<Result<v1::StreamFrame, Status>>;

    /// 流式调用（`invocation.md` §1）：流式操作契约的 RFC 之前插件不实现流式调用。读第一帧：流在第一帧之前结束或第一帧不是 `start`
    /// 以 `InvalidArgument` 结束，第一帧是 `start` 以 `Unimplemented` 结束，不论操作是否声明、插件是否就绪。
    async fn invoke_stream(&self, request: Request<Streaming<v1::StreamFrame>>) -> Result<Response<Self::InvokeStreamStream>, Status> {
        let mut frames = request.into_inner();
        match frames.message().await?.and_then(|frame| frame.frame) {
            Some(v1::stream_frame::Frame::Start(start)) => Err(Status::unimplemented(format!(
                "The operation '{}' cannot be invoked as a stream: plugins do not implement streaming yet.",
                start.operation
            ))),
            Some(_) => Err(Status::invalid_argument("The first frame of a stream must be 'start'.")),
            None => Err(Status::invalid_argument("The stream ended before its first frame.")),
        }
    }

    /// 投递一条消息（`invocation.md` §4），依次：没有信封或方式不是空、`competing`、`broadcast` 为 `InvalidArgument`；按（主题，方式，
    /// 类型）找订阅，一个也没有为 `KERNEL_NOTFOUND_001` + `plugins.event.unsubscribed`；不在可服务状态为 `PLUGINS_UNAVAILABLE_001`；
    /// 然后还原上下文，匹配的订阅按声明顺序依次交给各自的处理器，第一个失败即结果，处理器 panic 为 `KERNEL_UNEXPECTED_001` +
    /// `kernel.invocation.unhandled_exception`。
    async fn handle_event(&self, request: Request<v1::HandleEventRequest>) -> Result<Response<v1::HandleEventResponse>, Status> {
        let grpc_deadline = grpc_deadline(&request);
        let request = request.into_inner();
        let runtime = &self.runtime;
        let Some(envelope) = request.envelope else {
            return Err(Status::invalid_argument("The delivery has no envelope."));
        };
        let Some(mode) = SubscriptionMode::parse(&request.mode) else {
            return Err(Status::invalid_argument(format!("The delivery mode '{}' is neither 'competing' nor 'broadcast'.", request.mode)));
        };
        let envelope = envelope_of(envelope);
        let handlers: Vec<Arc<dyn EventHandler>> =
            runtime.subscriptions.iter().filter(|(subscription, _)| subscription.matches(&envelope, mode)).map(|(_, handler)| Arc::clone(handler)).collect();
        if handlers.is_empty() {
            let error = unsubscribed(&runtime.manifest.id, &envelope.topic, &envelope.event_type, mode.as_str());
            return Ok(Response::new(v1::HandleEventResponse { status: Some(error_status(&error)) }));
        }
        let call = match runtime.enter() {
            Ok(call) => call,
            Err(error) => return Ok(Response::new(v1::HandleEventResponse { status: Some(error_status(&error)) })),
        };
        let (ctx, _cancel) = self.context(request.metadata.as_ref(), grpc_deadline);
        let _tenant = runtime.tenants.enter(ctx.tenant());
        let event = Event { envelope, attempt: request.attempt, mode };
        let task = tokio::spawn(async move {
            let _call = call;
            for handler in handlers {
                handler.handle(&ctx, event.clone()).await?;
            }
            Ok(())
        });
        let outcome = match task.await {
            Ok(outcome) => outcome,
            Err(error) => Err(unhandled(runtime, error, "an event handler panicked")),
        };
        Ok(Response::new(v1::HandleEventResponse { status: Some(status_of(&outcome)) }))
    }
}

/// 状态变化时推送的健康：不另求回调，可服务期间沿用上次的健康项，其余状态没有健康项。
fn local(runtime: &Runtime, last: &Health) -> Health {
    let snapshot = runtime.snapshot();
    let items = if snapshot.state.is_serving() { last.items.clone() } else { Vec::new() };
    Health { state: snapshot.state, items, error: snapshot.error }
}

/// 处理器任务以 panic 结束：未处理失败（RFC-0011 规则 5，同调用器 Observe 阶段的翻译），panic 的内容只写进 stderr。
fn unhandled(runtime: &Runtime, error: tokio::task::JoinError, message: &str) -> Error {
    let reason = if error.is_panic() { panic_reason(error.into_panic().as_ref()) } else { "the task was cancelled".to_owned() };
    runtime.logger.log(Level::Error, message, &[("reason", Value::from(reason))]);
    operation::unhandled()
}
