//! 操作契约与 XGI 消息（RFC-0011）：业务以操作为单位声明，调用器让它经过完整管道并在 handle 阶段执行。
//!
//! 请求与响应用 `serde_json` 编解码，JSON 是 XGI 的规范编码；类型化入口 [`Invoker::invoke_typed`] 不经 JSON。
//!
//! ```
//! use std::sync::Arc;
//! use serde::{Deserialize, Serialize};
//! use xihan_framework::invocation::Context;
//! use xihan_framework::operation::{self, Contract, Invoker, Registry, Request};
//! use xihan_framework::pipeline::Builder;
//!
//! #[derive(Deserialize)]
//! struct Create { quantity: u32 }
//! #[derive(Serialize)]
//! struct Created { quantity: u32 }
//!
//! let registry = Arc::new(Registry::new());
//! registry
//!     .add(Contract::new("orders.create"), operation::handler_fn(|_ctx, request: Create| Box::pin(async move { Ok(Created { quantity: request.quantity }) })))
//!     .unwrap();
//! let invoker = Arc::new(Invoker::new(registry, &Builder::new()).unwrap());
//! let _response = invoker.invoke_owned(Context::background(), Request::new("orders.create", r#"{"quantity":2}"#)); // 交给执行器 await
//! ```

use std::any::{Any, type_name};
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::task::Poll;
use std::time::SystemTime;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::BoxFuture;
use crate::invocation::{Context, ERROR_CODE_DEADLINE_EXCEEDED, MESSAGE_DEADLINE_BEFORE_START};
use crate::pipeline::{self, APP_OWNER, Middleware, Next, Pipeline, Placement, Step};
use crate::primitives::json::{self, Node};
use crate::primitives::{Details, ERROR_CODE_INVALID_ARGUMENT, Error, FrameworkError, PipelineStage, Result, Stability, is_error_code};

/// 操作未登记。
pub const ERROR_CODE_NOT_FOUND: &str = "KERNEL_NOTFOUND_001";
/// 操作载荷不合法（非 JSON、`null` 或类型不匹配）。
pub const ERROR_CODE_INVALID_PAYLOAD: &str = "KERNEL_VALIDATION_001";
/// 类型化调用的请求或响应类型与登记不符。
pub const ERROR_CODE_TYPE_MISMATCH: &str = "KERNEL_VALIDATION_002";
/// 操作 ID 重复登记（配置期）。
pub const ERROR_CODE_DUPLICATE: &str = "KERNEL_OPERATION_001";
/// 操作登记表已满（配置期）。
pub const ERROR_CODE_REGISTRY_FULL: &str = "KERNEL_OPERATION_002";

/// 调用期间的意外错误：管道以成功结束而没有响应（规则 9），或响应编码失败。
pub const ERROR_CODE_UNEXPECTED: &str = "KERNEL_UNEXPECTED_001";

/// 操作未登记的消息键；参数 `id`。
pub const MESSAGE_NOT_FOUND: &str = "kernel.operation.unknown";
/// 载荷不合法的消息键；参数 `id`、`reason`（出错位置的 JSON 路径，如 `$.quantity`）。
pub const MESSAGE_INVALID_PAYLOAD: &str = "kernel.operation.invalid_payload";
/// 类型不符的消息键；参数只有 `id`（ADR-0236）。
pub const MESSAGE_TYPE_MISMATCH: &str = "kernel.operation.type_mismatch";
/// 管道以成功结束而没有响应的消息键；参数 `id`。
pub const MESSAGE_NO_RESPONSE: &str = "kernel.operation.no_response";
/// 操作 ID 重复登记的消息键；参数 `id`、`owner`（已登记的那一个的归属）。
pub const MESSAGE_DUPLICATE: &str = "kernel.operation.duplicate";
/// 操作登记表已满的消息键；参数 `capacity`。
pub const MESSAGE_REGISTRY_FULL: &str = "kernel.operation.registry_full";
/// 中间件或处理器在调用期间 panic 的消息键（与 .NET 的未处理异常同键）；没有参数。
pub const MESSAGE_UNHANDLED_PANIC: &str = "kernel.invocation.unhandled_exception";

/// 调用器内置步骤的归属（RFC-0011 规则 10），与 .NET 内置中间件的归属相同。
pub const KERNEL_OWNER: &str = "xihan.kernel.core";

/// Observe 阶段 `Beginning` 上的内置步骤：把排在它之后的中间件与处理器的 panic 翻译成未处理失败（规则 5）。
pub const OBSERVE_MIDDLEWARE: &str = "ObserveMiddleware";

/// Resilience 阶段 `Beginning` 上的内置步骤：进入时截止时间已到即结束调用。
pub const DEADLINE_MIDDLEWARE: &str = "DeadlineMiddleware";

/// Validate 阶段 `Beginning` 上的内置步骤：原始调用在这里解码载荷（规则 2），类型化调用跳过。
pub const VALIDATION_MIDDLEWARE: &str = "ValidationMiddleware";

