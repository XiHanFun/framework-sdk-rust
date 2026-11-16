//! 操作：Go SDK `operation` 测试的移植，以及容量、短路、嵌套调用与 `'static` 调用。

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;
use xihan_framework::invocation::Context;
use xihan_framework::operation::{
    self, CAPACITY, Contract, ERROR_CODE_DUPLICATE, ERROR_CODE_INVALID_PAYLOAD, ERROR_CODE_NOT_FOUND, ERROR_CODE_REGISTRY_FULL, ERROR_CODE_TYPE_MISMATCH,
    EXPOSE_AI, EXPOSE_HTTP, Invoker, MESSAGE_DUPLICATE, MESSAGE_INVALID_PAYLOAD, MESSAGE_NO_RESPONSE, MESSAGE_NOT_FOUND, MESSAGE_REGISTRY_FULL,
    MESSAGE_TYPE_MISMATCH, Registry, Request, Schema,
};
use xihan_framework::pipeline::{self, APP_OWNER, Builder, Placement};
use xihan_framework::primitives::{BAGGAGE_CORRELATION, ERROR_CODE_INVALID_ARGUMENT, Error, PipelineStage, Stability};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct CreateOrder {
    quantity: i64,
}

#[derive(Debug, PartialEq, Serialize)]
struct OrderCreated {
    id: String,
    quantity: i64,
}

fn registry() -> Arc<Registry> {
    let registry = Registry::new();
    let handler = operation::handler_fn(|ctx: &Context, request: CreateOrder| {
        Box::pin(async move {
            if request.quantity <= 0 {
                return Err(Error::validation("ORDERS_VALIDATION_001", "orders.quantity.invalid").with_detail("quantity", request.quantity));
            }
            Ok(OrderCreated { id: format!("o-{}", ctx.correlation().unwrap_or_default()), quantity: request.quantity })
        })
    });
    let contract = Contract::new("orders.create").with_stability(Stability::Stable).with_expose([EXPOSE_HTTP]);
    registry.add_with(contract, handler, Some(Schema::new("{}", "{}")), APP_OWNER).unwrap();
    Arc::new(registry)
}

fn correlated(correlation: &str) -> Context {
    Context::builder().baggage(BAGGAGE_CORRELATION, correlation).build()
}

