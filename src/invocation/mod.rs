//! 调用上下文（RFC-0003）：取消、截止时间、W3C 追踪、已登记的 Baggage 与请求作用域对象袋。
//!
//! 不绑定任何协议，载体映射由协议适配器与插件运行时实现。取消令牌与截止时间是本模块自带的轻量类型，
//! 不依赖任何异步运行时：[`Context::done`] 返回的 future 由任何执行器驱动，截止时间由进程里一个后台线程按时触发。
//!
//! ```
//! use std::time::Duration;
//! use xihan_framework::invocation::Context;
//! use xihan_framework::primitives::BAGGAGE_TENANT;
//!
//! let ctx = Context::builder().timeout(Duration::from_secs(2)).baggage(BAGGAGE_TENANT, "t1").build();
//! assert_eq!(ctx.tenant(), Some("t1"));
//! assert!(ctx.has_deadline() && ctx.cancellation().is_none());
//! ```

mod cancellation;
mod deadline;
mod trace;

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

pub use cancellation::{CancellationToken, WaitForCancellation};
use deadline::Deadline;
pub use trace::{TraceContext, new_span_id, new_trace_id};

use crate::primitives::{
    BAGGAGE_CORRELATION, BAGGAGE_IDEMPOTENCY_KEY, BAGGAGE_LOCALE, BAGGAGE_MAX_VALUE_LENGTH, BAGGAGE_PRINCIPAL, BAGGAGE_TENANT, Error,
    is_baggage_key_registered, is_baggage_key_well_formed, is_baggage_value_acceptable,
};

/// 调用超过截止时间。
pub const ERROR_CODE_DEADLINE_EXCEEDED: &str = "KERNEL_TIMEOUT_001";
/// 调用在截止时间到达之前被调用方取消。
pub const ERROR_CODE_CANCELLED: &str = "KERNEL_CANCELLED_001";

/// 超时的消息键：截止时间在调用途中到达。
pub const MESSAGE_DEADLINE_EXCEEDED: &str = "kernel.invocation.deadline_exceeded";
/// 超时的消息键：调用开始时截止时间已经过去（调用器在第一个中间件之前按时钟判断）。
pub const MESSAGE_DEADLINE_BEFORE_START: &str = "kernel.invocation.deadline_before_start";
/// 取消的消息键。
pub const MESSAGE_CANCELLED: &str = "kernel.invocation.cancelled";

/// 一次调用的上下文。
///
/// 克隆与派生（[`Context::with_baggage`]）共享取消令牌、截止时间与对象袋，Baggage 按值复制：派生上下文加的键
/// 不会漏回原上下文。上下文是 `Send + Sync` 的，可以在异步任务之间借用。
#[derive(Clone)]
pub struct Context {
    token: CancellationToken,
    deadline: Option<Arc<Deadline>>,
    trace: TraceContext,
    baggage: Arc<BTreeMap<String, String>>,
    items: Arc<Items>,
}