/// 登记表的容量。
pub const CAPACITY: usize = 10_000;

/// 暴露的协议：HTTP。
pub const EXPOSE_HTTP: &str = "http";
/// 暴露的协议：gRPC。
pub const EXPOSE_GRPC: &str = "grpc";
/// 暴露的协议：MCP。
pub const EXPOSE_MCP: &str = "mcp";
/// 暴露的协议：命令行。
pub const EXPOSE_COMMAND_LINE: &str = "commandline";
/// 暴露的协议：AI 工具目录（`ai.tools@1`）。
pub const EXPOSE_AI: &str = "ai";

/// 判断操作 ID 是否符合 `<resource>.<verb>`：`^[a-z][a-z0-9]*(-[a-z0-9]+)*\.[a-z][a-z0-9]*(-[a-z0-9]+)*$`。
pub fn is_id(id: &str) -> bool {
    crate::grammar::operation_id(id)
}

/// 操作契约：ID、稳定性、暴露的协议与授权要求（`permission` 与 `policy` 二选一）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Contract {
    /// 操作 ID `<resource>.<verb>`。
    pub id: String,
    /// 稳定性；缺省 [`Stability::Stable`]。
    pub stability: Stability,
    /// 暴露的协议名（`EXPOSE_*`）；空表示没有声明，由宿主按它的缺省处理。
    pub expose: Vec<String>,
    /// 所需的权限码。
    pub permission: Option<String>,
    /// 所需的策略名。
    pub policy: Option<String>,
}

impl Contract {
    /// 用操作 ID 创建：稳定，没有声明暴露的协议与授权要求。
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), stability: Stability::Stable, expose: Vec::new(), permission: None, policy: None }
    }

    /// 设置稳定性。
    pub fn with_stability(mut self, stability: Stability) -> Self {
        self.stability = stability;
        self
    }

    /// 设置暴露的协议。
    pub fn with_expose<I, S>(mut self, protocols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.expose = protocols.into_iter().map(Into::into).collect();
        self
    }

    /// 设置所需的权限码。
    pub fn with_permission(mut self, permission: impl Into<String>) -> Self {
        self.permission = Some(permission.into());
        self
    }

    /// 设置所需的策略名。
    pub fn with_policy(mut self, policy: impl Into<String>) -> Self {
        self.policy = Some(policy.into());
        self
    }
}

/// 请求与响应的 JSON Schema（draft 2020-12）文本；两段都必须是 JSON 对象。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Schema {
    /// 请求的 Schema。
    pub request: String,
    /// 响应的 Schema。
    pub response: String,
}

impl Schema {
    /// 用请求与响应的 Schema 创建。
    pub fn new(request: impl Into<String>, response: impl Into<String>) -> Self {
        Self { request: request.into(), response: response.into() }
    }
}

/// XGI 请求：操作 ID 与请求类型的 UTF-8 JSON（空等价于 `{}`）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Request {
    /// 操作 ID。
    pub id: String,
    /// 请求载荷。
    pub payload: Vec<u8>,
}

impl Request {
    /// 用操作 ID 与载荷创建。
    pub fn new(id: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self { id: id.into(), payload: payload.into() }
    }
}

/// 处理一个操作：可恢复失败以 `Err` 返回，不 panic。
pub trait Handler<Req, Res>: Send + Sync + 'static {
    /// 处理一次请求。
    fn handle<'a>(&'a self, ctx: &'a Context, request: Req) -> BoxFuture<'a, Result<Res>>;
}

impl<Req, Res, H: Handler<Req, Res> + ?Sized> Handler<Req, Res> for Arc<H> {
    fn handle<'a>(&'a self, ctx: &'a Context, request: Req) -> BoxFuture<'a, Result<Res>> {
        (**self).handle(ctx, request)
    }
}

/// 由函数实现的 [`Handler`]，见 [`handler_fn`]。
pub struct HandlerFn<Req, Res, F> {
    function: F,
    // fn(Req) -> Res 只记下类型参数，不影响 Send、Sync
    types: PhantomData<fn(Req) -> Res>,
}

/// 用函数创建处理器：`handler_fn(|ctx, request: CreateOrder| Box::pin(async move { … }))`。
pub fn handler_fn<Req, Res, F>(function: F) -> HandlerFn<Req, Res, F>
where
    F: for<'a> Fn(&'a Context, Req) -> BoxFuture<'a, Result<Res>> + Send + Sync + 'static,
{
    HandlerFn { function, types: PhantomData }
}

impl<Req, Res, F> Handler<Req, Res> for HandlerFn<Req, Res, F>
where
    Req: 'static,
    Res: 'static,
    F: for<'a> Fn(&'a Context, Req) -> BoxFuture<'a, Result<Res>> + Send + Sync + 'static,
{
    fn handle<'a>(&'a self, ctx: &'a Context, request: Req) -> BoxFuture<'a, Result<Res>> {
        (self.function)(ctx, request)
    }
}

