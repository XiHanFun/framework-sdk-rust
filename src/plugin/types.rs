//! 插件作者接触的类型：订阅与事件、健康、配置与生命周期钩子。

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;

use super::host::Host;
use crate::BoxFuture;
use crate::invocation::Context;
use crate::primitives::json::{self, ReadError};
use crate::primitives::{Error, LifecycleState, Result};

/// 订阅方式（RFC-0009「订阅方式」）：宿主按它向消息代理订阅主题，投递时在 `HandleEventRequest.mode` 里带上（`invocation.md` §4）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SubscriptionMode {
    /// `competing`：订阅了主题的宿主实例里只有一个把消息交给插件；协议里的空同它。
    #[default]
    Competing,
    /// `broadcast`：每个在线的宿主实例都把消息交给自己的插件实例，尽力而为、不进死信。
    Broadcast,
}

impl SubscriptionMode {
    /// 协议里的名字：`competing` 或 `broadcast`。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Competing => "competing",
            Self::Broadcast => "broadcast",
        }
    }

    /// 按协议里的名字解析：空与 `competing` 是竞争消费，`broadcast` 是广播，其余为 `None`。
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "" | "competing" => Some(Self::Competing),
            "broadcast" => Some(Self::Broadcast),
            _ => None,
        }
    }
}

impl fmt::Display for SubscriptionMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 一个事件订阅：主题、可选的只投递的 CloudEvents `type`，以及订阅方式（`plugin.proto` 的 `Subscription`）。
///
/// 同一（主题，类型）对只能声明一次，不能以两种方式各声明一次（`handshake.md` §3 第 5 条）；同一主题的不同类型可以各用一种方式。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Subscription {
    /// 主题（RFC-0009 文法 `^[a-z0-9][a-z0-9._-]{0,127}$`）。
    pub topic: String,
    /// 只投递这个 `type` 的消息；`None` 表示主题下全部。
    pub message_type: Option<String>,
    /// 订阅方式。
    pub mode: SubscriptionMode,
}

impl Subscription {
    /// 竞争消费主题下的全部消息。
    pub fn new(topic: impl Into<String>) -> Self {
        Self { topic: topic.into(), message_type: None, mode: SubscriptionMode::Competing }
    }

    /// 只投递这个 CloudEvents `type` 的消息。
    pub fn with_message_type(mut self, message_type: impl Into<String>) -> Self {
        self.message_type = Some(message_type.into());
        self
    }

    /// 设置订阅方式。
    pub fn with_mode(mut self, mode: SubscriptionMode) -> Self {
        self.mode = mode;
        self
    }

    /// 判断一次投递是否交给这个订阅（`invocation.md` §4）：主题与方式都相同，类型为空或等于信封的 `type`。
    pub(crate) fn matches(&self, envelope: &Envelope, mode: SubscriptionMode) -> bool {
        self.topic == envelope.topic && self.mode == mode && self.message_type.as_deref().is_none_or(|message_type| message_type == envelope.event_type)
    }
}

/// 判断主题是否符合 RFC-0009 的文法 `^[a-z0-9][a-z0-9._-]{0,127}$`。
pub(crate) fn is_topic(topic: &str) -> bool {
    let bytes = topic.as_bytes();
    (1..=128).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|&byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-'))
}

/// CloudEvents 1.0 信封的平铺（`plugin.proto` 的 `Envelope`，RFC-0009）。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Envelope {
    /// 主题。
    pub topic: String,
    /// `id`。
    pub id: String,
    /// `source`。
    pub source: String,
    /// `type`。
    pub event_type: String,
    /// `specversion`；固定 `1.0`。
    pub spec_version: String,
    /// `subject`。
    pub subject: Option<String>,
    /// `time`（RFC 3339）。
    pub time: Option<String>,
    /// `datacontenttype`；缺省 `application/json`。
    pub data_content_type: String,
    /// `data`。
    pub data: Vec<u8>,
    /// 扩展属性，如携带租户的 `xihantenant`（[`TENANT_EXTENSION`](super::TENANT_EXTENSION)）。
    pub extensions: BTreeMap<String, String>,
}

impl Envelope {
    /// 用主题与 CloudEvents 的三个必填属性创建：`specversion` 为 `1.0`，`datacontenttype` 为 `application/json`，没有数据。
    pub fn new(topic: impl Into<String>, id: impl Into<String>, source: impl Into<String>, event_type: impl Into<String>) -> Self {
        Self {
            topic: topic.into(),
            id: id.into(),
            source: source.into(),
            event_type: event_type.into(),
            spec_version: "1.0".to_owned(),
            subject: None,
            time: None,
            data_content_type: "application/json".to_owned(),
            data: Vec::new(),
            extensions: BTreeMap::new(),
        }
    }