/// 请求作用域对象袋：按类型存取，派生上下文共享，可并发读写，不跨进程传播。
#[derive(Default)]
struct Items {
    values: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl Context {
    /// 开始构建上下文。
    pub fn builder() -> ContextBuilder {
        ContextBuilder::default()
    }

    /// 没有调用方取消、没有截止时间、带新生成追踪的上下文。
    pub fn background() -> Self {
        Self::builder().build()
    }

    /// 绝对截止时间；没有时为 `None`。
    pub fn deadline(&self) -> Option<SystemTime> {
        self.deadline.as_ref().map(|deadline| deadline.at())
    }

    /// 判断是否有截止时间。
    pub fn has_deadline(&self) -> bool {
        self.deadline.is_some()
    }

    /// 判断在 `now` 时是否已过截止时间；没有截止时间时总是假。
    ///
    /// 只用于展示与日志：超时与取消的归因看 [`Context::cancellation`]，它只认截止令牌，不另看时钟。
    pub fn is_expired(&self, now: SystemTime) -> bool {
        self.deadline().is_some_and(|deadline| now >= deadline)
    }

    /// 在 `now` 时到截止时间的剩余时间；没有截止时间时为 `None`，已过期为零。
    pub fn remaining_time(&self, now: SystemTime) -> Option<Duration> {
        self.deadline().map(|deadline| deadline.duration_since(now).unwrap_or(Duration::ZERO))
    }

    /// 追踪。
    pub fn trace(&self) -> &TraceContext {
        &self.trace
    }

    /// 追踪 ID：32 位小写十六进制。
    pub fn trace_id(&self) -> &str {
        self.trace.trace_id()
    }

    /// Span ID：16 位小写十六进制。
    pub fn span_id(&self) -> &str {
        self.trace.span_id()
    }

    /// W3C `traceparent`。
    pub fn trace_parent(&self) -> String {
        self.trace.trace_parent()
    }

    /// 全部 Baggage，按键排序。
    pub fn baggage(&self) -> &BTreeMap<String, String> {
        &self.baggage
    }

    /// 派生一个多一个（或改写一个）Baggage 键值的上下文：程序内逐项派生（`conventions/baggage-keys.md`，ADR-0235）。
    ///
    /// # Panics
    ///
    /// 键不符合 `xihan.<name>` 时 panic；已登记的键（幂等键除外）给了 [`is_baggage_value_acceptable`] 不接受的值时同样 panic。
    /// 两者都是编程错误，不静默丢弃、也不保留原值。来自外部的 Baggage 交给 [`ContextBuilder::baggage`]，它按外部输入丢弃不合的成员。
    #[track_caller]
    pub fn with_baggage(&self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let (key, value) = (key.into(), value.into());
        must_be_baggage_key(&key);
        if !keeps(&key, &value) {
            crate::primitives::invalid_argument(format!(
                "The value of the baggage key '{key}' must be non-empty, at most {BAGGAGE_MAX_VALUE_LENGTH} UTF-16 code units and free of control characters."
            ));
        }
        let mut baggage = BTreeMap::clone(&self.baggage);
        baggage.insert(key, value);
        Self { baggage: Arc::new(baggage), ..self.clone() }
    }

    /// 派生一个只保留已登记键的上下文：进程外的载体只写已登记的键（RFC-0003 规则 3）。
    pub fn without_unregistered_baggage(&self) -> Self {
        let baggage = self.baggage.iter().filter(|(key, _)| is_baggage_key_registered(key)).map(|(key, value)| (key.clone(), value.clone())).collect();
        Self { baggage: Arc::new(baggage), ..self.clone() }
    }

    /// 当前租户 ID。
    pub fn tenant(&self) -> Option<&str> {
        self.baggage_value(BAGGAGE_TENANT)
    }

    /// 当前主体 ID。
    pub fn principal(&self) -> Option<&str> {
        self.baggage_value(BAGGAGE_PRINCIPAL)
    }

    /// 业务关联 ID。
    pub fn correlation(&self) -> Option<&str> {
        self.baggage_value(BAGGAGE_CORRELATION)
    }

    /// 幂等键。
    pub fn idempotency_key(&self) -> Option<&str> {
        self.baggage_value(BAGGAGE_IDEMPOTENCY_KEY)
    }

    /// 请求文化（BCP 47）。
    pub fn locale(&self) -> Option<&str> {
        self.baggage_value(BAGGAGE_LOCALE)
    }

    fn baggage_value(&self, key: &str) -> Option<&str> {
        self.baggage.get(key).map(String::as_str)
    }

    /// 在对象袋里放一个值，以它的类型为键，返回同类型原来的值。
    ///
    /// 派生上下文共享对象袋；同一类型只有一个值，需要多个时用各自的新类型包装。
    pub fn insert_item<T: Any + Send + Sync>(&self, value: T) -> Option<Arc<T>> {
        self.items.write().insert(TypeId::of::<T>(), Arc::new(value)).and_then(downcast)
    }

    /// 从对象袋里取某个类型的值。
    pub fn item<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.items.read().get(&TypeId::of::<T>()).cloned().and_then(downcast)
    }