impl<Req, Res, F> fmt::Debug for HandlerFn<Req, Res, F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("HandlerFn").field("request", &type_name::<Req>()).field("response", &type_name::<Res>()).finish_non_exhaustive()
    }
}

/// 解码载荷、执行、编码响应：登记时按请求与响应类型生成，不经反射。
///
/// 解码与执行分开：原始调用在 Validate 阶段的内置步骤里解码，handle 阶段之后执行（RFC-0011 规则 10）。
trait Execute: Send + Sync {
    fn decode(&self, id: &str, payload: &[u8]) -> Result<Box<dyn Any + Send>>;
    fn run<'a>(&'a self, ctx: &'a Context, id: &'a str, request: Box<dyn Any + Send>) -> BoxFuture<'a, Result<Vec<u8>>>;
}

struct JsonExecutor<Req, Res> {
    handler: Arc<dyn Handler<Req, Res>>,
}

impl<Req, Res> Execute for JsonExecutor<Req, Res>
where
    Req: DeserializeOwned + Send + 'static,
    Res: Serialize + Send + 'static,
{
    fn decode(&self, id: &str, payload: &[u8]) -> Result<Box<dyn Any + Send>> {
        decode::<Req>(id, payload).map(|request| Box::new(request) as Box<dyn Any + Send>)
    }

    fn run<'a>(&'a self, ctx: &'a Context, id: &'a str, request: Box<dyn Any + Send>) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            // 解码与执行来自同一个执行器，类型总是对的
            let request = *request
                .downcast::<Req>()
                .map_err(|_| Error::literal(ERROR_CODE_UNEXPECTED, format!("The decoded request of '{id}' does not have the registered type.")))?;
            let response = self.handler.handle(ctx, request).await?;
            serde_json::to_vec(&response)
                .map_err(|error| Error::literal(ERROR_CODE_UNEXPECTED, format!("The response of '{id}' could not be encoded: {error}")))
        })
    }
}

/// 登记表里的一个操作：契约、Schema、归属与执行入口。
pub struct Descriptor {
    contract: Contract,
    schema: Option<Schema>,
    owner: String,
    executor: Box<dyn Execute>,
    // 装着 Arc<dyn Handler<Req, Res>>：类型化调用按请求与响应类型取回
    typed: Box<dyn Any + Send + Sync>,
    request_type: String,
    response_type: String,
}

impl Descriptor {
    /// 契约。
    pub fn contract(&self) -> &Contract {
        &self.contract
    }

    /// 操作 ID。
    pub fn id(&self) -> &str {
        &self.contract.id
    }

    /// Schema；没有登记时为 `None`。
    pub fn schema(&self) -> Option<&Schema> {
        self.schema.as_ref()
    }

    /// 归属：清单 ID 或 [`APP_OWNER`]。
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// 请求类型的短名（不带模块路径）。
    pub fn request_type(&self) -> &str {
        &self.request_type
    }

    /// 响应类型的短名（不带模块路径）。
    pub fn response_type(&self) -> &str {
        &self.response_type
    }

    /// 不经管道直接执行：解码载荷、执行、编码响应；载荷不合法返回 `KERNEL_VALIDATION_001`。
    pub fn execute<'a>(&'a self, ctx: &'a Context, payload: &'a [u8]) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let request = self.executor.decode(self.id(), payload)?;
            self.executor.run(ctx, self.id(), request).await
        })
    }
}

impl fmt::Debug for Descriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Descriptor")
            .field("contract", &self.contract)
            .field("schema", &self.schema)
            .field("owner", &self.owner)
            .field("request_type", &self.request_type)
            .field("response_type", &self.response_type)
            .finish_non_exhaustive()
    }
}

/// 操作登记表：ID 唯一，容量 [`CAPACITY`]，按 ID 升序列出；可在多个线程间共享。
#[derive(Default)]
pub struct Registry {
    descriptors: RwLock<BTreeMap<String, Arc<Descriptor>>>,
}

impl Registry {
    /// 创建空的登记表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个操作：没有 Schema，归属 [`APP_OWNER`]；错误同 [`Registry::add_with`]。
    pub fn add<Req, Res, H>(&self, contract: Contract, handler: H) -> std::result::Result<(), FrameworkError>
    where
        Req: DeserializeOwned + Send + 'static,
        Res: Serialize + Send + 'static,
        H: Handler<Req, Res>,
    {
        self.add_with(contract, handler, None, APP_OWNER)
    }