#[test]
fn invoke_runs_the_pipeline_and_encodes_the_response() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observe = {
        let seen = Arc::clone(&seen);
        pipeline::from_fn("Observe", move |ctx, next| {
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                let id = operation::current(ctx).map(|descriptor| descriptor.id().to_owned());
                let payload = operation::request_of(ctx).map(|request| request.payload.clone());
                let outcome = next.run(ctx).await;
                let response = operation::response_payload_of(ctx).map(|payload| String::from_utf8(payload).unwrap());
                seen.lock().unwrap().push((id, payload, response));
                outcome
            })
        })
    };
    let invoker = Invoker::new(registry(), Builder::new().use_at(observe, PipelineStage::Observe, Placement::End, APP_OWNER).unwrap()).unwrap();

    let ctx = correlated("c1");
    let response = common::block_on(invoker.invoke(&ctx, Request::new("orders.create", r#"{"quantity":2}"#))).unwrap();
    assert_eq!(String::from_utf8(response).unwrap(), r#"{"id":"o-c1","quantity":2}"#);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [(Some("orders.create".to_owned()), Some(br#"{"quantity":2}"#.to_vec()), Some(r#"{"id":"o-c1","quantity":2}"#.to_owned()))]
    );
    assert!(operation::current(&ctx).is_none() && operation::response_payload_of(&ctx).is_none(), "the scope is restored after the invocation");

    let failed = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.create", r#"{"quantity":0}"#))).unwrap_err();
    assert_eq!(failed.code(), "ORDERS_VALIDATION_001");

    let missing = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.missing", ""))).unwrap_err();
    assert_eq!((missing.code(), missing.message_key()), (ERROR_CODE_NOT_FOUND, Some(MESSAGE_NOT_FOUND)));
    assert_eq!(missing.message(), "Operation 'orders.missing' is not registered.", "the built-in en-US template");
    common::assert_kernel_error(&missing, MESSAGE_NOT_FOUND);
    assert_eq!(seen.lock().unwrap().len(), 2, "an unknown operation does not enter the pipeline");

    // reason 只写出错位置的 JSON 路径（RFC-0011 规则 2）
    for (payload, reason) in [
        ("null", "$"),
        (" null ", "$"),
        // 对象位置只收 JSON 对象：数组形式的结构体是根上的类型不符（ADR-0236）
        ("[true]", "$"),
        // 只有空白不是 JSON，不等价于空载荷
        (" \n", "$"),
        ("[1, 2]", "$"),
        ("{", "$"),
        (r#"{"quantity":"two"}"#, "$.quantity"),
        (r#"{"quantity":1} {}"#, "$"),
    ] {
        let invalid = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.create", payload))).unwrap_err();
        assert_eq!((invalid.code(), invalid.message_key()), (ERROR_CODE_INVALID_PAYLOAD, Some(MESSAGE_INVALID_PAYLOAD)), "payload {payload:?}");
        assert_eq!(invalid.details().unwrap()["reason"], json!(reason), "payload {payload:?}");
        common::assert_kernel_error(&invalid, MESSAGE_INVALID_PAYLOAD);
    }
    let empty = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.create", ""))).unwrap_err();
    assert_eq!(empty.code(), "ORDERS_VALIDATION_001", "an empty payload is {{}}");
}

#[test]
fn typed_invoke_checks_types() {
    let invoker = Invoker::new(registry(), &Builder::new()).unwrap();
    let ctx = correlated("c2");
    let created = common::block_on(invoker.invoke_typed::<CreateOrder, OrderCreated>(&ctx, "orders.create", CreateOrder { quantity: 3 })).unwrap();
    assert_eq!(created, OrderCreated { id: "o-c2".to_owned(), quantity: 3 });

    let mismatch =
        common::block_on(invoker.invoke_typed::<CreateOrder, String>(&Context::background(), "orders.create", CreateOrder { quantity: 3 })).unwrap_err();
    assert_eq!((mismatch.code(), mismatch.message_key()), (ERROR_CODE_TYPE_MISMATCH, Some(MESSAGE_TYPE_MISMATCH)));
    // 只带 id（ADR-0236）：类型名是实现语言的，登记的类型仍可从描述里读
    assert_eq!(mismatch.details().unwrap().keys().collect::<Vec<_>>(), ["id"]);
    assert_eq!(mismatch.message(), "Operation 'orders.create' was invoked with request or response types that do not match its registration.");
    let descriptor = invoker.registry().get("orders.create").unwrap();
    assert_eq!((descriptor.request_type(), descriptor.response_type()), ("CreateOrder", "OrderCreated"));
    common::assert_kernel_error(&mismatch, MESSAGE_TYPE_MISMATCH);

    let missing = common::block_on(invoker.invoke_typed::<CreateOrder, OrderCreated>(&ctx, "orders.missing", CreateOrder::default())).unwrap_err();
    assert_eq!(missing.code(), ERROR_CODE_NOT_FOUND);
    let failed = common::block_on(invoker.invoke_typed::<CreateOrder, OrderCreated>(&ctx, "orders.create", CreateOrder::default())).unwrap_err();
    assert_eq!(failed.code(), "ORDERS_VALIDATION_001");
}

#[test]
fn registry_rejects_duplicates_and_bad_ids() {
    let registry = Registry::new();
    let handler = || operation::handler_fn(|_ctx, _request: CreateOrder| Box::pin(async { Ok(()) }));
    assert_eq!(registry.add(Contract::new("Orders.Create"), handler()).unwrap_err().code(), ERROR_CODE_INVALID_ARGUMENT, "bad ID");
    let both = Contract::new("orders.create").with_permission("orders.write").with_policy("owners");
    let error = registry.add(both, handler()).unwrap_err();
    assert_eq!((error.code(), error.message()), (ERROR_CODE_INVALID_ARGUMENT, "The operation 'orders.create' must declare either a permission or a policy."));
    assert_eq!(registry.add_with(Contract::new("orders.create"), handler(), None, "").unwrap_err().code(), ERROR_CODE_INVALID_ARGUMENT, "empty owner");
    for schema in [Schema::new("[]", "{}"), Schema::new("{}", "not json")] {
        assert_eq!(registry.add_with(Contract::new("orders.create"), handler(), Some(schema), APP_OWNER).unwrap_err().code(), ERROR_CODE_INVALID_ARGUMENT);
    }
    assert!(registry.is_empty());

    registry.add(Contract::new("orders.create").with_permission("orders.write").with_expose([EXPOSE_HTTP, EXPOSE_AI]), handler()).unwrap();
    let error = registry.add(Contract::new("orders.create"), handler()).unwrap_err();
    assert_eq!((error.code(), error.message()), (ERROR_CODE_DUPLICATE, "Operation 'orders.create' is already registered by 'app'."));
    common::assert_kernel_error(&error.to_error(), MESSAGE_DUPLICATE);
    registry.add_with(Contract::new("order-lines.list"), handler(), None, "xihan.samples.orders").unwrap();

    let list = registry.list();
    let ids: Vec<&str> = list.iter().map(|descriptor| descriptor.id()).collect();
    assert_eq!(ids, ["order-lines.list", "orders.create"], "listed by ID");
    assert_eq!((list[0].owner(), list[1].owner()), ("xihan.samples.orders", APP_OWNER));
    assert_eq!(list[1].contract().expose, ["http", "ai"]);
    assert_eq!(list[1].contract().permission.as_deref(), Some("orders.write"));
    assert_eq!((list[1].request_type(), list[1].response_type()), ("CreateOrder", "()"));
    assert_eq!(registry.len(), 2);
    assert!(operation::is_id("order-lines.list") && !operation::is_id("orders"));
}

#[test]
fn registry_capacity_is_bounded() {
    let registry = Registry::new();
    for index in 0..CAPACITY {
        registry.add(Contract::new(format!("r{index}.create")), operation::handler_fn(|_ctx, _request: CreateOrder| Box::pin(async { Ok(()) }))).unwrap();
    }
    assert_eq!(registry.len(), CAPACITY);
    let handler = || operation::handler_fn(|_ctx, _request: CreateOrder| Box::pin(async { Ok(()) }));
    let duplicate = registry.add(Contract::new("r0.create"), handler()).unwrap_err();
    assert_eq!(duplicate.code(), ERROR_CODE_DUPLICATE, "duplicates are reported before capacity");
    let full = registry.add(Contract::new("extra.create"), handler()).unwrap_err();
    assert_eq!((full.code(), full.message()), (ERROR_CODE_REGISTRY_FULL, "The operation registry has reached its capacity of 10000."));
    common::assert_kernel_error(&full.to_error(), MESSAGE_REGISTRY_FULL);
    assert_eq!(registry.len(), CAPACITY);
}

#[test]
fn short_circuits_and_nested_invocations() {
    // 以成功短路、没有执行操作：没有响应可交出
    let skip = pipeline::from_fn("Skip", |_ctx, _next| Box::pin(async { Ok(()) }));
    let invoker = Invoker::new(registry(), Builder::new().use_at(skip, PipelineStage::Idempotency, Placement::End, APP_OWNER).unwrap()).unwrap();
    let skipped = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.create", r#"{"quantity":1}"#))).unwrap_err();
    common::assert_kernel_error(&skipped, MESSAGE_NO_RESPONSE);
    assert_eq!(skipped.message(), "Operation 'orders.create' completed without producing a response.");
    let skipped = common::block_on(invoker.invoke_typed::<CreateOrder, OrderCreated>(&Context::background(), "orders.create", CreateOrder { quantity: 1 }));
    common::assert_kernel_error(&skipped.unwrap_err(), MESSAGE_NO_RESPONSE);

    // 嵌套调用各看到自己的操作，返回后恢复外层
    let registry = registry();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observe = {
        let seen = Arc::clone(&seen);
        pipeline::from_fn("Observe", move |ctx, next| {
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                let outcome = next.run(ctx).await;
                seen.lock().unwrap().push(operation::current(ctx).map(|descriptor| descriptor.id().to_owned()));
                outcome
            })
        })
    };
    let invoker =
        Arc::new(Invoker::new(Arc::clone(&registry), Builder::new().use_at(observe, PipelineStage::Observe, Placement::End, APP_OWNER).unwrap()).unwrap());
    let inner = Arc::clone(&invoker);
    registry
        .add(
            Contract::new("orders.reorder"),
            operation::handler_fn(move |ctx: &Context, request: CreateOrder| {
                let inner = Arc::clone(&inner);
                Box::pin(
                    async move { inner.invoke_typed::<CreateOrder, OrderCreated>(ctx, "orders.create", request).await.map(|created| created.quantity * 10) },
                )
            }),
        )
        .unwrap();
    let ctx = correlated("c3");
    let response = common::block_on(invoker.invoke(&ctx, Request::new("orders.reorder", r#"{"quantity":4}"#))).unwrap();
    assert_eq!(response, b"40");
    assert_eq!(seen.lock().unwrap().as_slice(), [Some("orders.create".to_owned()), Some("orders.reorder".to_owned())]);

    // 'static 的调用可以交给别的线程
    let call = invoker.invoke_owned(correlated("c4"), Request::new("orders.create", r#"{"quantity":5}"#));
    let response = std::thread::spawn(move || common::block_on(call)).join().unwrap().unwrap();
    assert_eq!(response, br#"{"id":"o-c4","quantity":5}"#);
}

#[test]
fn execute_skips_the_pipeline_and_reports_encoding_failures() {
    let maps = Registry::new();
    maps.add(
        Contract::new("maps.get"),
        operation::handler_fn(|_ctx, _request: CreateOrder| Box::pin(async { Ok(BTreeMap::from([((1u8, 2u8), "tuple keys are not JSON object keys")])) })),
    )
    .unwrap();
    let descriptor = maps.get("maps.get").unwrap();
    let error = common::block_on(descriptor.execute(&Context::background(), b"{}")).unwrap_err();
    assert_eq!((error.code(), error.message_key()), ("KERNEL_UNEXPECTED_001", None));
    assert!(error.message().starts_with("The response of 'maps.get' could not be encoded"), "{}", error.message());

    let ok = registry();
    let created = common::block_on(ok.get("orders.create").unwrap().execute(&correlated("c5"), br#"{"quantity":1}"#)).unwrap();
    assert_eq!(created, br#"{"id":"o-c5","quantity":1}"#);
}

#[derive(Clone, Debug, Deserialize)]
struct PlaceOrder {
    #[allow(dead_code)]
    lines: Vec<Line>,
}

#[derive(Clone, Debug, Deserialize)]
struct Line {
    #[allow(dead_code)]
    quantity: u32,
    #[serde(rename = "unit price")]
    #[allow(dead_code)]
    unit_price: f64,
}

#[test]
fn invalid_payload_reasons_are_json_paths() {
    let registry = Registry::new();
    registry.add(Contract::new("orders.place"), operation::handler_fn(|_ctx, _request: PlaceOrder| Box::pin(async { Ok(()) }))).unwrap();
    let invoker = Invoker::new(Arc::new(registry), &Builder::new()).unwrap();
    for (payload, reason) in [
        (r#"{"lines":[{"quantity":1,"unit price":2.5},{"quantity":-1,"unit price":1}]}"#, "$.lines[1].quantity"),
        (r#"{"lines":[{"quantity":1,"unit price":"x"}]}"#, "$.lines[0]['unit price']"),
        (r#"{"lines":[{"quantity":1}]}"#, "$.lines[0]"),
        (r#"{"lines":{}}"#, "$.lines"),
        ("{}", "$"),
    ] {
        let error = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.place", payload))).unwrap_err();
        assert_eq!(error.details().unwrap()["reason"], json!(reason), "payload {payload}");
    }
    assert!(common::block_on(invoker.invoke(&Context::background(), Request::new("orders.place", r#"{"lines":[]}"#))).is_ok());
}

/// 中间件或处理器 panic 时调用以 KERNEL_UNEXPECTED_001 结束（RFC-0011 规则 5），对象袋恢复成调用前的样子。
#[test]
fn panics_become_unexpected_errors() {
    let crashing = Registry::new();
    crashing
        .add(
            Contract::new("orders.crash"),
            operation::handler_fn(|_ctx, request: CreateOrder| {
                Box::pin(async move {
                    assert!(request.quantity >= 0, "the handler crashed");
                    Ok(())
                })
            }),
        )
        .unwrap();
    let invoker = Invoker::new(Arc::new(crashing), &Builder::new()).unwrap();
    let ctx = Context::background();
    let crashed = common::block_on(invoker.invoke(&ctx, Request::new("orders.crash", r#"{"quantity":-1}"#))).unwrap_err();
    assert_eq!((crashed.code(), crashed.message_key(), crashed.details()), ("KERNEL_UNEXPECTED_001", Some(operation::MESSAGE_UNHANDLED_PANIC), None));
    assert_eq!(crashed.message(), "An unhandled exception occurred during the invocation.");
    common::assert_kernel_error(&crashed, operation::MESSAGE_UNHANDLED_PANIC);
    assert!(crashed.inner().is_none(), "an ordinary panic has no inner error");
    assert!(operation::current(&ctx).is_none(), "the scope is restored after a panic");
    let typed = common::block_on(invoker.invoke_typed::<CreateOrder, ()>(&ctx, "orders.crash", CreateOrder { quantity: -1 })).unwrap_err();
    assert_eq!(typed.message_key(), Some(operation::MESSAGE_UNHANDLED_PANIC));
    assert!(common::block_on(invoker.invoke(&ctx, Request::new("orders.crash", r#"{"quantity":1}"#))).is_ok(), "the invoker keeps working");

    let explode = pipeline::from_fn("Explode", |_ctx, _next| Box::pin(async { panic!("the middleware crashed") }));
    let invoker = Invoker::new(registry(), Builder::new().use_at(explode, PipelineStage::Observe, Placement::End, APP_OWNER).unwrap()).unwrap();
    let crashed = common::block_on(invoker.invoke(&ctx, Request::new("orders.create", r#"{"quantity":1}"#))).unwrap_err();
    assert_eq!(crashed.message_key(), Some(operation::MESSAGE_UNHANDLED_PANIC));
}

/// panic 来自框架错误（原语的非法参数）时，那个错误放进 inner（RFC-0011 规则 10）。
#[test]
fn framework_panics_become_inner_errors() {
    let registry = Registry::new();
    registry
        .add(
            Contract::new("orders.misuse"),
            operation::handler_fn(|_ctx, _request: CreateOrder| {
                Box::pin(async move {
                    let code = String::from("not-a-code");
                    Err::<(), _>(Error::new(code, "orders.misuse"))
                })
            }),
        )
        .unwrap();
    let invoker = Invoker::new(Arc::new(registry), &Builder::new()).unwrap();
    let crashed = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.misuse", "{}"))).unwrap_err();
    common::assert_kernel_error(&crashed, operation::MESSAGE_UNHANDLED_PANIC);
    let inner = crashed.inner().expect("the framework error is the inner error");
    assert_eq!((inner.code(), inner.message_key()), (ERROR_CODE_INVALID_ARGUMENT, None));
    assert!(inner.message().contains("'not-a-code'"), "{}", inner.message());
}

/// 截止时间在调用开始时已过：调用器在第一个中间件之前按时钟判断，报 deadline_before_start，管道与操作都不执行（RFC-0011 规则 10）。
#[test]
fn expired_deadlines_stop_before_the_pipeline() {
    let entered = Arc::new(Mutex::new(0));
    let count = {
        let entered = Arc::clone(&entered);
        pipeline::from_fn("Count", move |ctx, next| {
            let entered = Arc::clone(&entered);
            Box::pin(async move {
                *entered.lock().unwrap() += 1;
                next.run(ctx).await
            })
        })
    };
    let invoker = Invoker::new(registry(), Builder::new().use_at(count, PipelineStage::Observe, Placement::Beginning, APP_OWNER).unwrap()).unwrap();
    let late = Context::builder().deadline(std::time::SystemTime::now() - std::time::Duration::from_millis(1)).build();
    let error = common::block_on(invoker.invoke(&late, Request::new("orders.create", r#"{"quantity":1}"#))).unwrap_err();
    common::assert_kernel_error(&error, xihan_framework::invocation::MESSAGE_DEADLINE_BEFORE_START);
    let typed = common::block_on(invoker.invoke_typed::<CreateOrder, OrderCreated>(&late, "orders.create", CreateOrder { quantity: 1 })).unwrap_err();
    common::assert_kernel_error(&typed, xihan_framework::invocation::MESSAGE_DEADLINE_BEFORE_START);
    assert_eq!(*entered.lock().unwrap(), 0, "the pipeline does not run");
    assert!(operation::current(&late).is_none());

    let missing = common::block_on(invoker.invoke(&late, Request::new("orders.missing", "{}"))).unwrap_err();
    assert_eq!(missing.code(), ERROR_CODE_NOT_FOUND, "an unknown operation is reported first");

    let in_time = Context::builder().timeout(std::time::Duration::from_secs(60)).build();
    assert!(common::block_on(invoker.invoke(&in_time, Request::new("orders.create", r#"{"quantity":1}"#))).is_ok());
    assert_eq!(*entered.lock().unwrap(), 1);
}

/// payload-reasons.json 的订单：lines 是可选的数组，每项是对象，quantity 是可选的 32 位整数。
#[derive(Clone, Debug, Deserialize)]
struct VectorOrder {
    #[allow(dead_code)]
    lines: Option<Vec<VectorLine>>,
}

#[derive(Clone, Debug, Deserialize)]
struct VectorLine {
    #[serde(default)]
    #[allow(dead_code)]
    quantity: i32,
}

/// payload-reasons.json 的 profile，按 README 的映射声明：name 必填、接受 null；整数、浮点、布尔可选而不接受 null；
/// 字符串、数组、对象与任意 JSON 接受 null；inner.n 必填。
#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct VectorProfile {
    #[serde(deserialize_with = "operation::nullable")]
    name: Option<String>,
    note: Option<String>,
    #[serde(default)]
    count: i64,
    #[serde(default)]
    ratio: f64,
    #[serde(default)]
    flag: bool,
    tags: Option<Vec<Option<String>>>,
    scores: Option<Vec<i64>>,
    inner: Option<VectorInner>,
    meta: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct VectorInner {
    n: i64,
}

#[derive(Deserialize)]
struct ReasonVectors {
    requests: BTreeMap<String, String>,
    cases: Vec<ReasonCase>,
}

#[derive(Deserialize)]
struct ReasonCase {
    name: String,
    request: String,
    payload: String,
    reason: Option<String>,
}

/// 载荷的解码按 payload-reasons.json（RFC-0011 规则 2，ADR-0236）：先过共同的 JSON 读取检查，再按请求类型解码；
/// reason 为 null 表示解码成功，否则逐字相等。counts 是 32 位整数的字典，order 是带明细数组的订单，profile 带必填成员与各种取值。
#[test]
fn payload_reasons_follow_the_vectors() {
    let registry = Registry::new();
    registry.add(Contract::new("counts.set"), operation::handler_fn(|_ctx, _request: BTreeMap<String, i32>| Box::pin(async { Ok(()) }))).unwrap();
    registry.add(Contract::new("orders.place"), operation::handler_fn(|_ctx, _request: VectorOrder| Box::pin(async { Ok(()) }))).unwrap();
    registry.add(Contract::new("profiles.save"), operation::handler_fn(|_ctx, _request: VectorProfile| Box::pin(async { Ok(()) }))).unwrap();
    let invoker = Invoker::new(Arc::new(registry), &Builder::new()).unwrap();
    let vectors: ReasonVectors = common::vector("operations/payload-reasons.json");
    assert_eq!(vectors.requests.keys().collect::<Vec<_>>(), ["counts", "order", "profile"]);
    for case in &vectors.cases {
        let id = match case.request.as_str() {
            "counts" => "counts.set",
            "order" => "orders.place",
            "profile" => "profiles.save",
            other => panic!("unknown request type {other}"),
        };
        let outcome = common::block_on(invoker.invoke(&Context::background(), Request::new(id, case.payload.as_str())));
        match (&case.reason, outcome) {
            (None, Ok(_)) => {}
            (None, Err(error)) => panic!("{} must decode: {error}", case.name),
            (Some(reason), Err(error)) => {
                common::assert_kernel_error(&error, MESSAGE_INVALID_PAYLOAD);
                assert_eq!(error.details().unwrap()["reason"], json!(reason), "{}", case.name);
            }
            (Some(reason), Ok(_)) => panic!("{} must fail at {reason}", case.name),
        }
    }
}
