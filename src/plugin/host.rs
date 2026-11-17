//! 宿主回调（`invocation.md` §5）：插件以会话令牌回调宿主的 `Host.Invoke` 与 `Host.Publish`，宿主以插件身份执行。

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use tonic::Code;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

use super::convert::{metadata_of, proto_envelope, result_of};
use super::endpoint::{Connector, Endpoint};
use super::messages::{denied, failed, session_invalid, tenant_denied, unreachable};
use super::types::Envelope;
use super::v1::host_client::HostClient;
use super::{PERMISSION_MESSAGING, PERMISSION_PIPELINE, SESSION_METADATA_KEY, TENANT_EXTENSION, v1};
use crate::invocation::{Context, ERROR_CODE_CANCELLED, ERROR_CODE_DEADLINE_EXCEEDED, MESSAGE_CANCELLED, MESSAGE_DEADLINE_EXCEEDED};
use crate::operation::{ERROR_CODE_INVALID_PAYLOAD, ERROR_CODE_NOT_FOUND, MESSAGE_INVALID_PAYLOAD, MESSAGE_NOT_FOUND};
use crate::primitives::{Error, Result};

/// 回调 `Host.Invoke` 的阶段名，进错误的 `phase` 参数。
const PHASE_INVOKE: &str = "Host.Invoke";
/// 回调 `Host.Publish` 的阶段名。
const PHASE_PUBLISH: &str = "Host.Publish";

/// 宿主回调服务的客户端：每次调用带会话令牌，追踪、截止时间与已登记的 Baggage 取自调用上下文。
///
/// 在调用与投递里经 [`host`] 取得，在调用之外经 [`Configuration::host`](super::Configuration::host)。发出之前先在本地核对
/// `invocation.md` §5 的两条规则，不合的不发给宿主、直接以 `PLUGINS_DENIED_002` 失败：
///
/// - 所需的权限（`Host.Invoke` 要 `capability:kernel.pipeline`，`Host.Publish` 要 `capability:messaging.queue`）必须在清单
///   `permissions` 里声明并且已被宿主授予（`Configure` 下发的 `granted_permissions`）；
/// - 租户只能是宿主此刻交给本插件处理的某个调用的租户：`Host.Invoke` 看上下文的 `xihan.tenant`，`Host.Publish` 逐项看信封的
///   `xihantenant` 扩展；宿主没有在处理本插件的调用时（调用已结束、后台回调）不能带租户。
///
/// 宿主照样再核对一遍，它是最终的判定者。传输层失败按 `invocation.md` §3 的映射表从插件这一侧对应：宿主不可达为
/// `PLUGINS_UNAVAILABLE_001`，`DeadlineExceeded` 为 `KERNEL_TIMEOUT_001`，调用方取消按上下文的令牌归因，`Unauthenticated` 与
/// `PermissionDenied` 为 `PLUGINS_DENIED_002`，`Unimplemented` 为 `KERNEL_NOTFOUND_001`，`InvalidArgument` 为 `KERNEL_VALIDATION_001`，
/// 其余为 `PLUGINS_UNEXPECTED_001`。
pub struct Host {
    endpoint: Endpoint,
    plugin: String,
    session: MetadataValue<tonic::metadata::Ascii>,
    client: HostClient<Channel>,
    grants: Arc<Grants>,
    tenants: Arc<Tenants>,
}

/// 对象袋里放宿主客户端的类型。
#[derive(Clone)]
pub(crate) struct HostHandle(pub(crate) Arc<Host>);

/// 取当前调用的宿主客户端：运行时在每次 `Invoke` 与 `HandleEvent` 的上下文里放入它；宿主没有下发回调端点、或不在插件的调用里时为 `None`。
pub fn host(ctx: &Context) -> Option<Arc<Host>> {
    ctx.item::<HostHandle>().map(|handle| Arc::clone(&handle.0))
}

