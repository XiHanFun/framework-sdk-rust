//! 调用上下文（RFC-0003）：回放向量 `primitives/trace-context.json` 与 Baggage 值的向量，以及截止时间与取消的归因（规则 1、RFC-0004）。

mod common;

use std::thread;
use std::time::{Duration, Instant, SystemTime};

use xihan_framework::invocation::{
    CancellationToken, Context, ERROR_CODE_CANCELLED, ERROR_CODE_DEADLINE_EXCEEDED, MESSAGE_CANCELLED, MESSAGE_DEADLINE_EXCEEDED, TraceContext,
};
use xihan_framework::primitives::{BAGGAGE_CORRELATION, BAGGAGE_IDEMPOTENCY_KEY, BAGGAGE_LOCALE, BAGGAGE_TENANT, ERROR_CODE_INVALID_ARGUMENT};

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
    ctx.cancellation_error().map(|error| error.code().to_owned())
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
    let error = ctx.cancellation_error().unwrap();
    assert_eq!((error.code(), error.message_key(), error.kind()), (ERROR_CODE_DEADLINE_EXCEEDED, Some(MESSAGE_DEADLINE_EXCEEDED), "TIMEOUT"));
    assert!(ctx.is_cancelled() && ctx.cancellation_token().is_cancelled());
    assert!(ctx.is_expired(SystemTime::now()));
    assert_eq!(ctx.remaining_time(SystemTime::now()), Some(Duration::ZERO));

    let caller = CancellationToken::new();
    let cancelled = Context::builder().cancellation(&caller).build();
    caller.cancel();
    common::block_on(cancelled.done());
    let error = cancelled.cancellation_error().unwrap();
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
    common::assert_kernel_error(&ctx.cancellation_error().unwrap(), MESSAGE_DEADLINE_EXCEEDED);

    // 构建上下文时不判断「开始前已过」：截止时间已过的上下文立即超时，键仍是 deadline_exceeded；before_start 由调用器判断
    let late = Context::builder().deadline(SystemTime::now() - Duration::from_millis(1)).build();
    common::assert_kernel_error(&late.cancellation_error().unwrap(), MESSAGE_DEADLINE_EXCEEDED);

    let caller = CancellationToken::new();
    let cancelled = Context::builder().cancellation(&caller).build();
    caller.cancel();
    common::assert_kernel_error(&cancelled.cancellation_error().unwrap(), MESSAGE_CANCELLED);
}

/// Baggage 的值约束（conventions/baggage-keys.md，ADR-0235）：构建上下文按外部输入丢弃不合的成员，程序内逐项派生给已登记键不合的值即 panic；幂等键与未登记的键不按这条处理。
#[test]
fn baggage_values_follow_the_conventions() {
    let values = common::baggage_values();
    // 以码元列出的样例里组得成字符串的（成对的代理项）照收；孤立的代理项组不成 Rust 的字符串，到不了上下文（ADR-0321）
    let mut acceptable = values.acceptable.clone();
    acceptable.extend(values.acceptable_utf16.iter().map(|case| case.text().unwrap_or_else(|error| panic!("{}: {error}", case.name))));
    assert!(values.unacceptable_utf16.iter().all(|case| case.text().is_err()));
    for value in &acceptable {
        let ctx = Context::builder().baggage(BAGGAGE_TENANT, value.as_str()).build();
        assert_eq!(ctx.tenant(), Some(value.as_str()), "{value:?} must be kept");
        assert_eq!(Context::background().with_baggage(BAGGAGE_CORRELATION, value.as_str()).correlation(), Some(value.as_str()));
    }
    for value in &values.unacceptable {
        let ctx = Context::builder()
            .baggage(BAGGAGE_TENANT, value.as_str())
            .baggage(BAGGAGE_IDEMPOTENCY_KEY, value.as_str())
            .baggage("xihan.note", value.as_str())
            .build();
        assert_eq!(ctx.tenant(), None, "{value:?} must be dropped");
        assert_eq!(ctx.idempotency_key(), Some(value.as_str()), "the idempotency key is left to the idempotency stage");
        assert_eq!(ctx.baggage().get("xihan.note").map(String::as_str), Some(value.as_str()), "unregistered keys are not checked");
        // 程序内逐项派生给已登记的键一个不合的值是编程错误：panic，不静默丢弃也不保留原值
        let message = common::panic_message(|| drop(correlated_context().with_baggage(BAGGAGE_CORRELATION, value.as_str())));
        assert!(message.starts_with(ERROR_CODE_INVALID_ARGUMENT) && message.contains("'xihan.correlation'"), "{message}");
        assert_eq!(Context::background().with_baggage(BAGGAGE_IDEMPOTENCY_KEY, value.as_str()).idempotency_key(), Some(value.as_str()));
        assert_eq!(Context::background().with_baggage("xihan.note", value.as_str()).baggage().get("xihan.note").map(String::as_str), Some(value.as_str()));
    }
    // 外部输入丢弃不合的成员，同名的原值保留；合格的值改写原值
    assert_eq!(
        Context::builder()
            .baggage(BAGGAGE_TENANT, "t1")
            .baggage(
                BAGGAGE_TENANT,
                "a
b"
            )
            .build()
            .tenant(),
        Some("t1")
    );
    assert_eq!(Context::builder().baggage(BAGGAGE_TENANT, "t1").baggage(BAGGAGE_TENANT, "t2").build().tenant(), Some("t2"));
}

