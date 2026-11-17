//! 宿主回调（`invocation.md` §5）：插件以会话令牌回调宿主的 `Host.Invoke` 与 `Host.Publish`，宿主以插件身份执行。

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::SystemTime;

use tonic::Code;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

use super::convert::{metadata_of, proto_envelope, result_of};
use super::endpoint::{Connector, Endpoint};
use super::messages::{PHASE_PUBLISH, REASON_NO_ENDPOINT, denied, failed, host_unavailable, session_invalid, tenant_denied};
use super::types::Envelope;
use super::v1::host_client::HostClient;
use super::{PERMISSION_MESSAGING, PERMISSION_PIPELINE, SESSION_METADATA_KEY, v1};
use crate::invocation::{Context, ERROR_CODE_CANCELLED, ERROR_CODE_DEADLINE_EXCEEDED, MESSAGE_CANCELLED, MESSAGE_DEADLINE_EXCEEDED};
use crate::operation::{ERROR_CODE_INVALID_PAYLOAD, ERROR_CODE_NOT_FOUND, MESSAGE_INVALID_PAYLOAD, MESSAGE_NOT_FOUND};
use crate::primitives::{Error, Result};

/// 宿主回调服务的客户端：每次调用带会话令牌，追踪、截止时间与已登记的 Baggage 取自调用上下文（`invocation.md` §5）。
///
/// 在调用与投递里经 [`host`] 取得，在调用之外经 [`Configuration::host`](super::Configuration::host)。发出之前依次核对，不合即不发请求：
///
/// 1. 回调端点：`XIHAN_PLUGIN_HOST_ADDRESS` 里没有本运行时能连的项时（[`Host::endpoint`] 为 `None`），`PLUGINS_UNAVAILABLE_001` +
///    `plugins.host.unavailable`（`reason` 为 `no supported callback endpoint`）；
/// 2. 权限：所需的权限（`Host.Invoke` 要 `capability:kernel.pipeline`，`Host.Publish` 要 `capability:messaging.queue`）在清单
///    `permissions` 里、也在 `Configure` 下发的 `granted_permissions` 里，否则 `PLUGINS_DENIED_002` + `plugins.callback.denied`；
/// 3. 租户：上下文的 `xihan.tenant` 必须是宿主此刻交给本插件处理的某个调用或投递的租户（同一租户可以同时有多个），否则
///    `PLUGINS_DENIED_002` + `plugins.callback.tenant_denied`；宿主没有在处理本插件的调用时（调用已结束、后台回调）不能带租户；
/// 4. 上下文的截止时间已过按 `DeadlineExceeded`（`KERNEL_TIMEOUT_001`），已被取消按「`Cancelled`，取消信号已触发」
///    （`KERNEL_CANCELLED_001`），与下面映射表的那一行相同；发出之后上下文取消时放弃这次调用，按同样的两行给出结果。
///
/// 第 2、3 项与宿主的顺序相同、给出同样的错误；`Host.Publish` 的每一项都是整批的结果。信封的 `xihantenant` 扩展不在本地核对，由宿主
/// 逐项判定。
///
/// 宿主的 gRPC 状态按 `invocation.md` §5 的表映射（向量 `plugin/callback-status.json`），只按表、不另按时钟或令牌改写（超时的归因交给
/// 插件自己管道里的截止时间步骤，RFC-0003 规则 1）：`Unauthenticated` 为 `plugins.callback.session_invalid`，`PermissionDenied` 为
/// `plugins.callback.denied`，`DeadlineExceeded` 为 `KERNEL_TIMEOUT_001`，上下文已取消时的 `Cancelled` 为 `KERNEL_CANCELLED_001`，
/// 其余的 `Cancelled`、`Unavailable` 与连接失败为 `plugins.host.unavailable`，`Host.Invoke` 的 `Unimplemented` 与 `InvalidArgument`
/// 为 `kernel.operation.unknown` 与 `kernel.operation.invalid_payload`，其余为 `plugins.process.failed`。tonic 按截止时间在本端中止
/// 调用时报的 `Cancelled`（`Timeout expired`）先换成 `DeadlineExceeded`。
pub struct Host {
    endpoint: Option<Endpoint>,
    plugin: String,
    session: MetadataValue<tonic::metadata::Ascii>,
    client: Option<HostClient<Channel>>,
    grants: Arc<Grants>,
    tenants: Arc<Tenants>,
}