impl Host {
    /// 连接宿主的回调端点；连接是惰性的，第一次回调时才建立。要在 tokio 运行时里调用。
    pub(crate) fn new(endpoint: Endpoint, plugin: &str, session: &str, grants: Arc<Grants>, tenants: Arc<Tenants>) -> std::result::Result<Self, String> {
        let session = MetadataValue::try_from(session).map_err(|_| format!("The session token cannot be sent as the '{SESSION_METADATA_KEY}' metadata."))?;
        // URI 只是占位：真正的地址来自端点，连接器按它拨号
        let channel = tonic::transport::Endpoint::from_static("http://xihan.plugin.host").connect_with_connector_lazy(Connector { endpoint: endpoint.clone() });
        Ok(Self { endpoint, plugin: plugin.to_owned(), session, client: HostClient::new(channel), grants, tenants })
    }

    /// 宿主回调端点。
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// 以插件身份经宿主的完整管道调用一个操作（`Host.Invoke`），成功时返回响应载荷；`payload` 是请求类型的 JSON，空等价于 `{}`。
    ///
    /// 宿主的业务失败原样返回；本地核对与传输层失败见 [`Host`]。调用随上下文的截止时间设置 gRPC 截止时间，上下文取消时放弃这次调用。
    pub async fn invoke(&self, ctx: &Context, operation: &str, payload: impl Into<Vec<u8>>) -> Result<Vec<u8>> {
        self.require(PERMISSION_PIPELINE)?;
        if let Some(tenant) = ctx.tenant().filter(|tenant| !self.tenants.contains(tenant)) {
            return Err(tenant_denied(&self.plugin, tenant));
        }
        let request = v1::InvokeRequest { operation: operation.to_owned(), payload: payload.into(), metadata: Some(metadata_of(ctx)) };
        let request = self.request(ctx, request)?;
        let mut client = self.client.clone();
        let response = tokio::select! {
            // 上下文取消就放弃这次调用：丢弃请求的 future 会让 gRPC 取消它，宿主那边随之取消
            () = ctx.done() => return Err(cancellation(ctx)),
            response = client.invoke(request) => response,
        };
        match response {
            Ok(response) => {
                let response = response.into_inner();
                result_of(response.status.as_ref(), &self.plugin, PHASE_INVOKE).map(|()| response.payload)
            }
            Err(status) => Err(self.status_error(&status, ctx, PHASE_INVOKE, Some(operation))),
        }
    }

    /// 经宿主的消息代理发布消息（`Host.Publish`），结果与 `envelopes` 一一对应；空列表不发请求。
    ///
    /// 带了不该带的租户的那一项在本地失败、不发出，其余照常发布；权限不够时每一项都失败。宿主少回了结果时，缺的那几项是
    /// `PLUGINS_UNEXPECTED_001`。
    pub async fn publish(&self, ctx: &Context, envelopes: Vec<Envelope>) -> Vec<Result<()>> {
        if envelopes.is_empty() {
            return Vec::new();
        }
        if let Err(error) = self.require(PERMISSION_MESSAGING) {
            return vec![Err(error); envelopes.len()];
        }
        let mut results: Vec<Option<Result<()>>> = vec![None; envelopes.len()];
        let (mut positions, mut sent) = (Vec::new(), Vec::new());
        for (position, envelope) in envelopes.into_iter().enumerate() {
            match envelope.extensions.get(TENANT_EXTENSION).filter(|tenant| !self.tenants.contains(tenant)) {
                Some(tenant) => results[position] = Some(Err(tenant_denied(&self.plugin, tenant))),
                None => {
                    positions.push(position);
                    sent.push(proto_envelope(envelope));
                }
            }
        }
        if !sent.is_empty() {
            let outcomes = self.send(ctx, sent).await;
            for (position, outcome) in positions.into_iter().zip(outcomes) {
                results[position] = Some(outcome);
            }
        }
        // 每一项都已填上：越界的在本地、其余的按发布结果
        results.into_iter().map(|result| result.unwrap_or_else(|| Err(failed(&self.plugin, PHASE_PUBLISH, "the envelope was not published")))).collect()
    }

