//! 调用上下文：Go SDK `invocation` 测试的移植，以及截止时间与取消的归因（RFC-0003 规则 1、RFC-0004）。

mod common;

use std::thread;
use std::time::{Duration, Instant, SystemTime};

use xihan_framework::invocation::{
    CancellationToken, Context, ERROR_CODE_CANCELLED, ERROR_CODE_DEADLINE_EXCEEDED, MESSAGE_CANCELLED, MESSAGE_DEADLINE_BEFORE_START,
    MESSAGE_DEADLINE_EXCEEDED, TraceContext,
};
use xihan_framework::primitives::{BAGGAGE_CORRELATION, BAGGAGE_LOCALE, BAGGAGE_TENANT, ERROR_CODE_INVALID_ARGUMENT};

/// 对象袋里按类型存取，测试用的新类型。
#[derive(Debug, PartialEq)]
struct Attempt(u32);

/// 轮询直到条件成立；定时器在后台线程上触发，给它充足的余量。
fn eventually(condition: impl Fn() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

fn code(ctx: &Context) -> Option<String> {
    ctx.cancellation().map(|error| error.code().to_owned())
}

#[test]
fn new_generates_trace_and_keeps_baggage() {
    let ctx = Context::builder().baggage(BAGGAGE_TENANT, "t1").build();
    assert_eq!((ctx.trace_id().len(), ctx.span_id().len()), (32, 16), "trace {} span {}", ctx.trace_id(), ctx.span_id());
    assert_eq!((ctx.tenant(), ctx.principal()), (Some("t1"), None));

    let derived = ctx.with_baggage(BAGGAGE_CORRELATION, "c1").with_baggage("xihan.feature-flag", "on");
    assert_eq!((derived.correlation(), ctx.correlation()), (Some("c1"), None), "derived baggage must not leak back");
    assert_eq!(derived.trace_id(), ctx.trace_id());
    let stripped = derived.without_unregistered_baggage();
    assert!(!stripped.baggage().contains_key("xihan.feature-flag"), "unregistered keys are stripped");
    assert_eq!(stripped.baggage().keys().collect::<Vec<_>>(), [BAGGAGE_CORRELATION, BAGGAGE_TENANT]);

    assert_eq!(derived.insert_item(Attempt(1)), None);
    assert_eq!(ctx.item::<Attempt>().as_deref(), Some(&Attempt(1)), "items are shared between derived contexts");
    assert_eq!(stripped.insert_item(Attempt(2)).as_deref(), Some(&Attempt(1)));
    assert_eq!(ctx.remove_item::<Attempt>().as_deref(), Some(&Attempt(2)));
    assert_eq!(derived.item::<Attempt>(), None);

    let parsed = TraceContext::parse(&ctx.trace_parent()).expect("the generated traceparent parses");
    assert_eq!((parsed.trace_id(), parsed.span_id(), parsed.is_sampled()), (ctx.trace_id(), ctx.span_id(), true));
    assert_ne!(Context::background().trace_id(), ctx.trace_id(), "every context starts a new trace");
}

#[test]
fn deadline_cancels_and_translates() {
    let ctx = Context::builder().timeout(Duration::from_millis(20)).build();
    assert!(ctx.has_deadline());
    let remaining = ctx.remaining_time(SystemTime::now()).unwrap();
    assert!(remaining <= Duration::from_millis(20), "remaining {remaining:?}");
    assert_eq!(code(&ctx), None);
    common::block_on(ctx.done());
    let error = ctx.cancellation().unwrap();
    assert_eq!((error.code(), error.message_key(), error.kind()), (ERROR_CODE_DEADLINE_EXCEEDED, Some(MESSAGE_DEADLINE_EXCEEDED), "TIMEOUT"));
    assert!(ctx.is_cancelled() && ctx.cancellation_token().is_cancelled());
    assert!(ctx.is_expired(SystemTime::now()));
    assert_eq!(ctx.remaining_time(SystemTime::now()), Some(Duration::ZERO));

    let caller = CancellationToken::new();
    let cancelled = Context::builder().cancellation(&caller).build();
    caller.cancel();
    common::block_on(cancelled.done());
    let error = cancelled.cancellation().unwrap();
    assert_eq!((error.code(), error.message_key(), error.kind()), (ERROR_CODE_CANCELLED, Some(MESSAGE_CANCELLED), "CANCELLED"));
    assert_eq!(code(&Context::background()), None, "no error without cancellation");
    assert_eq!(Context::background().deadline(), None);
}

/// 归因只看截止令牌：截止令牌已触发即超时，不论调用方是否也已取消；截止令牌没有触发时调用方的取消是取消。
#[test]
fn attribution_follows_the_deadline_token() {
    let caller = CancellationToken::new();
    let ctx = Context::builder().cancellation(&caller).timeout(Duration::from_millis(30)).build();
    caller.cancel();
    assert_eq!(code(&ctx).as_deref(), Some(ERROR_CODE_CANCELLED), "cancelled before the deadline");
    assert!(eventually(|| code(&ctx).as_deref() == Some(ERROR_CODE_DEADLINE_EXCEEDED)), "once the deadline token fires it is a timeout");

    let caller = CancellationToken::new();
    let ctx = Context::builder().cancellation(&caller).deadline(SystemTime::now() - Duration::from_secs(1)).build();
    assert_eq!(code(&ctx).as_deref(), Some(ERROR_CODE_DEADLINE_EXCEEDED), "a deadline in the past fires at once");
    caller.cancel();
    assert_eq!(code(&ctx).as_deref(), Some(ERROR_CODE_DEADLINE_EXCEEDED), "a later caller cancellation does not change it");

    // 派生上下文共享令牌与截止时间
    let ctx = Context::builder().timeout(Duration::from_millis(10)).build();
    let derived = ctx.with_baggage(BAGGAGE_LOCALE, "zh-CN");
    common::block_on(derived.done());
    assert_eq!(code(&ctx).as_deref(), Some(ERROR_CODE_DEADLINE_EXCEEDED));
}

#[test]
fn unrepresentable_deadlines_never_fire() {
    let ctx = Context::builder().timeout(Duration::MAX).build();
    assert!(!ctx.has_deadline(), "a timeout beyond SystemTime means no deadline");
    let far = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(u64::MAX / 4)).or_else(|| SystemTime::now().checked_add(Duration::from_secs(1 << 40)));
    if let Some(far) = far {
        let ctx = Context::builder().deadline(far).build();
        assert!(ctx.has_deadline() && !ctx.is_cancelled());
        assert_eq!(code(&ctx), None);
    }
}