fn correlated_context() -> Context {
    Context::builder().baggage(BAGGAGE_CORRELATION, "c1").build()
}

#[derive(serde::Deserialize)]
struct TraceVectors {
    parse: TraceParseCases,
    format: Vec<TraceFormatCase>,
}

#[derive(serde::Deserialize)]
struct TraceParseCases {
    valid: Vec<TraceParseCase>,
    invalid: Vec<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TraceParseCase {
    text: String,
    trace_id: String,
    span_id: String,
    sampled: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TraceFormatCase {
    trace_id: String,
    span_id: String,
    sampled: Option<bool>,
    text: String,
}

/// traceparent 的解析与写出按 trace-context.json（RFC-0003 规则 7）：不合法的当作没有给出，写出只带采样位。
#[test]
fn trace_context_follows_the_vectors() {
    let vectors: TraceVectors = common::vector("primitives/trace-context.json");
    for case in &vectors.parse.valid {
        let parsed = TraceContext::parse(&case.text).unwrap_or_else(|| panic!("{} must parse", case.text));
        assert_eq!(
            (parsed.trace_id(), parsed.span_id(), parsed.sampled()),
            (case.trace_id.as_str(), case.span_id.as_str(), Some(case.sampled)),
            "{}",
            case.text
        );
        let ctx = Context::builder().trace_parent(&case.text).build();
        assert_eq!(ctx.trace_id(), case.trace_id);
        assert!(ctx.trace_parent().ends_with(if case.sampled { "-01" } else { "-00" }), "only the sampled bit is written: {}", ctx.trace_parent());
    }
    for text in &vectors.parse.invalid {
        assert!(TraceContext::parse(text).is_none(), "{text:?} must be invalid");
        let ctx = Context::builder().trace_parent(text).build();
        assert_eq!((ctx.trace().sampled(), ctx.trace_id().len()), (None, 32), "an invalid traceparent is treated as absent: {text:?}");
    }
    for case in &vectors.format {
        let trace = TraceContext::with_sampling(case.trace_id.as_str(), case.span_id.as_str(), case.sampled).unwrap();
        assert_eq!(trace.trace_parent(), case.text);
    }
    let root = Context::background();
    assert_eq!((root.trace().sampled(), root.trace().is_sampled()), (None, true));
    assert!(root.trace_parent().ends_with("-01"));
}

/// 派生上下文（RFC-0003）：`with_cancellation` 还跟随另一个令牌，`with_deadline` / `with_timeout` 只能收紧截止时间；追踪、Baggage
/// 与对象袋与原上下文共享，派生上下文的取消与截止时间不回传给原上下文。
#[test]
fn derived_contexts_follow_more_tokens_and_tighter_deadlines() {
    let caller = CancellationToken::new();
    let ctx = Context::builder().cancellation(&caller).baggage(BAGGAGE_TENANT, "t1").build();
    ctx.insert_item(Attempt(1));

    // 还跟随另一个令牌：它取消时派生的上下文取消，按调用方取消归因，原上下文不受影响
    let extra = CancellationToken::new();
    let derived = ctx.with_cancellation(&extra);
    assert_eq!((derived.trace_id(), derived.span_id(), derived.tenant()), (ctx.trace_id(), ctx.span_id(), Some("t1")));
    assert_eq!(derived.item::<Attempt>().as_deref(), Some(&Attempt(1)), "the items are shared");
    assert_eq!(code(&derived), None);
    extra.cancel();
    common::block_on(derived.done());
    assert_eq!(code(&derived).as_deref(), Some(ERROR_CODE_CANCELLED));
    assert!(!ctx.is_cancelled() && !caller.is_cancelled(), "cancelling a derived context does not reach the original");
    assert!(ctx.with_cancellation(&extra).is_cancelled(), "a context derived with a cancelled token is born cancelled");
    let follower = ctx.with_cancellation(&CancellationToken::new());

    // 截止时间：原来没有的加上，原上下文仍然没有
    let timed = ctx.with_timeout(Duration::from_secs(60));
    let at = timed.deadline().expect("the derived context has a deadline");
    assert!(!ctx.has_deadline() && !timed.is_cancelled());
    assert_eq!((timed.trace_id(), timed.tenant(), timed.item::<Attempt>().as_deref()), (ctx.trace_id(), Some("t1"), Some(&Attempt(1))));
    // 只能收紧：更晚的截止时间、表示不了的超时都不改变它
    assert_eq!(timed.with_deadline(at + Duration::from_secs(60)).deadline(), Some(at), "a later deadline does not loosen it");
    assert_eq!(timed.with_deadline(at).deadline(), Some(at));
    assert_eq!(timed.with_timeout(Duration::MAX).deadline(), Some(at));
    assert!(!ctx.with_timeout(Duration::MAX).has_deadline(), "a timeout beyond SystemTime means no deadline");
    let tighter = timed.with_deadline(at - Duration::from_secs(30));
    assert_eq!((tighter.deadline(), timed.deadline()), (Some(at - Duration::from_secs(30)), Some(at)));
    assert!(tighter.remaining_time(SystemTime::now()).unwrap() <= Duration::from_secs(30));

    // 已经过去的截止时间立即触发，只触发派生的上下文
    let expired = timed.with_deadline(SystemTime::now() - Duration::from_secs(1));
    assert_eq!(code(&expired).as_deref(), Some(ERROR_CODE_DEADLINE_EXCEEDED));
    assert!(!timed.is_cancelled() && !ctx.is_cancelled());
    // 收紧后的截止时间到了：派生的上下文超时，原上下文照常
    let short = timed.with_timeout(Duration::from_millis(20));
    common::block_on(short.done());
    assert_eq!(code(&short).as_deref(), Some(ERROR_CODE_DEADLINE_EXCEEDED));
    assert!(!timed.is_cancelled() && !ctx.is_cancelled());

    // 原上下文取消时派生的都随之取消：截止令牌没有触发的按调用方取消归因，已经超时的仍是超时
    caller.cancel();
    assert!(follower.is_cancelled() && timed.is_cancelled() && tighter.is_cancelled());
    for cancelled in [&follower, &timed, &tighter] {
        assert_eq!(code(cancelled).as_deref(), Some(ERROR_CODE_CANCELLED));
    }
    assert_eq!(code(&short).as_deref(), Some(ERROR_CODE_DEADLINE_EXCEEDED));
}

/// 原上下文的截止时间到了，派生的上下文同样是超时：归因看的是它继承的截止令牌，不因为多跟随了一个令牌就算作取消。
#[test]
fn an_inherited_deadline_is_a_timeout_for_the_derived_context() {
    let extra = CancellationToken::new();
    let outer = Context::builder().timeout(Duration::from_millis(20)).build();
    let derived = outer.with_cancellation(&extra);
    assert_eq!(derived.deadline(), outer.deadline());
    common::block_on(derived.done());
    assert_eq!(code(&derived).as_deref(), Some(ERROR_CODE_DEADLINE_EXCEEDED));
    extra.cancel();
    assert_eq!(code(&derived).as_deref(), Some(ERROR_CODE_DEADLINE_EXCEEDED), "a later cancellation does not change it");
}

/// 访问器的旧名 `Context::cancellation` 弃用但仍可用，与 `cancellation_error` 给出同一个结果。
#[test]
#[allow(deprecated)]
fn the_old_name_of_the_accessor_still_works() {
    let caller = CancellationToken::new();
    let ctx = Context::builder().cancellation(&caller).build();
    assert_eq!((ctx.cancellation(), ctx.cancellation_error()), (None, None));
    caller.cancel();
    assert_eq!(ctx.cancellation(), ctx.cancellation_error());
    assert_eq!(ctx.cancellation().as_ref().map(xihan_framework::primitives::Error::code), Some(ERROR_CODE_CANCELLED));
}