    /// 设置数据；`datacontenttype` 不变。
    pub fn with_data(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.data = data.into();
        self
    }

    /// 设置 `subject`。
    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// 加一个扩展属性。
    pub fn with_extension(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extensions.insert(name.into(), value.into());
        self
    }
}

/// 一次投递：信封、投递次数（从 1 起）与这次投递的订阅方式（`plugin.proto` 的 `HandleEventRequest`）。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Event {
    /// 信封。
    pub envelope: Envelope,
    /// 投递次数，从 1 起。
    pub attempt: u32,
    /// 这次投递的订阅方式。
    pub mode: SubscriptionMode,
}

/// 处理一条投递的消息（`invocation.md` §4）：返回 `Ok` 表示已处理，`Err` 由宿主的消息代理按重试与死信策略处理。
///
/// 投递是至少一次的：处理器必须幂等，可用信封的 `id` 或 Baggage 里的幂等键去重。
pub trait EventHandler: Send + Sync + 'static {
    /// 处理一条消息。
    fn handle<'a>(&'a self, ctx: &'a Context, event: Event) -> BoxFuture<'a, Result<()>>;
}

impl<H: EventHandler + ?Sized> EventHandler for Arc<H> {
    fn handle<'a>(&'a self, ctx: &'a Context, event: Event) -> BoxFuture<'a, Result<()>> {
        (**self).handle(ctx, event)
    }
}

/// 由函数实现的 [`EventHandler`]，见 [`event_handler_fn`]。
pub struct EventHandlerFn<F> {
    function: F,
    // 只为让 F 的约束写在类型上，不影响 Send、Sync
    marker: PhantomData<fn()>,
}

/// 用函数创建事件处理器：`event_handler_fn(|ctx, event| Box::pin(async move { … }))`。
pub fn event_handler_fn<F>(function: F) -> EventHandlerFn<F>
where
    F: for<'a> Fn(&'a Context, Event) -> BoxFuture<'a, Result<()>> + Send + Sync + 'static,
{
    EventHandlerFn { function, marker: PhantomData }
}

impl<F> EventHandler for EventHandlerFn<F>
where
    F: for<'a> Fn(&'a Context, Event) -> BoxFuture<'a, Result<()>> + Send + Sync + 'static,
{
    fn handle<'a>(&'a self, ctx: &'a Context, event: Event) -> BoxFuture<'a, Result<()>> {
        (self.function)(ctx, event)
    }
}

impl<F> fmt::Debug for EventHandlerFn<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("EventHandlerFn").finish_non_exhaustive()
    }
}

/// 健康项状态（`plugin.proto` 的 `HealthStatus`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HealthStatus {
    /// 健康。
    Healthy,
    /// 降级：宿主据此把插件记为 `Degraded`（`lifecycle.md` §1）。
    Degraded,
    /// 不健康：同样让宿主把插件记为 `Degraded`。
    Unhealthy,
}

impl HealthStatus {
    /// 协议里的数值。
    pub(crate) const fn value(self) -> i32 {
        match self {
            Self::Healthy => 1,
            Self::Degraded => 2,
            Self::Unhealthy => 3,
        }
    }
}

/// 一个健康项；名字必须在清单 `contributes.health` 里声明（`lifecycle.md` §4）。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HealthItem {
    /// 健康项名。
    pub name: String,
    /// 状态。
    pub status: HealthStatus,
    /// 说明。
    pub description: Option<String>,
}

impl HealthItem {
    /// 用名字与状态创建，没有说明。
    pub fn new(name: impl Into<String>, status: HealthStatus) -> Self {
        Self { name: name.into(), status, description: None }
    }

    /// 设置说明。
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// 插件自报的健康（`plugin.proto` 的 `HealthResponse`）。
///
/// [`Hooks::health`] 报的状态只认 `Ready`、`Degraded` 与 `Failed`（`lifecycle.md` §1 插件可自报的状态里，`Started` 与 `Draining`
/// 由运行时自己迁移），并且只在运行中（`Ready` 或 `Degraded`）按合法迁移生效；健康项只保留清单声明过的名字。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Health {
    /// 生命周期状态。
    pub state: LifecycleState,
    /// 健康项。
    pub items: Vec<HealthItem>,
    /// `Degraded` / `Failed` 时的原因。
    pub error: Option<Error>,
}

