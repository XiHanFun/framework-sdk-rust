//! 插件作者接触的类型：订阅与事件、健康、配置与生命周期钩子。

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::host::Host;
use crate::BoxFuture;
use crate::invocation::Context;
use crate::primitives::json::{self, ReadError};
use crate::primitives::{Error, LifecycleState, Result, named_enum};

named_enum! {
    /// 订阅方式（RFC-0009「订阅方式」）：宿主按它向消息代理订阅主题，投递时在 `HandleEventRequest.mode` 里带上（`invocation.md` §4）。
    ///
    /// 名字是协议里的 `competing` 与 `broadcast`；读入（`parse`、`FromStr`、serde）时协议里的空同 `competing`，写出总是名字。
    /// 协议以后可以增加订阅方式，所以是 `#[non_exhaustive]`：本 crate 之外的 `match` 要带通配分支。
    #[derive(Default)]
    #[non_exhaustive]
    pub enum SubscriptionMode ("subscription mode") {
        /// `competing`：订阅了主题的宿主实例里只有一个把消息交给插件；协议里的空同它。
        #[default]
        Competing = 0 => "competing" | "",
        /// `broadcast`：每个在线的宿主实例都把消息交给自己的插件实例，尽力而为、不进死信。
        Broadcast = 1 => "broadcast",
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
    #[must_use]
    pub fn with_message_type(mut self, message_type: impl Into<String>) -> Self {
        self.message_type = Some(message_type.into());
        self
    }

    /// 设置订阅方式。
    #[must_use]
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
    /// `dataschema`（数据遵循的模式，URI）；`None` 与空串都是没有（`invocation.md` §4，ADR-0305）。
    ///
    /// 原样写给宿主、原样交给处理器：SDK 不裁剪、不核对 URI 语法，只有空白的值由宿主拒绝那一项。
    pub data_schema: Option<String>,
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
            data_schema: None,
            data: Vec::new(),
            extensions: BTreeMap::new(),
        }
    }

    /// 设置数据；`datacontenttype` 不变。
    #[must_use]
    pub fn with_data(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.data = data.into();
        self
    }

    /// 把 `data` 按 [`json::to_vec`] 写成数据（类型化的写法）；`datacontenttype` 不变，缺省就是 `application/json`。
    ///
    /// 写不出（非有限的浮点、序列化不支持的值）时返回 `serde_json` 的错误，信封随之丢弃。
    pub fn with_json_data<T: Serialize + ?Sized>(mut self, data: &T) -> std::result::Result<Self, serde_json::Error> {
        self.data = json::to_vec(data)?;
        Ok(self)
    }

    /// 把数据按 JSON 读成 `T`（同 [`json::from_slice`]：先过共同的 JSON 读取检查，数字按值模型），不看 `datacontenttype`。
    ///
    /// 没有数据或数据不是 JSON 为 [`ReadError::Syntax`]，有重复成员为 [`ReadError::Duplicate`]，与 `T` 不符时 [`ReadError::path`]
    /// 是出错的位置。
    pub fn data_as<T: DeserializeOwned>(&self) -> std::result::Result<T, ReadError> {
        json::from_slice(&self.data)
    }

    /// 设置 `subject`。
    #[must_use]
    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// 设置 `dataschema`，原样写给宿主。
    #[must_use]
    pub fn with_data_schema(mut self, data_schema: impl Into<String>) -> Self {
        self.data_schema = Some(data_schema.into());
        self
    }

    /// 加一个扩展属性。
    #[must_use]
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

/// 健康项状态（`plugin.proto` 的 `HealthStatus`）；协议里是数值，没有按名字的编码。
///
/// 协议以后可以增加状态，所以是 `#[non_exhaustive]`：本 crate 之外的 `match` 要带通配分支。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
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

/// 一个已解析的资源绑定（`plugin.proto` 的 `Binding`，`lifecycle.md` §2、§7.2）：只含清单声明的绑定。
///
/// 主值与属性值可能是凭据，不要写进日志、错误与其他诊断输出：`Debug` 只带绑定名、类型与属性名，不带主值与属性值。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Binding {
    /// 绑定名：清单里的名字。
    pub name: String,
    /// 资源类型（`connection:relational`、`cache:distributed`、`queue`、`blob`）。
    pub resource_type: String,
    /// 主值：连接串或地址。来源只给了属性、没有给主值时为空，不是错误。
    pub value: String,
    /// 其余属性：属性名到属性值。
    ///
    /// 属性名区分大小写、原样交出（`accessKey` 与 `AccessKey` 是两项）；没有属性时是空表（按较早的规范实现的宿主不发这个字段，读到的
    /// 也是空表），值为空的属性照样在表里，各项的先后没有含义（这里按属性名的字节序）。运行时不核对属性名的文法与上限，那是宿主的事。
    pub properties: BTreeMap<String, String>,
}