/// 对象袋里放宿主客户端的类型。
#[derive(Clone)]
pub(crate) struct HostHandle(pub(crate) Arc<Host>);

/// 取当前调用的宿主客户端：运行时在每次 `Invoke` 与 `HandleEvent` 的上下文里放入它；上下文不是运行时交给处理器的（不在插件的调用里）
/// 时为 `None`。宿主没有给出能连的回调端点时同样有客户端，回调以 `plugins.host.unavailable` 失败。
pub fn host(ctx: &Context) -> Option<Arc<Host>> {
    ctx.item::<HostHandle>().map(|handle| Arc::clone(&handle.0))
}

/// 回调的两种 RPC（`invocation.md` §5）；`Invoke` 带操作 ID。
#[derive(Clone, Copy, Debug)]
pub(crate) enum Rpc<'a> {
    /// `Host.Invoke`。
    Invoke(&'a str),
    /// `Host.Publish`。
    Publish,
}

impl Rpc<'_> {
    /// 所需的权限。
    fn permission(self) -> &'static str {
        match self {
            Self::Invoke(_) => PERMISSION_PIPELINE,
            Self::Publish => PERMISSION_MESSAGING,
        }
    }

    /// 错误的 `phase`：操作 ID 或 `publish`。
    fn phase(&self) -> &str {
        match self {
            Self::Invoke(operation) => operation,
            Self::Publish => PHASE_PUBLISH,
        }
    }
}

impl Host {
    /// 用选好的回调端点创建；连接是惰性的，第一次回调时才建立。没有端点时不建连接，回调在本地失败。要在 tokio 运行时里调用。
    pub(crate) fn new(
        endpoint: Option<Endpoint>,
        plugin: &str,
        session: &str,
        grants: Arc<Grants>,
        tenants: Arc<Tenants>,
    ) -> std::result::Result<Self, String> {
        let session = MetadataValue::try_from(session).map_err(|_| format!("The session token cannot be sent as the '{SESSION_METADATA_KEY}' metadata."))?;
        // URI 只是占位：真正的地址来自端点，连接器按它拨号
        let client = endpoint.clone().map(|endpoint| {
            HostClient::new(tonic::transport::Endpoint::from_static("http://xihan.plugin.host").connect_with_connector_lazy(Connector { endpoint }))
        });
        Ok(Self { endpoint, plugin: plugin.to_owned(), session, client, grants, tenants })
    }

    /// 回调用的宿主端点：`XIHAN_PLUGIN_HOST_ADDRESS` 里第一个本运行时能连的项；没有时为 `None`，回调以 `plugins.host.unavailable` 失败。
    pub fn endpoint(&self) -> Option<&Endpoint> {
        self.endpoint.as_ref()
    }

    /// 以插件身份经宿主的完整管道调用一个操作（`Host.Invoke`），成功时返回响应载荷；`payload` 是请求类型的 JSON，空等价于 `{}`。
    ///
    /// 宿主的业务失败原样返回（`Status.error` 按 `invocation.md` §3 读入）；本地核对与 gRPC 状态的映射见 [`Host`]。调用随上下文的
    /// 截止时间设置 gRPC 截止时间，上下文取消时放弃这次调用。
    pub async fn invoke(&self, ctx: &Context, operation: &str, payload: impl Into<Vec<u8>>) -> Result<Vec<u8>> {
        let rpc = Rpc::Invoke(operation);
        let mut client = self.check(ctx, rpc)?;
        let request = self.request(ctx, v1::InvokeRequest { operation: operation.to_owned(), payload: payload.into(), metadata: Some(metadata_of(ctx)) });
        let response = tokio::select! {
            biased;
            // 上下文取消就放弃这次调用：丢弃请求的 future 会让 gRPC 取消它，宿主那边随之取消
            () = ctx.done() => return Err(self.abandoned(ctx, rpc)),
            response = client.invoke(request) => response,
        };
        match response {
            Ok(response) => {
                let response = response.into_inner();
                result_of(response.status.as_ref(), &self.plugin, operation).map(|()| response.payload)
            }
            Err(status) => Err(self.status_error(ctx, rpc, &status)),
        }
    }