    /// 登记一个操作。
    ///
    /// ID 不合法、同时声明了权限与策略、归属为空或 Schema 不是 JSON 对象返回 `PRIMITIVES_VALIDATION_001`，
    /// 重复返回 `KERNEL_OPERATION_001`，已满返回 `KERNEL_OPERATION_002`。
    pub fn add_with<Req, Res, H>(&self, contract: Contract, handler: H, schema: Option<Schema>, owner: &str) -> std::result::Result<(), FrameworkError>
    where
        Req: DeserializeOwned + Send + 'static,
        Res: Serialize + Send + 'static,
        H: Handler<Req, Res>,
    {
        let id = contract.id.clone();
        if !is_id(&id) {
            return Err(invalid(format!("The operation ID '{id}' must match <resource>.<verb>.")));
        }
        if contract.permission.is_some() && contract.policy.is_some() {
            return Err(invalid(format!("The operation '{id}' must declare either a permission or a policy.")));
        }
        if owner.is_empty() {
            return Err(invalid(format!("The operation '{id}' needs an owner.")));
        }
        if let Some(schema) = &schema {
            for (part, text) in [("request", &schema.request), ("response", &schema.response)] {
                if serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(text).is_err() {
                    return Err(invalid(format!("The {part} schema of '{id}' must be a JSON object.")));
                }
            }
        }
        let handler: Arc<dyn Handler<Req, Res>> = Arc::new(handler);
        let descriptor = Descriptor {
            contract,
            schema,
            owner: owner.to_owned(),
            executor: Box::new(JsonExecutor { handler: Arc::clone(&handler) }),
            typed: Box::new(handler),
            request_type: short_type_name::<Req>(),
            response_type: short_type_name::<Res>(),
        };

        // 重复与容量在同一次写锁里判断：并发的登记不会越过容量，也不会都以为自己是第一个
        let mut descriptors = self.write();
        if let Some(existing) = descriptors.get(&id) {
            let details = Details::from([("id".to_owned(), Value::from(id.as_str())), ("owner".to_owned(), Value::from(existing.owner()))]);
            return Err(FrameworkError::with_message(ERROR_CODE_DUPLICATE, MESSAGE_DUPLICATE, details));
        }
        if descriptors.len() >= CAPACITY {
            let details = Details::from([("capacity".to_owned(), Value::from(CAPACITY))]);
            return Err(FrameworkError::with_message(ERROR_CODE_REGISTRY_FULL, MESSAGE_REGISTRY_FULL, details));
        }
        descriptors.insert(id, Arc::new(descriptor));
        Ok(())
    }

    /// 按 ID 取描述。
    pub fn get(&self, id: &str) -> Option<Arc<Descriptor>> {
        self.read().get(id).cloned()
    }

    /// 按 ID 升序列出全部描述。
    pub fn list(&self) -> Vec<Arc<Descriptor>> {
        self.read().values().cloned().collect()
    }

    /// 已登记的操作数。
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// 判断是否没有登记任何操作。
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    // 锁里只有对表的整体增删，panic 不会留下半改的状态，所以中毒时照常使用

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, Arc<Descriptor>>> {
        self.descriptors.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, Arc<Descriptor>>> {
        self.descriptors.write().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Registry").field("operations", &self.read().keys().collect::<Vec<_>>()).finish()
    }
}

/// 对象袋里的当前操作。
#[derive(Clone)]
struct CurrentOperation(Arc<Descriptor>);

/// 对象袋里的当前 XGI 请求。
#[derive(Clone)]
struct CurrentRequest(Arc<Request>);

/// 对象袋里 handle 阶段写回的响应载荷。
#[derive(Clone)]
struct ResponsePayload(Vec<u8>);

/// 对象袋里 Validate 阶段解码好的请求，handle 阶段之后取走；只在原始调用里有。
#[derive(Clone)]
struct DecodedRequest(Arc<Mutex<Option<Box<dyn Any + Send>>>>);

/// 返回当前调用的操作描述：管道里的中间件用；不在调用里为 `None`。
pub fn current(ctx: &Context) -> Option<Arc<Descriptor>> {
    ctx.item::<CurrentOperation>().map(|current| Arc::clone(&current.0))
}

/// 返回当前调用的 XGI 请求；类型化调用没有 XGI 请求，为 `None`。
pub fn request_of(ctx: &Context) -> Option<Arc<Request>> {
    ctx.item::<CurrentRequest>().map(|current| Arc::clone(&current.0))
}

/// 返回 handle 阶段写回的响应载荷：操作成功后在中间件的下游返回之后可读。
pub fn response_payload_of(ctx: &Context) -> Option<Vec<u8>> {
    ctx.item::<ResponsePayload>().map(|payload| payload.0.clone())
}

/// 一次调用在对象袋里的作用域：进入时放入当前操作与请求，离开时（含 future 被丢弃）恢复上一层。
///
/// 同一上下文上的嵌套调用因此各看到自己的操作；对象袋由派生上下文共享，并发的调用应各用各的上下文。
struct Scope<'c> {
    ctx: &'c Context,
    operation: Option<Arc<CurrentOperation>>,
    request: Option<Arc<CurrentRequest>>,
    response: Option<Arc<ResponsePayload>>,
    decoded: Option<Arc<DecodedRequest>>,
}