impl Health {
    /// 用状态创建，没有健康项与原因。
    pub fn new(state: LifecycleState) -> Self {
        Self { state, items: Vec::new(), error: None }
    }

    /// 加一个健康项。
    pub fn with_item(mut self, item: HealthItem) -> Self {
        self.items.push(item);
        self
    }

    /// 设置原因。
    pub fn with_error(mut self, error: Error) -> Self {
        self.error = Some(error);
        self
    }
}

/// 一个已解析的资源绑定（`plugin.proto` 的 `Binding`）：只含清单声明的绑定。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Binding {
    /// 绑定名。
    pub name: String,
    /// 资源类型（`connection:relational`、`cache:distributed`、`queue`、`blob`）。
    pub resource_type: String,
    /// 连接串或地址。
    pub value: String,
}

/// `Configure` 下发的内容（`lifecycle.md` §2）。
#[derive(Clone)]
#[non_exhaustive]
pub struct Configuration {
    /// 清单 `contributes.config.section` 节的 JSON 对象文本；没有声明配置节时为 `{}`。配置源经宿主展开后再序列化
    /// （`lifecycle.md` §2），规范没有规定叶子的类型，.NET 参考实现把标量都写成字符串：数字与布尔成员先按字符串读。
    pub section: String,
    /// 宿主授予的权限，等于清单 `permissions`。
    pub granted_permissions: Vec<String>,
    /// 只含清单声明的绑定。
    pub bindings: Vec<Binding>,
    /// 宿主为插件创建的可写工作目录（宿主停止时不删除）。
    pub work_directory: PathBuf,
    pub(crate) host: Option<Arc<Host>>,
}

impl Configuration {
    /// 按共同的 JSON 读取检查把配置节读成 `T`；失败时 [`ReadError::path`] 是出错的位置。
    pub fn decode<T: DeserializeOwned>(&self) -> std::result::Result<T, ReadError> {
        json::from_str(&self.section)
    }

    /// 宿主回调的客户端：插件在调用之外（后台任务）回调宿主时用它；宿主没有下发回调端点时为 `None`。
    pub fn host(&self) -> Option<Arc<Host>> {
        self.host.clone()
    }
}

impl fmt::Debug for Configuration {
    // 绑定的值常是连接串，不写进调试输出
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Configuration")
            .field("granted_permissions", &self.granted_permissions)
            .field("bindings", &self.bindings.iter().map(|binding| &binding.name).collect::<Vec<_>>())
            .field("work_directory", &self.work_directory)
            .finish_non_exhaustive()
    }
}

/// 插件作者接入生命周期的钩子（`lifecycle.md` §2、§4、§5）；方法都有什么都不做的缺省实现。
///
/// 返回的 `Err` 原样放进应答的 `Status.error`；钩子 panic 时是 `PLUGINS_UNEXPECTED_001` + `plugins.process.failed`。
/// `configure` 或 `start` 失败即插件进入 `Failed`。
pub trait Hooks: Send + Sync + 'static {
    /// `Configure` 时调用：读配置节、记下绑定与工作目录。
    fn configure<'a>(&'a self, configuration: &'a Configuration) -> BoxFuture<'a, Result<()>> {
        let _ = configuration;
        Box::pin(async { Ok(()) })
    }

    /// `Start` 时调用；返回 `Ok` 后插件就绪、接受调用。
    fn start(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// 停止时调用：在途调用已经完成（或宽限已过），在这里释放资源；`reason` 是 `shutdown` / `unload` / `restart`，
    /// stdin 关闭或收到 SIGTERM 时是 `exit`；`grace` 是剩下的宽限。
    fn stop<'a>(&'a self, reason: &'a str, grace: Duration) -> BoxFuture<'a, Result<()>> {
        let _ = (reason, grace);
        Box::pin(async { Ok(()) })
    }

    /// 当前健康；`None` 表示按运行时自己的状态上报、没有健康项。`Health` 与 `WatchHealth` 调用它，`WatchHealth` 每秒重新取一次，
    /// 变化时推送，所以它要快、不阻塞。
    fn health(&self) -> Option<Health> {
        None
    }
}

/// 不接入任何钩子。
pub(crate) struct NoHooks;

impl Hooks for NoHooks {}