    /// 经宿主的消息代理发布消息（`Host.Publish`），结果与 `envelopes` 一一对应；空列表不发请求。
    ///
    /// 本地核对与 RPC 失败都是整批的结果：每一项都是同一个错误。宿主返回的结果数与信封数不同时，每一项都是 `PLUGINS_UNEXPECTED_001` +
    /// `plugins.process.failed`（`phase` 为 `publish`）。宿主在这次 RPC 的上下文里发布，信封缺的租户、主体等扩展属性由宿主从上下文补齐。
    pub async fn publish(&self, ctx: &Context, envelopes: Vec<Envelope>) -> Vec<Result<()>> {
        let count = envelopes.len();
        if count == 0 {
            return Vec::new();
        }
        let mut client = match self.check(ctx, Rpc::Publish) {
            Ok(client) => client,
            Err(error) => return vec![Err(error); count],
        };
        let request =
            self.request(ctx, v1::PublishRequest { envelopes: envelopes.into_iter().map(proto_envelope).collect(), metadata: Some(metadata_of(ctx)) });
        let response = tokio::select! {
            biased;
            () = ctx.done() => return vec![Err(self.abandoned(ctx, Rpc::Publish)); count],
            response = client.publish(request) => response,
        };
        match response {
            Ok(response) => {
                let results = response.into_inner().results;
                if results.len() != count {
                    let error = failed(&self.plugin, PHASE_PUBLISH, format!("the host returned {} results for {count} envelopes", results.len()));
                    return vec![Err(error); count];
                }
                results.iter().map(|status| result_of(Some(status), &self.plugin, PHASE_PUBLISH)).collect()
            }
            Err(status) => vec![Err(self.status_error(ctx, Rpc::Publish, &status)); count],
        }
    }

    /// 发出之前的核对（`invocation.md` §5），依次：回调端点，权限，租户，上下文的截止时间与取消；通过时返回客户端。
    fn check(&self, ctx: &Context, rpc: Rpc<'_>) -> Result<HostClient<Channel>> {
        let Some(client) = &self.client else {
            return Err(host_unavailable(&self.plugin, REASON_NO_ENDPOINT));
        };
        let permission = rpc.permission();
        if !self.grants.allows(permission) {
            return Err(denied(&self.plugin, permission));
        }
        if let Some(tenant) = ctx.tenant().filter(|tenant| !self.tenants.contains(tenant)) {
            return Err(tenant_denied(&self.plugin, tenant));
        }
        // 注定超时或已被放弃的调用不发出；结果与映射表相应的一行相同
        if ctx.is_expired(SystemTime::now()) || ctx.is_cancelled() {
            return Err(self.abandoned(ctx, rpc));
        }
        Ok(client.clone())
    }

    /// 包装请求：带会话令牌，按上下文的剩余时间设置 gRPC 截止时间（与 `metadata.deadline` 相同）。
    fn request<T>(&self, ctx: &Context, message: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        request.metadata_mut().insert(SESSION_METADATA_KEY, self.session.clone());
        if let Some(remaining) = ctx.remaining_time(SystemTime::now()) {
            request.set_timeout(remaining);
        }
        request
    }

    /// 上下文的截止时间已过或已被取消、回调不发出或被放弃时的结果（`invocation.md` §5）：截止时间已过按 `DeadlineExceeded`，否则按
    /// 「`Cancelled`，取消信号已触发」。上下文的截止时间就是 gRPC 截止时间，到点时 gRPC 自己也以截止时间中止，两者给出同一行。
    fn abandoned(&self, ctx: &Context, rpc: Rpc<'_>) -> Error {
        let code = if ctx.is_expired(SystemTime::now()) { Code::DeadlineExceeded } else { Code::Cancelled };
        callback_failure(rpc, code, "", true, &self.plugin)
    }

    /// gRPC 失败 → 错误：库自己的叫法先换成标准状态，再只按 [`callback_failure`] 的表映射。
    fn status_error(&self, ctx: &Context, rpc: Rpc<'_>, status: &tonic::Status) -> Error {
        callback_failure(rpc, standard_code(status), status.message(), ctx.is_cancelled(), &self.plugin)
    }
}

/// gRPC 库自己的叫法换成标准状态（`invocation.md` §5）：tonic 按截止时间在本端中止调用时以 `Cancelled` 报 `Timeout expired`
/// （[`tonic::TimeoutExpired`]），按 `DeadlineExceeded`；其余原样。
fn standard_code(status: &tonic::Status) -> Code {
    if status.code() == Code::Cancelled && status.message() == tonic::TimeoutExpired(()).to_string() { Code::DeadlineExceeded } else { status.code() }
}

impl fmt::Debug for Host {
    // 会话令牌不写进调试输出
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Host").field("endpoint", &self.endpoint).field("plugin", &self.plugin).finish_non_exhaustive()
    }
}