impl<'c> Scope<'c> {
    fn enter(ctx: &'c Context, descriptor: Arc<Descriptor>, request: Option<Arc<Request>>) -> Self {
        let operation = ctx.insert_item(CurrentOperation(descriptor));
        let (request, decoded) = match request {
            Some(request) => (ctx.insert_item(CurrentRequest(request)), ctx.insert_item(DecodedRequest(Arc::new(Mutex::new(None))))),
            None => (ctx.remove_item::<CurrentRequest>(), ctx.remove_item::<DecodedRequest>()),
        };
        let response = ctx.remove_item::<ResponsePayload>();
        Self { ctx, operation, request, response, decoded }
    }

    /// 取走这次调用写回的响应载荷。
    fn take_response(&self) -> Option<Vec<u8>> {
        self.ctx.remove_item::<ResponsePayload>().map(|payload| Arc::unwrap_or_clone(payload).0)
    }
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        restore(self.ctx, self.operation.take());
        restore(self.ctx, self.request.take());
        restore(self.ctx, self.response.take());
        restore(self.ctx, self.decoded.take());
    }
}

/// 把对象袋里某个类型的值恢复成进入前的样子。
fn restore<T: Clone + Send + Sync + 'static>(ctx: &Context, previous: Option<Arc<T>>) {
    match previous {
        Some(previous) => {
            ctx.insert_item(Arc::unwrap_or_clone(previous));
        }
        None => {
            ctx.remove_item::<T>();
        }
    }
}

/// XGI 调用的终点：在 handle 阶段之后执行当前操作，成功时把响应载荷写回上下文。
struct Dispatch<'d> {
    descriptor: &'d Descriptor,
    payload: &'d [u8],
}

impl Step for Dispatch<'_> {
    fn run<'a>(&'a self, ctx: &'a Context) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let (id, executor) = (self.descriptor.id(), &self.descriptor.executor);
            // Validate 阶段解码好的请求；中间件再次进入下游（如重试）时它已被取走，按同一份载荷重新解码
            let decoded = ctx.item::<DecodedRequest>().and_then(|slot| slot.0.lock().unwrap_or_else(PoisonError::into_inner).take());
            let request = match decoded {
                Some(request) => request,
                None => executor.decode(id, self.payload)?,
            };
            let payload = executor.run(ctx, id, request).await?;
            ctx.insert_item(ResponsePayload(payload));
            Ok(())
        })
    }
}

/// 类型化调用的终点：直接以请求对象调用处理器，结果留在槽里；中间件重试时每次用请求的一份克隆。
struct Typed<Req, Res> {
    handler: Arc<dyn Handler<Req, Res>>,
    request: Req,
    outcome: Mutex<Option<Result<Res>>>,
}

impl<Req, Res> Step for Typed<Req, Res>
where
    Req: Clone + Send + Sync + 'static,
    Res: Send + 'static,
{
    fn run<'a>(&'a self, ctx: &'a Context) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let result = self.handler.handle(ctx, self.request.clone()).await;
            let outcome = result.as_ref().map(|_| ()).map_err(Clone::clone);
            // 槽只在这里整体赋值，中毒时照常使用
            *self.outcome.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
            outcome
        })
    }
}

/// Observe 阶段的内置步骤：只接住排在它之后的中间件与处理器的 panic（RFC-0011 规则 5、10）。
///
/// 排在它之前的（Ingress 阶段，或 Observe 阶段 `Beginning` 上归属的序数排在 [`KERNEL_OWNER`] 之前的）panic 原样交给调用方，与 .NET 一致。
struct ObserveStep;

impl Middleware for ObserveStep {
    fn name(&self) -> &str {
        OBSERVE_MIDDLEWARE
    }

    fn invoke<'a>(&'a self, ctx: &'a Context, next: Next<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(CatchPanic(next.run(ctx)))
    }
}

/// Resilience 阶段的内置步骤：进入时按时钟核对截止时间，已到即以 `KERNEL_TIMEOUT_001` + `kernel.invocation.deadline_before_start`
/// 结束，后面的中间件与处理器不执行；进入时不核对取消，途中的超时与取消按上下文的截止令牌归因（RFC-0003 第 1 条）。
struct DeadlineStep;

impl Middleware for DeadlineStep {
    fn name(&self) -> &str {
        DEADLINE_MIDDLEWARE
    }

    fn invoke<'a>(&'a self, ctx: &'a Context, next: Next<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if ctx.is_expired(SystemTime::now()) {
                return Err(Error::timeout(ERROR_CODE_DEADLINE_EXCEEDED, MESSAGE_DEADLINE_BEFORE_START));
            }
            next.run(ctx).await
        })
    }
}

/// Validate 阶段的内置步骤：原始调用在这里解码载荷，失败即以 `KERNEL_VALIDATION_001` 结束；类型化调用没有载荷，跳过。
struct ValidationStep;