    /// 从对象袋里取走某个类型的值。
    pub fn remove_item<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.items.write().remove(&TypeId::of::<T>()).and_then(downcast)
    }

    /// 上下文的取消令牌：调用方取消（[`ContextBuilder::cancellation`]）或到达截止时间时触发。
    ///
    /// 传给需要令牌的 API；判断是超时还是取消用 [`Context::cancellation`]。
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.token
    }

    /// 判断上下文是否已取消（调用方取消或到达截止时间）。
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// 等待上下文取消：调用方取消或到达截止时间时完成。
    pub fn done(&self) -> WaitForCancellation {
        self.token.cancelled()
    }

    /// 把取消与超时翻译成原语层错误：截止令牌已触发为 `KERNEL_TIMEOUT_001`，否则调用方已取消为
    /// `KERNEL_CANCELLED_001`；都没有为 `None`。错误都没有参数（RFC-0011「错误码」）。
    ///
    /// 归因只看令牌（RFC-0003 规则 1、RFC-0004）：截止令牌已触发即超时，不论调用方是否也已取消；截止令牌没有触发时，
    /// 即使时钟已过截止时间，调用方的取消仍是取消。超时的消息键是 `kernel.invocation.deadline_exceeded`；调用开始时截止时间
    /// 已经过去的 `kernel.invocation.deadline_before_start` 由调用器在第一个中间件之前判断（RFC-0011 规则 10），不在这里。
    pub fn cancellation(&self) -> Option<Error> {
        if self.deadline_fired() {
            return Some(Error::timeout(ERROR_CODE_DEADLINE_EXCEEDED, MESSAGE_DEADLINE_EXCEEDED));
        }
        self.token.is_cancelled().then(|| Error::cancelled(ERROR_CODE_CANCELLED, MESSAGE_CANCELLED))
    }

    /// 截止令牌是否已触发：只看定时器，不读时钟；没有截止时间时为假。
    pub(crate) fn deadline_fired(&self) -> bool {
        self.deadline.as_ref().is_some_and(|deadline| deadline.has_fired())
    }

    /// 测试用：让截止令牌立即触发，截止时间不变，造出「令牌已触发、墙上时钟还没到截止时间」的先后。
    #[cfg(test)]
    pub(crate) fn fire_deadline_now(&self) {
        if let Some(deadline) = &self.deadline {
            deadline.fire_now(&self.token);
        }
    }
}