/// gRPC 状态码 0–16 的 PascalCase 名字（`invocation.md` §3），下标即状态码。
const STATUS_NAMES: [&str; 17] = [
    "OK",
    "Cancelled",
    "Unknown",
    "InvalidArgument",
    "DeadlineExceeded",
    "NotFound",
    "AlreadyExists",
    "PermissionDenied",
    "ResourceExhausted",
    "FailedPrecondition",
    "Aborted",
    "OutOfRange",
    "Unimplemented",
    "Internal",
    "Unavailable",
    "DataLoss",
    "Unauthenticated",
];

/// `reason` 里的 `<状态名>: <说明>`（`invocation.md` §3）：说明原样取状态的消息，可以为空。
fn status_reason(code: Code, message: &str) -> String {
    // tonic 把不认识的状态码读成 Unknown，下标总在表里
    let name = usize::try_from(i32::from(code)).ok().and_then(|index| STATUS_NAMES.get(index)).copied().unwrap_or("Unknown");
    format!("{name}: {message}")
}

/// 回调的 gRPC 失败映射成错误（`invocation.md` §5 的表，向量 `plugin/callback-status.json`）：`code` 与 `message` 是 gRPC 状态
/// （连接失败按 `Unavailable`），`context_cancelled` 是回调所在的调用上下文的取消信号是否已触发。
pub(crate) fn callback_failure(rpc: Rpc<'_>, code: Code, message: &str, context_cancelled: bool, plugin: &str) -> Error {
    match (code, rpc) {
        (Code::Unauthenticated, _) => session_invalid(),
        (Code::PermissionDenied, _) => denied(plugin, rpc.permission()),
        (Code::DeadlineExceeded, _) => Error::timeout(ERROR_CODE_DEADLINE_EXCEEDED, MESSAGE_DEADLINE_EXCEEDED),
        // Cancelled 只在调用方确实取消时算取消；调用方没有取消的 Cancelled 是连接被宿主或网络切断，与 Unavailable 同样处理
        (Code::Cancelled, _) if context_cancelled => Error::cancelled(ERROR_CODE_CANCELLED, MESSAGE_CANCELLED),
        (Code::Cancelled | Code::Unavailable, _) => host_unavailable(plugin, status_reason(code, message)),
        // Unimplemented 与 InvalidArgument 只对 Host.Invoke 有操作层面的含义；Host.Publish 的这两种落到最后一项
        (Code::Unimplemented, Rpc::Invoke(operation)) => Error::not_found(ERROR_CODE_NOT_FOUND, MESSAGE_NOT_FOUND).with_detail("id", operation),
        (Code::InvalidArgument, Rpc::Invoke(operation)) => {
            Error::validation(ERROR_CODE_INVALID_PAYLOAD, MESSAGE_INVALID_PAYLOAD).with_detail("id", operation).with_detail("reason", message)
        }
        _ => failed(plugin, rpc.phase(), status_reason(code, message)),
    }
}

/// 插件的权限：清单声明的与宿主授予的，两边都有才放行。
pub(crate) struct Grants {
    declared: Vec<String>,
    granted: RwLock<Vec<String>>,
}

impl Grants {
    /// 以清单声明的权限创建；`Configure` 之前什么都没授予。
    pub(crate) fn new(declared: Vec<String>) -> Self {
        Self { declared, granted: RwLock::new(Vec::new()) }
    }

    /// 记下 `Configure` 下发的授予。
    pub(crate) fn grant(&self, granted: Vec<String>) {
        // 锁里只有整体赋值，中毒时照常使用
        *self.granted.write().unwrap_or_else(PoisonError::into_inner) = granted;
    }

    /// 声明并且已被授予；授予多于声明的部分不放行。
    pub(crate) fn allows(&self, permission: &str) -> bool {
        self.declared.iter().any(|declared| declared == permission)
            && self.granted.read().unwrap_or_else(PoisonError::into_inner).iter().any(|granted| granted == permission)
    }
}

/// 宿主此刻交给插件处理的调用与投递的租户：按租户计数，调用结束时减掉；条目数不超过在途调用数。
#[derive(Default)]
pub(crate) struct Tenants {
    active: Mutex<HashMap<String, usize>>,
}