impl Middleware for ValidationStep {
    fn name(&self) -> &str {
        VALIDATION_MIDDLEWARE
    }

    fn invoke<'a>(&'a self, ctx: &'a Context, next: Next<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let (Some(slot), Some(descriptor), Some(request)) = (ctx.item::<DecodedRequest>(), current(ctx), request_of(ctx)) {
                let decoded = descriptor.executor.decode(descriptor.id(), &request.payload)?;
                *slot.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(decoded);
            }
            next.run(ctx).await
        })
    }
}

/// 管道执行中的 panic 转成失败结果（RFC-0011 规则 5、10）：中间件或处理器 panic 时调用以 `KERNEL_UNEXPECTED_001` +
/// `kernel.invocation.unhandled_exception` 结束、没有参数，不把 panic 抛给调用方；panic 来自框架错误（原语的非法参数等，
/// 消息是 `<错误码>: <说明>`）时，那个错误放进 `inner`，与 .NET 的 Observe 阶段一致。取消不是 panic，按上下文归因。
///
/// 只在 `panic = "unwind"` 下有效；panic 之后不再轮询内层 future，对象袋的作用域照常由 `Scope` 恢复。
struct CatchPanic<'a>(BoxFuture<'a, Result<()>>);

impl Future for CatchPanic<'_> {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, context: &mut std::task::Context<'_>) -> Poll<Result<()>> {
        // AssertUnwindSafe：panic 之后内层 future 只会被丢弃，不会再被轮询，它里面可能半改的状态不会再被读到
        match panic::catch_unwind(AssertUnwindSafe(|| self.0.as_mut().poll(context))) {
            Ok(poll) => poll,
            Err(payload) => Poll::Ready(Err(unhandled(payload.as_ref()))),
        }
    }
}

/// 把 panic 的内容翻译成未处理失败；内容是框架错误的文本 `<错误码>: <说明>` 时，它成为内部错误。
fn unhandled(payload: &(dyn Any + Send)) -> Error {
    let error = Error::unexpected(ERROR_CODE_UNEXPECTED, MESSAGE_UNHANDLED_PANIC);
    let text = payload.downcast_ref::<String>().map(String::as_str).or_else(|| payload.downcast_ref::<&str>().copied());
    let framework = text.and_then(|text| text.split_once(": ")).filter(|(code, message)| is_error_code(code) && !message.is_empty());
    match framework {
        Some((code, message)) => error.with_inner(Error::literal(code, message)),
        None => error,
    }
}

/// 让操作经过完整管道执行。
pub struct Invoker {
    registry: Arc<Registry>,
    pipeline: Pipeline,
}

impl Invoker {
    /// 用登记表与管道构建调用器；管道排序失败时返回 `KERNEL_PIPELINE_00x`。
    ///
    /// 管道里先放三个内置步骤，归属 [`KERNEL_OWNER`]、都在所在阶段的 `Beginning`，与 .NET 内置中间件的位置相同（RFC-0011 规则 10）：
    /// Observe 阶段的 [`OBSERVE_MIDDLEWARE`] 翻译 panic，Resilience 阶段的 [`DEADLINE_MIDDLEWARE`] 核对截止时间，Validate 阶段的
    /// [`VALIDATION_MIDDLEWARE`] 解码载荷。它们与 `builder` 里同位置的中间件按 RFC-0004 的规则排序（归属的序数序，同归属按登记顺序，
    /// 内置步骤先登记），顺序约束可以按这几个名字引用它们；`builder` 里登记了同名的中间件时返回 `KERNEL_PIPELINE_003`。
    pub fn new(registry: Arc<Registry>, builder: &pipeline::Builder) -> std::result::Result<Self, FrameworkError> {
        let mut pipeline = pipeline::Builder::new();
        pipeline.use_at(ObserveStep, PipelineStage::Observe, Placement::Beginning, KERNEL_OWNER)?;
        pipeline.use_at(DeadlineStep, PipelineStage::Resilience, Placement::Beginning, KERNEL_OWNER)?;
        pipeline.use_at(ValidationStep, PipelineStage::Validate, Placement::Beginning, KERNEL_OWNER)?;
        pipeline.extend_from(builder)?;
        Ok(Self { registry, pipeline: pipeline.build()? })
    }