impl fmt::Debug for Binding {
    // 主值与属性值可能是凭据：只写绑定名、类型与属性名，省掉的成员以 `..` 标出
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Binding")
            .field("name", &self.name)
            .field("resource_type", &self.resource_type)
            .field("property_names", &self.properties.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// `Configure` 下发的内容（`lifecycle.md` §2、§7.2）。
#[derive(Clone)]
#[non_exhaustive]
pub struct Configuration {
    /// 清单 `contributes.config.section` 节的 JSON 对象文本，原样交给作者；`configuration` 为空时是 `{}`，不是对象的配置节在调用钩子
    /// 之前就以 `plugins.process.failed` 拒绝。宿主把 `IConfiguration` 的叶子一律写成 JSON 字符串（`lifecycle.md` §2），类型化读取用
    /// [`Configuration::decode`]。
    pub section: String,
    /// 宿主授予的权限，等于清单 `permissions`；回调前的权限核对以它为准。
    pub granted_permissions: Vec<String>,
    /// 已解析的绑定连同属性，按宿主下发的先后；只含清单声明的绑定。
    pub bindings: Vec<Binding>,
    /// 宿主为插件创建的可写工作目录（宿主停止时不删除）。
    pub work_directory: PathBuf,
    pub(crate) host: Arc<Host>,
}

impl Configuration {
    /// 把配置节读成 `T`（`lifecycle.md` §7.2 的类型化读取）：成员名按 ASCII 不区分大小写对上字段（与 `IConfiguration` 一致，完全相同的
    /// 优先）；字符串叶子读成数字时按 JSON 数字的文法（`"8080"`、`"-1.5e3"`，不带空白），读成布尔时只认 `true`、`false`（ASCII 不区分
    /// 大小写）；其余照目标类型的常规读法（同 [`json::from_str`]，数字按值模型，对象位置只收对象）。失败时 [`ReadError::path`] 是出错的位置。
    ///
    /// ```
    /// # use serde::Deserialize;
    /// # use xihan_framework::plugin::Configuration;
    /// #[derive(Deserialize)]
    /// struct Settings {
    ///     port: u16,
    ///     verbose: bool,
    ///     name: String,
    /// }
    /// # fn read(configuration: &Configuration) {
    /// // 宿主写来的配置节：{"Port":"8080","VERBOSE":"True","name":"echo"}
    /// let settings: Settings = configuration.decode().unwrap();
    /// # }
    /// ```
    pub fn decode<T: DeserializeOwned>(&self) -> std::result::Result<T, ReadError> {
        json::decode_lenient(&json::parse(self.section.as_bytes())?)
    }

    /// 宿主回调的客户端：插件在调用之外（后台任务）回调宿主时用它。没有可用的回调端点时同样有客户端，回调以
    /// `plugins.host.unavailable` 失败（`handshake.md` §4）。
    pub fn host(&self) -> Arc<Host> {
        Arc::clone(&self.host)
    }
}

impl fmt::Debug for Configuration {
    // 配置节与绑定的主值、属性值可能是凭据，不写进调试输出：配置节省掉，绑定按 Binding 的 Debug 只带绑定名、类型与属性名
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Configuration")
            .field("granted_permissions", &self.granted_permissions)
            .field("bindings", &self.bindings)
            .field("work_directory", &self.work_directory)
            .finish_non_exhaustive()
    }
}

/// 插件作者接入生命周期的钩子（`lifecycle.md` §7）；方法都有什么都不做的缺省实现。
///
/// 返回的 `Err` 原样放进应答的 `Status.error`；钩子 panic 时是 `PLUGINS_UNEXPECTED_001` + `plugins.process.failed`，`phase` 是
/// `configure`、`start`、`stop` 或 `health`，`reason` 是 `panic`（`lifecycle.md` §7.6：Rust 没有异常，panic 的内容可能含路径、
/// 连接串，只写进 stderr 日志）。
pub trait Hooks: Send + Sync + 'static {
    /// `Configure` 时调用：读配置节、记下绑定与工作目录。配置节不是 JSON 对象时不调用；失败或 panic 即插件进入 `Failed`。
    fn configure<'a>(&'a self, configuration: &'a Configuration) -> BoxFuture<'a, Result<()>> {
        let _ = configuration;
        Box::pin(async { Ok(()) })
    }

    /// `Start` 时调用；返回 `Ok` 后插件进入 `Started`，再按 [`Hooks::health`] 进入 `Ready`、`Degraded` 或 `Failed`，然后才应答
    /// （`lifecycle.md` §7.3）。失败或 panic 即插件进入 `Failed`。
    fn start(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// 停止时调用，进程里最多一次，只在 [`Hooks::start`] 成功过之后（`lifecycle.md` §7.5）：在途调用已经结束（或宽限已过、它们的
    /// 取消令牌已触发），在这里释放资源。`reason` 是宿主 `Stop` 的原因（`shutdown` / `unload` / `restart`），stdin 关闭或收到 SIGTERM
    /// 时是 `shutdown`；`grace` 是剩下的宽限。失败与 panic 都只写进 `StopResponse.status`，进程照常以退出码 0 退出。
    fn stop<'a>(&'a self, reason: &'a str, grace: Duration) -> BoxFuture<'a, Result<()>> {
        let _ = (reason, grace);
        Box::pin(async { Ok(()) })
    }

    /// 健康回调（`lifecycle.md` §7.4）；`None` 表示没有健康回调：`Start` 之后直接进入 `Ready`，健康按运行时自己的状态上报、没有健康项。
    ///
    /// 在 `Start` 成功之后求一次，之后只在 `Ready` 与 `Degraded` 期间调用：每次 `Health`，以及 `WatchHealth` 每 10 秒一次。报的
    /// `Ready`、`Degraded` 或 `Failed` 照办（`Failed` 之后不再回来），其他状态不改变状态（`Start` 时按 `Ready`）；健康项名不在清单
    /// `contributes.health` 里的丢掉。panic 与报了 `Degraded` 一样：这一次的结果是 `Degraded`（`plugins.process.failed`，`phase` 为
    /// `health`）、没有健康项，状态迁到 `Degraded`，之后的结果照常决定状态。它要快、不阻塞。
    fn health(&self) -> Option<Health> {
        None
    }
}

/// 不接入任何钩子。
pub(crate) struct NoHooks;

impl Hooks for NoHooks {}