impl Tenants {
    /// 登记一个在途调用的租户；不带租户的调用不登记。返回的凭据丢弃时撤销。
    pub(crate) fn enter(self: &Arc<Self>, tenant: Option<&str>) -> TenantGuard {
        if let Some(tenant) = tenant {
            *self.lock().entry(tenant.to_owned()).or_insert(0) += 1;
        }
        TenantGuard { tenants: Arc::clone(self), tenant: tenant.map(str::to_owned) }
    }

    /// 判断这个租户是不是某个在途调用的租户。
    pub(crate) fn contains(&self, tenant: &str) -> bool {
        self.lock().contains_key(tenant)
    }

    // 锁里只有计数的增减，中毒时照常使用
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, usize>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 在途调用的租户凭据：宿主的这次 RPC 结束（含宿主取消）时丢弃。
pub(crate) struct TenantGuard {
    tenants: Arc<Tenants>,
    tenant: Option<String>,
}

impl Drop for TenantGuard {
    fn drop(&mut self) {
        if let Some(tenant) = self.tenant.take() {
            let mut active = self.tenants.lock();
            if let Some(count) = active.get_mut(&tenant) {
                *count -= 1;
                if *count == 0 {
                    active.remove(&tenant);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn tenants_are_counted_per_call() {
        let tenants = Arc::new(Tenants::default());
        let first = tenants.enter(Some("t1"));
        let second = tenants.enter(Some("t1"));
        let none = tenants.enter(None);
        assert!(tenants.contains("t1") && !tenants.contains("t2"));
        drop(first);
        assert!(tenants.contains("t1"));
        drop((second, none));
        assert!(!tenants.contains("t1"));
        assert!(tenants.lock().is_empty());
    }

    #[test]
    fn permissions_need_both_declaration_and_grant() {
        let grants = Grants::new(vec![PERMISSION_PIPELINE.to_owned()]);
        assert!(!grants.allows(PERMISSION_PIPELINE), "nothing is granted before Configure");
        grants.grant(vec![PERMISSION_PIPELINE.to_owned(), PERMISSION_MESSAGING.to_owned()]);
        assert!(grants.allows(PERMISSION_PIPELINE));
        assert!(!grants.allows(PERMISSION_MESSAGING), "a grant beyond the declaration does not count");
    }

    /// 回放向量 `plugin/callback-status.json` 的全部用例（含上下文已取消的）；经假的宿主端到端的回放在 `tests/plugin.rs`。
    #[test]
    fn callback_statuses_follow_the_vectors() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/spec/conformance/vectors/plugin/callback-status.json");
        let text = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("cannot read {path}: {error}; run `git submodule update --init`"));
        let vectors: Value = serde_json::from_str(&text).unwrap();
        let (plugin, operation) = (vectors["pluginId"].as_str().unwrap(), vectors["operation"].as_str().unwrap());
        for case in vectors["cases"].as_array().unwrap() {
            let rpc = match case["rpc"].as_str().unwrap() {
                "invoke" => Rpc::Invoke(operation),
                "publish" => Rpc::Publish,
                other => panic!("unknown rpc {other}"),
            };
            let code = Code::from_i32(i32::try_from(case["statusCode"].as_i64().unwrap()).unwrap());
            assert_eq!(status_reason(code, "").trim_end_matches(": "), case["status"].as_str().unwrap(), "{case}");
            let error = callback_failure(rpc, code, case["message"].as_str().unwrap(), case["contextCancelled"].as_bool().unwrap(), plugin);
            assert_eq!((error.code(), error.message_key()), (case["code"].as_str().unwrap(), case["key"].as_str()), "{case}");
            assert_eq!(serde_json::to_value(error.details()).unwrap(), case["details"], "{case}");
        }
        assert_eq!(status_reason(Code::Ok, "x"), "OK: x");
    }

    /// tonic 按截止时间在本端中止调用时以 `Cancelled` 报 `Timeout expired`：先换成 `DeadlineExceeded`（`invocation.md` §5）。
    #[test]
    fn the_library_timeout_is_a_deadline() {
        assert_eq!(standard_code(&tonic::Status::cancelled(tonic::TimeoutExpired(()).to_string())), Code::DeadlineExceeded);
        assert_eq!(standard_code(&tonic::Status::cancelled("Timeout expired")), Code::DeadlineExceeded);
        assert_eq!(standard_code(&tonic::Status::cancelled("the stream was reset")), Code::Cancelled);
        assert_eq!(standard_code(&tonic::Status::unavailable("Timeout expired")), Code::Unavailable);
    }
}