    /// 登记表。
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// 管道，含三个内置步骤。
    pub fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    /// 执行一个 XGI 请求，成功时返回响应载荷。
    ///
    /// 未登记返回 `KERNEL_NOTFOUND_001`，不进管道；否则把操作与请求放进上下文，经完整管道执行：Resilience 阶段进入时截止时间已到返回
    /// `KERNEL_TIMEOUT_001` + `kernel.invocation.deadline_before_start`，Validate 阶段解码载荷，handle 阶段之后执行、编码；
    /// 排在 Observe 阶段内置步骤之后的 panic 成为 `KERNEL_UNEXPECTED_001`，之前的原样交给调用方。中间件以成功短路、没有执行操作时返回 `KERNEL_UNEXPECTED_001` + `kernel.operation.no_response`（规则 9）；载荷不合法时 `reason` 是出错位置的 JSON 路径（如 `$.quantity`）。
    ///
    /// 载荷先过共同的 JSON 读取检查，再按请求类型的 `Deserialize` 解码；必填、可选与接受 `null` 的成员怎么声明见 [`nullable`]。
    // 返回类型写明 Send：签名即承诺 future 可以跨线程（gRPC 服务端要求），实现改动不会悄悄破坏这一点
    #[allow(clippy::manual_async_fn)]
    pub fn invoke<'a>(&'a self, ctx: &'a Context, request: Request) -> impl Future<Output = Result<Vec<u8>>> + Send + 'a {
        async move {
            let descriptor = self.registry.get(&request.id).ok_or_else(|| not_found(&request.id))?;
            let request = Arc::new(request);
            let scope = Scope::enter(ctx, Arc::clone(&descriptor), Some(Arc::clone(&request)));
            let terminal = Dispatch { descriptor: &descriptor, payload: &request.payload };
            let outcome = self.pipeline.run(ctx, &terminal).await;
            let response = scope.take_response();
            drop(scope);
            outcome?;
            response.ok_or_else(|| no_response(descriptor.id()))
        }
    }

    /// 同 [`Invoker::invoke`]，但 future 持有调用器与上下文、不借用调用方，可以 `'static` 地交给执行器（如 `tokio::spawn`）。
    pub fn invoke_owned(self: &Arc<Self>, ctx: Context, request: Request) -> impl Future<Output = Result<Vec<u8>>> + Send + 'static {
        let invoker = Arc::clone(self);
        async move { invoker.invoke(&ctx, request).await }
    }

    /// 类型化入口：不经 JSON，直接以请求对象执行。
    ///
    /// 进管道前只核对两项，按这个顺序：未登记返回 `KERNEL_NOTFOUND_001`，请求或响应类型与登记不符返回 `KERNEL_VALIDATION_002`；
    /// 其余同 [`Invoker::invoke`]，Validate 阶段的内置步骤跳过解码。
    /// 请求要可克隆：管道里的中间件可以多次进入下游（如重试），每次交给处理器一份克隆。
    // 返回类型写明 Send：签名即承诺 future 可以跨线程（gRPC 服务端要求），实现改动不会悄悄破坏这一点
    #[allow(clippy::manual_async_fn)]
    pub fn invoke_typed<'a, Req, Res>(&'a self, ctx: &'a Context, id: &'a str, request: Req) -> impl Future<Output = Result<Res>> + Send + 'a
    where
        Req: Clone + Send + Sync + 'static,
        Res: Send + 'static,
    {
        async move {
            let descriptor = self.registry.get(id).ok_or_else(|| not_found(id))?;
            let handler = descriptor.typed.downcast_ref::<Arc<dyn Handler<Req, Res>>>().ok_or_else(|| type_mismatch(id))?;
            let terminal = Typed { handler: Arc::clone(handler), request, outcome: Mutex::new(None) };
            let scope = Scope::enter(ctx, Arc::clone(&descriptor), None);
            let outcome = self.pipeline.run(ctx, &terminal).await;
            drop(scope);
            outcome?;
            terminal.outcome.into_inner().unwrap_or_else(PoisonError::into_inner).unwrap_or_else(|| Err(no_response(id)))
        }
    }
}

impl fmt::Debug for Invoker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Invoker").field("registry", &self.registry).field("pipeline", &self.pipeline).finish()
    }
}

/// 解码请求载荷（RFC-0011 规则 2）：空载荷等价于 `{}`；先过共同的 JSON 读取检查，再按请求类型解码，失败为 `KERNEL_VALIDATION_001`。
///
/// `reason` 是出错位置的路径：语法错误（含 BOM、嵌套过深、孤立的代理项）与 `null` 载荷写 `$`，重复成员写第一处重复的路径、
/// 先于一切类型问题；其余按文本顺序报第一处类型不符，缺少必填成员在所在对象结束时报该对象的路径（根上为 `$`）。
fn decode<Req: DeserializeOwned>(id: &str, payload: &[u8]) -> Result<Req> {
    // 只有长度为 0 的载荷等价于 {}：只有空白的不是 JSON
    let payload = if payload.is_empty() { b"{}".as_slice() } else { payload };
    let node = json::parse(payload).map_err(|error| invalid_payload(id, error.path().to_owned()))?;
    if node == Node::Null {
        return Err(invalid_payload(id, "$".to_owned()));
    }
    json::decode(&node).map_err(|error| invalid_payload(id, error.path().to_owned()))
}