    /// 发出一次发布，按顺序给出每一项的结果。
    async fn send(&self, ctx: &Context, envelopes: Vec<v1::Envelope>) -> Vec<Result<()>> {
        let count = envelopes.len();
        let request = match self.request(ctx, v1::PublishRequest { envelopes, metadata: Some(metadata_of(ctx)) }) {
            Ok(request) => request,
            Err(error) => return vec![Err(error); count],
        };
        let mut client = self.client.clone();
        let response = tokio::select! {
            () = ctx.done() => return vec![Err(cancellation(ctx)); count],
            response = client.publish(request) => response,
        };
        match response {
            Ok(response) => {
                let statuses = response.into_inner().results;
                let returned = statuses.len();
                let mut outcomes: Vec<Result<()>> = statuses.iter().take(count).map(|status| result_of(Some(status), &self.plugin, PHASE_PUBLISH)).collect();
                while outcomes.len() < count {
                    outcomes.push(Err(failed(&self.plugin, PHASE_PUBLISH, format!("the host returned {returned} results for {count} envelopes"))));
                }
                outcomes
            }
            Err(status) => vec![Err(self.status_error(&status, ctx, PHASE_PUBLISH, None)); count],
        }
    }

    /// 包装请求：带会话令牌，按上下文的剩余时间设置 gRPC 截止时间；截止时间已经过去时不发出。
    fn request<T>(&self, ctx: &Context, message: T) -> Result<tonic::Request<T>> {
        let mut request = tonic::Request::new(message);
        request.metadata_mut().insert(SESSION_METADATA_KEY, self.session.clone());
        if let Some(remaining) = ctx.remaining_time(SystemTime::now()) {
            // 截止时间已经过去：定时器可能还没触发令牌，直接按超时结束，不发出注定超时的调用
            if remaining == Duration::ZERO {
                return Err(Error::timeout(ERROR_CODE_DEADLINE_EXCEEDED, MESSAGE_DEADLINE_EXCEEDED));
            }
            request.set_timeout(remaining);
        }
        Ok(request)
    }

    /// 权限必须在清单声明并且已被授予（RFC-0017「回调与权限」）。
    fn require(&self, permission: &str) -> Result<()> {
        if self.grants.allows(permission) { Ok(()) } else { Err(denied(&self.plugin, permission)) }
    }

    /// gRPC 状态 → 错误（`invocation.md` §3 的映射表，从插件这一侧看宿主）。
    fn status_error(&self, status: &tonic::Status, ctx: &Context, phase: &str, operation: Option<&str>) -> Error {
        let reason = || format!("{:?}: {}", status.code(), status.message());
        match status.code() {
            // 连接失败与宿主进程已退出，tonic 都报 Unavailable
            Code::Unavailable => unreachable(&self.plugin, phase, reason()),
            Code::DeadlineExceeded => Error::timeout(ERROR_CODE_DEADLINE_EXCEEDED, MESSAGE_DEADLINE_EXCEEDED),
            // 调用方的令牌已触发是调用方取消（截止时间到了的按截止令牌归因）；否则是连接被切断，宿主不可用
            Code::Cancelled if ctx.is_cancelled() => cancellation(ctx),
            Code::Cancelled => unreachable(&self.plugin, phase, reason()),
            Code::Unauthenticated => session_invalid(),
            Code::PermissionDenied => denied(&self.plugin, if phase == PHASE_PUBLISH { PERMISSION_MESSAGING } else { PERMISSION_PIPELINE }),
            Code::Unimplemented => {
                let error = Error::not_found(ERROR_CODE_NOT_FOUND, MESSAGE_NOT_FOUND);
                match operation {
                    Some(operation) => error.with_detail("id", operation),
                    None => error.with_detail("id", phase),
                }
            }
            Code::InvalidArgument => Error::validation(ERROR_CODE_INVALID_PAYLOAD, MESSAGE_INVALID_PAYLOAD)
                .with_detail("id", operation.unwrap_or(phase))
                .with_detail("reason", status.message()),
            _ => failed(&self.plugin, phase, reason()),
        }
    }
}

impl fmt::Debug for Host {
    // 会话令牌不写进调试输出
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Host").field("endpoint", &self.endpoint).field("plugin", &self.plugin).finish_non_exhaustive()
    }
}

/// 上下文的取消归因：截止令牌触发为超时，否则为取消（RFC-0003 规则 1）。
fn cancellation(ctx: &Context) -> Error {
    ctx.cancellation().unwrap_or_else(|| Error::cancelled(ERROR_CODE_CANCELLED, MESSAGE_CANCELLED))
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

/// 宿主此刻交给插件处理的调用的租户：按租户计数，调用结束时减掉；条目数不超过在途调用数。
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
}