impl fmt::Debug for Context {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Context")
            .field("trace", &self.trace)
            .field("deadline", &self.deadline())
            .field("baggage", &self.baggage)
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl Items {
    // 锁里只有对表的整体增删，panic 不会留下半改的状态，所以中毒时照常使用

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<TypeId, Arc<dyn Any + Send + Sync>>> {
        self.values.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<TypeId, Arc<dyn Any + Send + Sync>>> {
        self.values.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 按类型取出：表以 `TypeId::of::<T>()` 为键，取出的值一定是 `T`。
fn downcast<T: Any + Send + Sync>(value: Arc<dyn Any + Send + Sync>) -> Option<Arc<T>> {
    value.downcast().ok()
}

/// 判断一个 Baggage 成员能否进入上下文：未登记的键不看值；幂等键不在这里丢弃，丢了它重复的调用会执行两次，
/// 不合法的幂等键由 Idempotency 阶段以校验错误拒绝
fn keeps(key: &str, value: &str) -> bool {
    !is_baggage_key_registered(key) || key == BAGGAGE_IDEMPOTENCY_KEY || is_baggage_value_acceptable(value)
}

/// 校验 Baggage 键，不合法即 panic（编程错误）。
#[track_caller]
fn must_be_baggage_key(key: &str) {
    if !is_baggage_key_well_formed(key) {
        crate::primitives::invalid_argument(format!("The baggage key '{key}' must match xihan.<name>."));
    }
}

/// 上下文的构建器，由 [`Context::builder`] 创建。
///
/// 同一项设置多次时以最后一次为准；截止时间以 [`ContextBuilder::deadline`] 与 [`ContextBuilder::timeout`] 中后调用的为准。
#[derive(Debug, Default)]
#[must_use = "a builder does nothing until it is built"]
pub struct ContextBuilder {
    cancellation: Option<CancellationToken>,
    deadline: Option<SystemTime>,
    trace: Option<TraceContext>,
    baggage: BTreeMap<String, String>,
}

impl ContextBuilder {
    /// 调用方的取消令牌：它取消时上下文随之取消（上下文令牌是它的子令牌），报 `KERNEL_CANCELLED_001`。
    pub fn cancellation(mut self, token: &CancellationToken) -> Self {
        self.cancellation = Some(token.clone());
        self
    }

    /// 绝对截止时间：到达时上下文取消，报 `KERNEL_TIMEOUT_001`；已经过去的在构建时立即触发。
    pub fn deadline(mut self, at: SystemTime) -> Self {
        self.deadline = Some(at);
        self
    }

    /// 以相对时间设置截止时间；超出 `SystemTime` 能表示的范围时等同于没有截止时间。
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.deadline = SystemTime::now().checked_add(timeout);
        self
    }

    /// 设置追踪；没有设置时构建时生成新的追踪。
    pub fn trace(mut self, trace: TraceContext) -> Self {
        self.trace = Some(trace);
        self
    }

    /// 从 W3C `traceparent` 设置追踪；不合法时忽略，与 W3C 对不合法头的处理一致（开始新的追踪）。
    pub fn trace_parent(mut self, trace_parent: &str) -> Self {
        if let Some(trace) = TraceContext::parse(trace_parent) {
            self.trace = Some(trace);
        }
        self
    }

    /// 加入一个 Baggage 成员：这是由一组 Baggage 构建上下文的入口，按外部输入处理（`conventions/baggage-keys.md`，ADR-0235）。
    ///
    /// 已登记键（幂等键除外）的值不合 [`is_baggage_value_acceptable`]（非空、按 UTF-16 码元计最多 256、不含 U+0000–U+001F
    /// 与 U+007F–U+009F）时丢弃这个成员，同名的原值保留，与 W3C 忽略不合法的成员一致；合格的值改写同名的原值。
    ///
    /// # Panics
    ///
    /// 键不符合 `xihan.<name>` 时 panic（编程错误）；来自载体的键先用 [`is_baggage_key_well_formed`] 判断。
    #[track_caller]
    pub fn baggage(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let (key, value) = (key.into(), value.into());
        must_be_baggage_key(&key);
        if keeps(&key, &value) {
            self.baggage.insert(key, value);
        }
        self
    }

    /// 构建上下文：有截止时间时登记定时器，没有追踪时生成新的追踪 ID 与 Span ID。
    ///
    /// # Panics
    ///
    /// 第一次登记截止时间而定时器线程无法启动，或需要生成追踪而操作系统的随机数源不可用时 panic。
    pub fn build(self) -> Context {
        let token = self.cancellation.as_ref().map_or_else(CancellationToken::new, CancellationToken::child_token);
        // 截止时间与取消并存：到达截止时间时上下文令牌触发（RFC-0003 规则 1）
        let deadline = self.deadline.map(|at| Deadline::start(at, &token));
        Context {
            token,
            deadline,
            trace: self.trace.unwrap_or_else(TraceContext::generate),
            baggage: Arc::new(self.baggage),
            items: Arc::new(Items::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 截止令牌由定时器按单调时钟触发，墙上时钟可能还没到截止时间：归因只看令牌，调用方随后取消也仍是超时。
    #[test]
    fn a_fired_deadline_token_is_a_timeout_before_the_clock_reaches_the_deadline() {
        let caller = CancellationToken::new();
        let ctx = Context::builder().cancellation(&caller).timeout(Duration::from_secs(60)).build();
        assert!(!ctx.deadline_fired() && ctx.cancellation().is_none());
        ctx.fire_deadline_now();
        assert!(ctx.deadline_fired() && ctx.is_cancelled() && !ctx.is_expired(SystemTime::now()));
        caller.cancel();
        assert_eq!(ctx.cancellation().as_ref().map(Error::code), Some(ERROR_CODE_DEADLINE_EXCEEDED));
        assert!(!Context::background().deadline_fired(), "a context without a deadline has no deadline token");
    }
}