/// 请求类型里必填而接受 `null` 的成员：`#[serde(deserialize_with = "operation::nullable")] name: Option<String>`（RFC-0011 规则 2）。
///
/// 必填只要求成员出现：缺少时解码失败、报所在对象的路径，写成 `null` 为 `None`。其余的声明照 serde 的惯例：
///
/// | 规范 | Rust 的请求类型 |
/// | --- | --- |
/// | 必填，不接受 `null` | 不带 `#[serde(default)]` 的非 `Option` 成员：`n: i64` |
/// | 必填，接受 `null` | `#[serde(deserialize_with = "operation::nullable")] name: Option<String>` |
/// | 可选，不接受 `null`（整数、浮点、布尔） | `#[serde(default)] count: i64`，缺少时取缺省值 |
/// | 可选，接受 `null`（字符串、数组、对象） | `Option<T>`：缺少与 `null` 都是 `None` |
/// | 任意 JSON | `serde_json::Value` 或 `Option<Value>` |
///
/// 数组项同理：接受 `null` 的字符串项写 `Vec<Option<String>>`。成员名区分大小写，未知成员忽略，数字不从字符串读，整数不接受
/// `1.0` 与 `1e2`、超出类型的范围即不符，浮点接受整数写法、上溢为无穷；对象位置只收 JSON 对象。
///
/// ```
/// use serde::Deserialize;
/// use xihan_framework::operation;
/// use xihan_framework::primitives::json;
///
/// #[derive(Debug, Deserialize)]
/// struct Profile {
///     #[serde(deserialize_with = "operation::nullable")]
///     name: Option<String>,
///     #[serde(default)]
///     count: i64,
/// }
///
/// assert_eq!(json::from_str::<Profile>(r#"{"name":null}"#).unwrap().name, None);
/// assert_eq!(json::from_str::<Profile>(r#"{"count":1}"#).unwrap_err().path(), "$");
/// assert_eq!(json::from_str::<Profile>(r#"{"name":"a","count":null}"#).unwrap_err().path(), "$.count");
/// ```
pub fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> std::result::Result<Option<T>, D::Error> {
    Option::<T>::deserialize(deserializer)
}

fn invalid_payload(id: &str, reason: String) -> Error {
    Error::validation(ERROR_CODE_INVALID_PAYLOAD, MESSAGE_INVALID_PAYLOAD).with_detail("id", id).with_detail("reason", reason)
}

fn not_found(id: &str) -> Error {
    Error::not_found(ERROR_CODE_NOT_FOUND, MESSAGE_NOT_FOUND).with_detail("id", id)
}

/// 类型化调用的类型与登记不符：只带 `id`（ADR-0236），实现语言的类型名各语言写不出同样的参数。
fn type_mismatch(id: &str) -> Error {
    Error::validation(ERROR_CODE_TYPE_MISMATCH, MESSAGE_TYPE_MISMATCH).with_detail("id", id)
}

/// 管道以成功结束而没有响应：某个中间件以成功短路、操作没有执行（规则 9）。
fn no_response(id: &str) -> Error {
    Error::unexpected(ERROR_CODE_UNEXPECTED, MESSAGE_NO_RESPONSE).with_detail("id", id)
}

fn invalid(message: String) -> FrameworkError {
    FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, message)
}

/// 类型的短名：去掉每段路径的模块前缀，如 `alloc::vec::Vec<my::Order>` 为 `Vec<Order>`。
fn short_type_name<T>() -> String {
    let full = type_name::<T>();
    let mut short = String::with_capacity(full.len());
    let mut rest = full;
    while let Some(index) = rest.find("::") {
        short.push_str(&rest[..index]);
        // 去掉刚写入的路径段：回退到上一个不属于标识符的字符之后
        let keep = short.char_indices().rev().find(|(_, char)| !(char.is_alphanumeric() || *char == '_')).map_or(0, |(index, char)| index + char.len_utf8());
        short.truncate(keep);
        rest = &rest[index + 2..];
    }
    short.push_str(rest);
    short
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Order;

    #[test]
    fn short_type_names_drop_module_paths() {
        assert_eq!(short_type_name::<Order>(), "Order");
        assert_eq!(short_type_name::<Vec<Option<Order>>>(), "Vec<Option<Order>>");
        assert_eq!(short_type_name::<(u32, String)>(), "(u32, String)");
        assert_eq!(short_type_name::<std::collections::BTreeMap<String, Order>>(), "BTreeMap<String, Order>");
    }

    #[test]
    fn ids_follow_the_grammar() {
        for id in ["orders.create", "order-lines.list", "a1.b2", "tenant-members.list-online"] {
            assert!(is_id(id), "{id}");
        }
        for id in [
            "orders",
            "Orders.create",
            "orders.Create",
            "orders.create.now",
            "orders-.create",
            "orders.-create",
            "1orders.create",
            "orders_x.create",
            ".create",
        ] {
            assert!(!is_id(id), "{id}");
        }
    }
}