#[test]
fn waiting_is_runtime_independent_and_wakes_other_threads() {
    let caller = CancellationToken::new();
    let ctx = Context::builder().cancellation(&caller).build();
    let waiting = ctx.done();
    let waiter = thread::spawn(move || common::block_on(waiting));
    thread::sleep(Duration::from_millis(20));
    caller.cancel();
    waiter.join().unwrap();
    let child = caller.child_token();
    assert!(child.is_cancelled());
    common::block_on(child.cancelled());
}

#[test]
fn trace_parent_parsing() {
    let parsed = TraceContext::parse("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01").expect("valid traceparent");
    assert!(parsed.is_sampled());
    assert!(TraceContext::parse("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-03").unwrap().is_sampled(), "sampling is the lowest flag bit");
    for bad in [
        "",
        "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331",
        "00-00000000000000000000000000000000-b7ad6b7169203331-01",
        "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01",
        "00-0AF7651916CD43DD8448EB211C80319C-b7ad6b7169203331-01",
        "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01-extra",
        "0-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
        "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-zz",
    ] {
        assert!(TraceContext::parse(bad).is_none(), "{bad:?} must not parse");
    }
    let ctx = Context::builder().trace_parent("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-00").build();
    assert_eq!(ctx.trace_id(), "0af7651916cd43dd8448eb211c80319c");
    assert_eq!(ctx.trace_parent(), "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-00");
    let ignored = Context::builder().trace_parent("garbage").build();
    assert_eq!(ignored.trace_id().len(), 32, "an invalid traceparent starts a new trace");
    assert!(TraceContext::new("0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331", false).is_some());
    assert!(TraceContext::new("0af7651916cd43dd8448eb211c80319", "b7ad6b7169203331", false).is_none());
}

#[test]
fn malformed_baggage_keys_panic() {
    let message = common::panic_message(|| drop(Context::background().with_baggage("tenant", "t1")));
    assert!(message.starts_with(ERROR_CODE_INVALID_ARGUMENT) && message.contains("'tenant'"), "{message}");
    let message = common::panic_message(|| drop(Context::builder().baggage("xihan.Tenant", "t1")));
    assert!(message.starts_with(ERROR_CODE_INVALID_ARGUMENT), "{message}");
}

#[test]
fn contexts_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<Context>();
    assert_send_sync::<CancellationToken>();
    assert_send_sync::<xihan_framework::invocation::WaitForCancellation>();
}

/// 截止时间与取消的错误按 kernel-messages.json：码、键、没有参数、en-US 文案。
#[test]
fn cancellation_errors_follow_kernel_messages() {
    let ctx = Context::builder().timeout(Duration::from_millis(5)).build();
    common::block_on(ctx.done());
    common::assert_kernel_error(&ctx.cancellation().unwrap(), MESSAGE_DEADLINE_EXCEEDED);

    let late = Context::builder().deadline(SystemTime::now() - Duration::from_millis(1)).build();
    common::assert_kernel_error(&late.cancellation().unwrap(), MESSAGE_DEADLINE_BEFORE_START);
    assert_eq!(
        late.with_baggage(BAGGAGE_LOCALE, "en-US").cancellation().unwrap().message_key(),
        Some(MESSAGE_DEADLINE_BEFORE_START),
        "derived contexts share it"
    );

    let caller = CancellationToken::new();
    let cancelled = Context::builder().cancellation(&caller).build();
    caller.cancel();
    common::assert_kernel_error(&cancelled.cancellation().unwrap(), MESSAGE_CANCELLED);
}
