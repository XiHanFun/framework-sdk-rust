//! 操作（RFC-0011）：回放向量 `operations/payload-reasons.json`、`primitives/operation-ids.json` 与 `primitives/kernel-messages.json`，
//! 以及登记、调用器、内置步骤、容量、短路、嵌套调用与 `'static` 调用。

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;
use xihan_framework::invocation::Context;
use xihan_framework::operation::{
    self, CAPACITY, Contract, DEADLINE_MIDDLEWARE, ERROR_CODE_DUPLICATE, ERROR_CODE_INVALID_PAYLOAD, ERROR_CODE_NOT_FOUND, ERROR_CODE_REGISTRY_FULL,
    ERROR_CODE_TYPE_MISMATCH, EXPOSE_AI, EXPOSE_HTTP, Invoker, MESSAGE_DUPLICATE, MESSAGE_INVALID_PAYLOAD, MESSAGE_NO_RESPONSE, MESSAGE_NOT_FOUND,
    MESSAGE_REGISTRY_FULL, MESSAGE_TYPE_MISMATCH, Registry, Request, Schema, VALIDATION_MIDDLEWARE,
};
use xihan_framework::pipeline::{self, APP_OWNER, Builder, Placement};
use xihan_framework::primitives::{BAGGAGE_CORRELATION, ERROR_CODE_INVALID_ARGUMENT, Error, PipelineStage, Stability, json};

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
                // 中间件看到的是编码之前的响应对象（RFC-0011 规则 10「编码」）
                let response = operation::with_response(ctx, |created: &OrderCreated| format!("{}:{}", created.id, created.quantity));
                seen.lock().unwrap().push((id, payload, response));
                outcome
            })
        })
    };
    let invoker = Invoker::new(registry(), Builder::new().use_at(observe, PipelineStage::Observe, Placement::End, APP_OWNER).unwrap()).unwrap();

    let ctx = correlated("c1");
    let response = common::block_on(invoker.invoke(&ctx, Request::new("orders.create", r#"{"quantity":2}"#))).unwrap();
    assert_eq!(String::from_utf8(response).unwrap(), r#"{"id":"o-c1","quantity":2}"#);
    assert_eq!(seen.lock().unwrap().as_slice(), [(Some("orders.create".to_owned()), Some(br#"{"quantity":2}"#.to_vec()), Some("o-c1:2".to_owned()))]);
    assert!(
        operation::current(&ctx).is_none() && operation::with_response(&ctx, |_: &OrderCreated| ()).is_none(),
        "the scope is restored after the invocation"
    );

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

/// operation-ids.json：操作 ID 的合法与非法样例，含 128 个字符的上限两侧。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OperationIdVectors {
    max_length: usize,
    valid: Vec<String>,
    invalid: Vec<String>,
}

/// 操作 ID 的文法与 128 个字符的上限（`conventions/operation-ids.md`、RFC-0011「操作 ID」，ADR-0276）：判定与登记表用同一条规则。
#[test]
fn operation_ids_follow_the_vectors() {
    let vectors: OperationIdVectors = common::vector("primitives/operation-ids.json");
    assert!(!vectors.valid.is_empty() && !vectors.invalid.is_empty());
    assert_eq!(operation::ID_MAX_LENGTH, vectors.max_length);
    let registry = Registry::new();
    let handler = || operation::handler_fn(|_ctx, _request: CreateOrder| Box::pin(async { Ok(()) }));
    for id in &vectors.valid {
        assert!(operation::is_id(id), "{id:?} must be valid");
        registry.add(Contract::new(id.as_str()), handler()).unwrap_or_else(|error| panic!("{id:?} must be registered: {}", error.message()));
    }
    assert_eq!(registry.len(), vectors.valid.len());
    for id in &vectors.invalid {
        assert!(!operation::is_id(id), "{id:?} must be invalid");
        let error = registry.add(Contract::new(id.as_str()), handler()).unwrap_err();
        assert_eq!(
            (error.code(), error.message()),
            (
                ERROR_CODE_INVALID_ARGUMENT,
                format!("The operation ID '{id}' must match <resource>.<verb> and be at most {} characters long.", vectors.max_length).as_str()
            ),
            "{id:?}"
        );
    }
    assert_eq!(registry.len(), vectors.valid.len(), "rejected registrations are not kept");

    // 向量要把上限的两侧都带上：正好在上限上的合法，别处都合文法、只多一个字符的不合法
    assert!(vectors.valid.iter().any(|id| id.len() == vectors.max_length));
    assert!(vectors.invalid.iter().any(|id| id.len() == vectors.max_length + 1));
    // 上限按整个 ID 计，与哪一段长无关
    let long = "a".repeat(vectors.max_length - 2);
    assert!(operation::is_id(&format!("{long}.b")) && !operation::is_id(&format!("{long}a.b")));
    assert!(operation::is_id(&format!("a.{long}")) && !operation::is_id(&format!("a.{long}b")));
}

/// 暴露的协议（`plugin.proto` 的 `OperationDeclaration.expose`）：空表示不对外发布，是 `Contract::new` 的缺省；写了就只能是五个协议名，
/// 拼错的名字在登记时报出来，不留到宿主那里被悄悄忽略。
#[test]
fn registry_rejects_unknown_expose_names() {
    let registry = Registry::new();
    let handler = || operation::handler_fn(|_ctx, _request: CreateOrder| Box::pin(async { Ok(()) }));
    assert!(Contract::new("orders.create").expose.is_empty(), "a contract is not published on any protocol unless it says so");

    // 名字区分大小写，与协议里的写法逐字相同；一个不认识的名字就拒绝整个登记
    for protocols in [vec!["htpp"], vec!["HTTP"], vec![""], vec![EXPOSE_HTTP, "rest"]] {
        let error = registry.add(Contract::new("orders.create").with_expose(protocols.clone()), handler()).unwrap_err();
        let unknown = protocols.last().unwrap();
        assert_eq!(
            (error.code(), error.message()),
            (ERROR_CODE_INVALID_ARGUMENT, format!("The operation 'orders.create' cannot be exposed to '{unknown}'.").as_str()),
            "{protocols:?}"
        );
    }
    // 同一个协议写两次也拒绝；按出现的顺序报第一处问题，不认识的名字与重复谁在前报谁
    for (protocols, problem) in [
        (vec![EXPOSE_HTTP, EXPOSE_AI, EXPOSE_HTTP], "lists 'http' more than once"),
        (vec![EXPOSE_AI, EXPOSE_AI, "rest"], "lists 'ai' more than once"),
        (vec![EXPOSE_HTTP, "rest", EXPOSE_HTTP], "cannot be exposed to 'rest'"),
    ] {
        let error = registry.add(Contract::new("orders.create").with_expose(protocols.clone()), handler()).unwrap_err();
        assert_eq!(
            (error.code(), error.message()),
            (ERROR_CODE_INVALID_ARGUMENT, format!("The operation 'orders.create' {problem}.").as_str()),
            "{protocols:?}"
        );
    }
    assert!(registry.is_empty());

    // 五个协议名都认，EXPOSE_ALL 就是它们
    assert_eq!(operation::EXPOSE_ALL, [EXPOSE_HTTP, operation::EXPOSE_GRPC, operation::EXPOSE_MCP, operation::EXPOSE_COMMAND_LINE, EXPOSE_AI]);
    registry.add(Contract::new("orders.create").with_expose(operation::EXPOSE_ALL), handler()).unwrap();
    assert_eq!(registry.get("orders.create").unwrap().contract().expose, ["http", "grpc", "mcp", "commandline", "ai"]);
    // 不写（空）照常登记：不对外发布，只能在进程内调用
    registry.add(Contract::new("orders.list"), handler()).unwrap();
    assert!(registry.get("orders.list").unwrap().contract().expose.is_empty());
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
    common::assert_kernel_error(&error, operation::MESSAGE_UNHANDLED_PANIC);
    assert!(error.inner().is_none());

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

    // 按 panic 内容的类型认出框架错误，不看文本：看起来像「码: 说明」的字符串 panic 没有内部错误
    let registry = Registry::new();
    registry
        .add(
            Contract::new("orders.lookalike"),
            operation::handler_fn::<CreateOrder, (), _>(|_ctx, _request| {
                Box::pin(async move { panic!("{ERROR_CODE_INVALID_ARGUMENT}: not a framework error") })
            }),
        )
        .unwrap();
    registry
        .add(
            Contract::new("orders.thrown"),
            operation::handler_fn::<CreateOrder, (), _>(|_ctx, _request| {
                Box::pin(async move {
                    std::panic::panic_any(xihan_framework::primitives::FrameworkError::with_message(
                        "ORDERS_CONFLICT_001",
                        "orders.duplicate",
                        [("id".to_owned(), json!("o1"))].into(),
                    ))
                })
            }),
        )
        .unwrap();
    let invoker = Invoker::new(Arc::new(registry), &Builder::new()).unwrap();
    let lookalike = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.lookalike", "{}"))).unwrap_err();
    assert!(lookalike.inner().is_none(), "a string panic is not a framework error");
    let thrown = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.thrown", "{}"))).unwrap_err();
    let inner = thrown.inner().expect("a FrameworkError payload is the inner error");
    assert_eq!(
        (inner.code(), inner.message_key(), inner.details().map(|details| details["id"].clone())),
        ("ORDERS_CONFLICT_001", Some("orders.duplicate"), Some(json!("o1")))
    );
}

/// 记下进入过的中间件名。
fn recorder(name: &'static str, log: &Arc<Mutex<Vec<&'static str>>>) -> impl pipeline::Middleware + 'static {
    let log = Arc::clone(log);
    pipeline::from_fn(name, move |ctx, next| {
        let log = Arc::clone(&log);
        Box::pin(async move {
            log.lock().unwrap().push(name);
            next.run(ctx).await
        })
    })
}

/// 截止时间在调用开始时已过：Resilience 阶段的内置步骤在进入时按时钟判断，报 deadline_before_start（RFC-0011 规则 10，ADR-0236）。
/// 排在它之前的 Ingress 与 Observe 阶段照常执行，之后的中间件与操作不执行；构造上下文时不判断。
#[test]
fn expired_deadlines_stop_at_the_resilience_stage() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut builder = Builder::new();
    builder
        .use_at(recorder("Ingress", &log), PipelineStage::Ingress, Placement::End, APP_OWNER)
        .unwrap()
        .use_at(recorder("Observe", &log), PipelineStage::Observe, Placement::End, APP_OWNER)
        .unwrap()
        .use_at(recorder("BeforeDeadline", &log), PipelineStage::Resilience, Placement::Beginning, APP_OWNER)
        .unwrap()
        .use_at(recorder("Retry", &log), PipelineStage::Resilience, Placement::End, APP_OWNER)
        .unwrap();
    let invoker = Invoker::new(registry(), &builder).unwrap();
    let late = Context::builder().deadline(std::time::SystemTime::now() - std::time::Duration::from_millis(1)).build();
    // 截止时间先于载荷：Resilience 在 Validate 之前，载荷不合法也报截止时间
    let error = common::block_on(invoker.invoke(&late, Request::new("orders.create", "not json"))).unwrap_err();
    common::assert_kernel_error(&error, xihan_framework::invocation::MESSAGE_DEADLINE_BEFORE_START);
    let typed = common::block_on(invoker.invoke_typed::<CreateOrder, OrderCreated>(&late, "orders.create", CreateOrder { quantity: 1 })).unwrap_err();
    common::assert_kernel_error(&typed, xihan_framework::invocation::MESSAGE_DEADLINE_BEFORE_START);
    // 归属 app 排在 xihan.kernel.core 之前，所以 BeforeDeadline 也执行了
    assert_eq!(*log.lock().unwrap(), ["Ingress", "Observe", "BeforeDeadline", "Ingress", "Observe", "BeforeDeadline"]);
    assert!(operation::current(&late).is_none());

    // 进管道前只核对未登记与类型不符，按这个顺序
    let missing = common::block_on(invoker.invoke_typed::<String, String>(&late, "orders.missing", String::new())).unwrap_err();
    assert_eq!(missing.code(), ERROR_CODE_NOT_FOUND, "an unknown operation is reported first");
    let mismatch = common::block_on(invoker.invoke_typed::<String, String>(&late, "orders.create", String::new())).unwrap_err();
    assert_eq!(mismatch.code(), ERROR_CODE_TYPE_MISMATCH, "a type mismatch is reported before the pipeline");

    log.lock().unwrap().clear();
    let in_time = Context::builder().timeout(std::time::Duration::from_secs(60)).build();
    assert!(common::block_on(invoker.invoke(&in_time, Request::new("orders.create", r#"{"quantity":1}"#))).is_ok());
    assert_eq!(*log.lock().unwrap(), ["Ingress", "Observe", "BeforeDeadline", "Retry"]);
}

/// 三个内置步骤都按归属 xihan.kernel.core 放在所在阶段的 Beginning，与同位置的中间件按 RFC-0004 排序（RFC-0011 规则 10）：
/// Observe 只接住排在它之后的 panic，Validate 在这里解码，排在它前面的中间件看得到不合法的载荷。
#[test]
fn built_in_steps_sit_where_the_reference_puts_them() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let explode = |name: &'static str| pipeline::from_fn(name, |_ctx, _next| Box::pin(async { panic!("the middleware crashed") }));
    let mut builder = Builder::new();
    builder
        .use_at(recorder("Early", &log), PipelineStage::Validate, Placement::Beginning, APP_OWNER)
        .unwrap()
        .use_at(recorder("Late", &log), PipelineStage::Validate, Placement::Beginning, "zeta.late.validator")
        .unwrap()
        .use_at(recorder("Audit", &log), PipelineStage::Observe, Placement::Beginning, "zeta.audit.core")
        .unwrap();
    let invoker = Invoker::new(registry(), &builder).unwrap();
    assert_eq!(
        invoker.pipeline().dump(),
        "observe      ObserveMiddleware\nobserve      Audit\nresilience   DeadlineMiddleware\nvalidate     Early\nvalidate     ValidationMiddleware\nvalidate     Late\n"
    );
    let invalid = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.create", r#"{"quantity":"x"}"#))).unwrap_err();
    common::assert_kernel_error(&invalid, MESSAGE_INVALID_PAYLOAD);
    assert_eq!(*log.lock().unwrap(), ["Audit", "Early"], "decoding fails at the built-in step, after Early and before Late");

    // 顺序约束可以按名字引用内置步骤；同名的中间件与内置步骤冲突
    let mut constrained = Builder::new();
    constrained.use_at(recorder("Guard", &log), PipelineStage::Validate, Placement::End, APP_OWNER).unwrap().before("Guard", VALIDATION_MIDDLEWARE).unwrap();
    let dump = Invoker::new(registry(), &constrained).unwrap().pipeline().dump();
    assert!(dump.ends_with("validate     Guard\nvalidate     ValidationMiddleware\n"), "{dump}");
    let mut clash = Builder::new();
    clash.use_at(recorder(DEADLINE_MIDDLEWARE, &log), PipelineStage::Handle, Placement::End, APP_OWNER).unwrap();
    assert_eq!(Invoker::new(registry(), &clash).unwrap_err().code(), "KERNEL_PIPELINE_003");

    // Ingress 阶段与 Observe 阶段 Beginning 上归属排在 xihan.kernel.core 之前的 panic 原样交给调用方，排在之后的被翻译
    for (stage, placement, owner, caught) in [
        (PipelineStage::Ingress, Placement::End, APP_OWNER, false),
        (PipelineStage::Observe, Placement::Beginning, APP_OWNER, false),
        (PipelineStage::Observe, Placement::Beginning, "zeta.audit.core", true),
        (PipelineStage::Observe, Placement::End, APP_OWNER, true),
        (PipelineStage::Transaction, Placement::End, APP_OWNER, true),
    ] {
        let mut builder = Builder::new();
        builder.use_at(explode("Explode"), stage, placement, owner).unwrap();
        let invoker = Invoker::new(registry(), &builder).unwrap();
        let ctx = Context::background();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            common::block_on(invoker.invoke(&ctx, Request::new("orders.create", r#"{"quantity":1}"#)))
        }));
        match (outcome, caught) {
            (Ok(Err(error)), true) => assert_eq!(error.message_key(), Some(operation::MESSAGE_UNHANDLED_PANIC)),
            (Err(_), false) => {}
            (outcome, _) => panic!("{stage} {placement} {owner}: caught must be {caught}, got {:?}", outcome.map(|result| result.map(|_| ()))),
        }
        assert!(operation::current(&ctx).is_none(), "the scope is restored after a panic");
    }

    // 排在 Validate 之后的中间件再次进入下游时，处理器拿到重新解码的请求
    let retry = pipeline::from_fn("Retry", |ctx, next| {
        Box::pin(async move {
            let _ = next.run(ctx).await;
            next.run(ctx).await
        })
    });
    let mut builder = Builder::new();
    builder.use_middleware(retry).unwrap();
    let invoker = Invoker::new(registry(), &builder).unwrap();
    let response = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.create", r#"{"quantity":4}"#))).unwrap();
    assert_eq!(String::from_utf8(response).unwrap(), r#"{"id":"o-","quantity":4}"#);
}

/// 响应写不出（RFC-0011 规则 10「编码」，ADR-0236）：管道结束之后编码，类型化的浮点成员是 NaN 或无穷、序列化不支持的值或序列化时
/// panic，都以 KERNEL_UNEXPECTED_001 + kernel.invocation.unhandled_exception 结束，框架错误进 inner，不抛给调用方，没有载荷。
#[test]
fn unwritable_responses_become_unhandled_failures() {
    #[derive(Serialize)]
    struct Measured {
        ratio: f64,
    }
    struct Exploding;
    impl Serialize for Exploding {
        fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            std::panic::panic_any(xihan_framework::primitives::FrameworkError::new("ORDERS_UNEXPECTED_001", "the response cannot be written."))
        }
    }
    let registry = Registry::new();
    registry
        .add(Contract::new("ratios.get"), operation::handler_fn(|_ctx, _request: CreateOrder| Box::pin(async { Ok(Measured { ratio: f64::NAN }) })))
        .unwrap();
    registry.add(Contract::new("limits.get"), operation::handler_fn(|_ctx, _request: CreateOrder| Box::pin(async { Ok(vec![f32::INFINITY]) }))).unwrap();
    registry.add(Contract::new("bombs.get"), operation::handler_fn(|_ctx, _request: CreateOrder| Box::pin(async { Ok(Exploding) }))).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observe = {
        let seen = Arc::clone(&seen);
        pipeline::from_fn("Seen", move |ctx, next| {
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                let outcome = next.run(ctx).await;
                // 中间件看到的是编码之前的对象：NaN 还在，调用在这里还是成功的
                seen.lock().unwrap().push((outcome.is_ok(), operation::with_response(ctx, |measured: &Measured| measured.ratio.is_nan())));
                outcome
            })
        })
    };
    let mut builder = Builder::new();
    builder.use_middleware(observe).unwrap();
    let invoker = Invoker::new(Arc::new(registry), &builder).unwrap();
    for id in ["ratios.get", "limits.get"] {
        let error = common::block_on(invoker.invoke(&Context::background(), Request::new(id, "{}"))).unwrap_err();
        common::assert_kernel_error(&error, operation::MESSAGE_UNHANDLED_PANIC);
        assert!(error.inner().is_none(), "{id}: a serialization error is not a framework error");
    }
    assert_eq!(*seen.lock().unwrap(), [(true, Some(true)), (true, None)]);
    let error = common::block_on(invoker.invoke(&Context::background(), Request::new("bombs.get", "{}"))).unwrap_err();
    common::assert_kernel_error(&error, operation::MESSAGE_UNHANDLED_PANIC);
    assert_eq!(error.inner().map(|inner| inner.code()), Some("ORDERS_UNEXPECTED_001"), "a framework error raised while encoding is the inner error");
    // 类型化调用不编码，NaN 原样交回
    let typed = common::block_on(invoker.invoke_typed::<CreateOrder, Measured>(&Context::background(), "ratios.get", CreateOrder::default())).unwrap();
    assert!(typed.ratio.is_nan());
}

/// 层层套着的数组，最里面是 `T`。
#[derive(Clone, Serialize)]
#[serde(untagged)]
enum Nested<T> {
    /// 最里面的值。
    Leaf(T),
    /// 只有一项的数组。
    List(Vec<Nested<T>>),
}

/// 把 `inner` 套进 `depth` 层数组。
fn nested<T>(depth: usize, inner: T) -> Nested<T> {
    (0..depth).fold(Nested::Leaf(inner), |value, _| Nested::List(vec![value]))
}

/// `depth` 层数组的文本，最里面是 0。
fn nested_text(depth: usize) -> String {
    format!("{}0{}", "[".repeat(depth), "]".repeat(depth))
}

/// 自己写两次成员 `a` 的响应。
#[derive(Clone)]
struct Twice;

impl Serialize for Twice {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("a", &1)?;
        map.serialize_entry("a", &2)?;
        map.end()
    }
}

/// 展开的映射里有与字段同名的键。
#[derive(Clone, Serialize)]
struct Flattened {
    /// 与映射里的键同名的字段。
    a: i64,
    /// 展开到同一层的成员。
    #[serde(flatten)]
    extra: BTreeMap<String, i64>,
}

/// 登记一个总是返回 `response` 的操作。
fn add_returning<R: Serialize + Clone + Send + Sync + 'static>(registry: &Registry, id: &str, response: R) {
    registry
        .add(
            Contract::new(id),
            operation::handler_fn(move |_ctx, _request: CreateOrder| {
                let response = response.clone();
                Box::pin(async move { Ok(response) })
            }),
        )
        .unwrap();
}

/// 原样交出 `text` 的 `RawValue`。
fn raw(text: &str) -> Box<serde_json::value::RawValue> {
    serde_json::value::RawValue::from_string(text.to_owned()).unwrap()
}

/// 写出的整段文本要过 RFC-0001「JSON 文本」的共同检查（RFC-0011 规则 10「编码」，ADR-0319）：嵌套超过 64 层（从响应的根算起，
/// 原样嵌进去的 `RawValue` 与 `Json` 也算）、任何一层有重复的成员名（自定义的 `Serialize`、`#[serde(flatten)]` 的映射与 `RawValue`
/// 都写得出）、`RawValue` 里孤立代理项的转义，都以 KERNEL_UNEXPECTED_001 + kernel.invocation.unhandled_exception 结束，不带 inner、
/// 没有载荷；恰好 64 层照常写出。
#[test]
fn responses_that_fail_the_json_text_check_are_not_written() {
    let registry = Registry::new();
    add_returning(&registry, "texts.deep", nested(65, 0));
    add_returning(&registry, "texts.deep-raw", nested(10, raw(&nested_text(60))));
    add_returning(&registry, "texts.deep-json", nested(10, json::Json::from_text(&nested_text(60)).unwrap()));
    add_returning(&registry, "texts.raw-duplicate", raw(r#"{"a":1,"a":2}"#));
    add_returning(&registry, "texts.raw-surrogate", raw(r#"{"a":"\ud800"}"#));
    add_returning(&registry, "texts.twice", Twice);
    add_returning(&registry, "texts.twice-nested", vec![BTreeMap::from([("b".to_owned(), Twice)])]);
    add_returning(&registry, "texts.flattened", Flattened { a: 1, extra: BTreeMap::from([("a".to_owned(), 2)]) });
    add_returning(&registry, "texts.deep-enough", nested(64, 0));
    add_returning(&registry, "texts.deep-enough-raw", nested(10, raw(&nested_text(54))));
    add_returning(&registry, "texts.deep-enough-json", nested(10, json::Json::from_text(&nested_text(54)).unwrap()));
    add_returning(&registry, "texts.flattened-apart", Flattened { a: 1, extra: BTreeMap::from([("b".to_owned(), 2)]) });
    let registry = Arc::new(registry);
    let invoker = Invoker::new(Arc::clone(&registry), &Builder::new()).unwrap();
    let ctx = Context::background();
    for id in [
        "texts.deep",
        "texts.deep-raw",
        "texts.deep-json",
        "texts.raw-duplicate",
        "texts.raw-surrogate",
        "texts.twice",
        "texts.twice-nested",
        "texts.flattened",
    ] {
        for outcome in [common::block_on(invoker.invoke(&ctx, Request::new(id, "{}"))), common::block_on(registry.get(id).unwrap().execute(&ctx, b"{}"))] {
            let error = outcome.expect_err(id);
            common::assert_kernel_error(&error, operation::MESSAGE_UNHANDLED_PANIC);
            assert!(error.inner().is_none(), "{id}: a response that is not JSON text has no inner error");
        }
    }
    for (id, expected) in [
        ("texts.deep-enough", nested_text(64)),
        ("texts.deep-enough-raw", nested_text(64)),
        ("texts.deep-enough-json", nested_text(64)),
        ("texts.flattened-apart", r#"{"a":1,"b":2}"#.to_owned()),
    ] {
        let payload = common::block_on(invoker.invoke(&ctx, Request::new(id, "{}"))).unwrap();
        assert_eq!(String::from_utf8(payload).unwrap(), expected, "{id}");
    }
}

/// 任意 JSON 的成员原样保留读到的值（RFC-0011 规则 2，ADR-0236）：处理器原样返回，响应载荷与原文的 JSON 值相等，
/// 1e400 不变成 null 或无穷，超出双精度的整数保持精确，2.0 与 2 的区别保留。
#[test]
fn any_json_members_keep_what_was_read() {
    #[derive(Deserialize)]
    struct Echo {
        data: json::Json,
    }
    let registry = Registry::new();
    registry.add(Contract::new("echoes.create"), operation::handler_fn(|_ctx, request: Echo| Box::pin(async move { Ok(request.data) }))).unwrap();
    let invoker = Invoker::new(Arc::new(registry), &Builder::new()).unwrap();
    let data = r#"{"big":1e400,"exact":9007199254740993,"huge":123456789012345678901234567890,"float":2.0,"integer":2,"zero":-0.0,"tiny":1E-400,"text":"é\/\t","nested":[{"b":1,"a":null}]}"#;
    let payload = format!(r#"{{ "data" : {data} }}"#);
    let response = common::block_on(invoker.invoke(&Context::background(), Request::new("echoes.create", payload))).unwrap();
    let response = String::from_utf8(response).unwrap();
    // 数字照原文，成员照文档顺序；字符串的转义按 JSON 字符串写法重写，JSON 值不变
    assert_eq!(response, data.replace(r"\/", "/"));
    assert_eq!(json::Json::from_text(&response).unwrap(), json::Json::from_text(data).unwrap());
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
    meta: Option<json::Json>,
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

/// 取消与截止时间只经令牌通知（RFC-0003 规则 1，ADR-0236）：调用器与内置步骤不中止处理器、不丢弃它的 future；令牌触发后处理器照常
/// 返回结果的，调用以那个结果结束，只有处理器把取消写成结果时才按令牌归因。
#[test]
fn cancellation_only_notifies_through_the_token() {
    let registry = Registry::new();
    registry
        .add(
            Contract::new("waits.finish"),
            operation::handler_fn(|ctx: &Context, _request: CreateOrder| {
                Box::pin(async move {
                    ctx.done().await;
                    Ok(format!("finished after {}", if ctx.is_cancelled() { "the token fired" } else { "nothing" }))
                })
            }),
        )
        .unwrap();
    registry
        .add(
            Contract::new("waits.report"),
            operation::handler_fn(|ctx: &Context, _request: CreateOrder| {
                Box::pin(async move {
                    ctx.done().await;
                    Err::<(), _>(ctx.cancellation_error().expect("the token fired"))
                })
            }),
        )
        .unwrap();
    // 下游之后照常做事的中间件：令牌触发也不打断它
    let after = Arc::new(Mutex::new(0));
    let counting = {
        let after = Arc::clone(&after);
        pipeline::from_fn("Counting", move |ctx, next| {
            let after = Arc::clone(&after);
            Box::pin(async move {
                let outcome = next.run(ctx).await;
                *after.lock().unwrap() += 1;
                outcome
            })
        })
    };
    let mut builder = Builder::new();
    builder.use_middleware(counting).unwrap();
    let invoker = Invoker::new(Arc::new(registry), &builder).unwrap();
    let timeout = std::time::Duration::from_millis(20);

    let timed = Context::builder().timeout(timeout).build();
    let finished = common::block_on(invoker.invoke(&timed, Request::new("waits.finish", "{}"))).unwrap();
    assert_eq!(finished, br#""finished after the token fired""#, "the deadline does not abort the handler");
    let reported = common::block_on(invoker.invoke(&Context::builder().timeout(timeout).build(), Request::new("waits.report", "{}"))).unwrap_err();
    common::assert_kernel_error(&reported, xihan_framework::invocation::MESSAGE_DEADLINE_EXCEEDED);

    let token = xihan_framework::invocation::CancellationToken::new();
    let cancelled = Context::builder().cancellation(&token).build();
    let canceller = {
        let token = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(timeout);
            token.cancel();
        })
    };
    let typed = common::block_on(invoker.invoke_typed::<CreateOrder, String>(&cancelled, "waits.finish", CreateOrder::default())).unwrap();
    assert_eq!(typed, "finished after the token fired", "a caller's cancellation does not abort the handler either");
    canceller.join().unwrap();
    assert_eq!(*after.lock().unwrap(), 3, "the middleware ran to the end every time");
}

/// 下游把取消写成 `KERNEL_CANCELLED_001` 的结果时，截止时间中间件按截止令牌归因（RFC-0003 规则 1，ADR-0236）：截止令牌已触发即
/// `KERNEL_TIMEOUT_001` + `kernel.invocation.deadline_exceeded`，只有调用方取消时仍是取消。
#[test]
fn a_cancelled_result_after_the_deadline_is_a_timeout() {
    use xihan_framework::invocation::{CancellationToken, ERROR_CODE_CANCELLED, MESSAGE_CANCELLED, MESSAGE_DEADLINE_EXCEEDED};

    let registry = Registry::new();
    registry
        .add(
            Contract::new("waits.cancelled"),
            operation::handler_fn(|ctx: &Context, _request: CreateOrder| {
                Box::pin(async move {
                    ctx.done().await;
                    // 只知道取消发生了：写成取消，不看是哪个令牌
                    Err::<(), _>(Error::cancelled(ERROR_CODE_CANCELLED, MESSAGE_CANCELLED))
                })
            }),
        )
        .unwrap();
    let invoker = Invoker::new(Arc::new(registry), &Builder::new()).unwrap();
    let timeout = std::time::Duration::from_millis(20);

    let timed = Context::builder().timeout(timeout).build();
    let error = common::block_on(invoker.invoke(&timed, Request::new("waits.cancelled", "{}"))).unwrap_err();
    common::assert_kernel_error(&error, MESSAGE_DEADLINE_EXCEEDED);

    // 截止令牌已触发时调用方同时取消也是超时
    let token = CancellationToken::new();
    let both = Context::builder().cancellation(&token).timeout(timeout).build();
    let canceller = {
        let token = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(timeout * 3);
            token.cancel();
        })
    };
    let error = common::block_on(invoker.invoke(&both, Request::new("waits.cancelled", "{}"))).unwrap_err();
    common::assert_kernel_error(&error, MESSAGE_DEADLINE_EXCEEDED);
    canceller.join().unwrap();

    let token = CancellationToken::new();
    let cancelled = Context::builder().cancellation(&token).build();
    let canceller = {
        let token = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(timeout);
            token.cancel();
        })
    };
    let error = common::block_on(invoker.invoke(&cancelled, Request::new("waits.cancelled", "{}"))).unwrap_err();
    common::assert_kernel_error(&error, MESSAGE_CANCELLED);
    canceller.join().unwrap();
}

/// 中间件在 handle 阶段之后看到编码之前的响应对象，XGI 调用与类型化调用一样（.NET 的 FindOperationResponse，RFC-0011 规则 3、10）；
/// 类型化调用的响应不经编码，原样交回调用方。
#[test]
fn middlewares_see_the_response_of_both_kinds_of_invocation() {
    #[derive(Clone, Serialize)]
    struct Report {
        lines: usize,
    }
    let registry = registry();
    registry
        .add(
            Contract::new("reports.build"),
            operation::handler_fn(|_ctx, request: CreateOrder| {
                Box::pin(async move { Ok(Report { lines: usize::try_from(request.quantity).unwrap_or_default() }) })
            }),
        )
        .unwrap_or_else(|error| panic!("{error}"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let reader = {
        let seen = Arc::clone(&seen);
        pipeline::from_fn("Reader", move |ctx, next| {
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                let before = operation::with_response(ctx, |_: &OrderCreated| ()).is_some();
                let outcome = next.run(ctx).await;
                let created = operation::with_response(ctx, |created: &OrderCreated| created.quantity);
                let report = operation::with_response(ctx, |report: &Report| report.lines);
                seen.lock().unwrap().push((before, created, report));
                outcome
            })
        })
    };
    let mut builder = Builder::new();
    builder.use_at(reader, PipelineStage::Handle, Placement::End, APP_OWNER).unwrap();
    let invoker = Invoker::new(registry, &builder).unwrap();
    let ctx = Context::background();

    let payload = common::block_on(invoker.invoke(&ctx, Request::new("orders.create", r#"{"quantity":2}"#))).unwrap();
    assert_eq!(payload, br#"{"id":"o-","quantity":2}"#);
    let created = common::block_on(invoker.invoke_typed::<CreateOrder, OrderCreated>(&ctx, "orders.create", CreateOrder { quantity: 3 })).unwrap();
    assert_eq!(created.quantity, 3, "the typed caller still receives the response");
    let report = common::block_on(invoker.invoke_typed::<CreateOrder, Report>(&ctx, "reports.build", CreateOrder { quantity: 4 })).unwrap();
    assert_eq!(report.lines, 4);
    let failed = common::block_on(invoker.invoke_typed::<CreateOrder, OrderCreated>(&ctx, "orders.create", CreateOrder::default())).unwrap_err();
    assert_eq!(failed.code(), "ORDERS_VALIDATION_001");
    assert_eq!(
        *seen.lock().unwrap(),
        [(false, Some(2), None), (false, Some(3), None), (false, None, Some(4)), (false, None, None)],
        "the response is visible after the handler in both kinds of invocation, and only then"
    );
    assert!(operation::with_response(&ctx, |_: &OrderCreated| ()).is_none(), "the slot is restored after the invocation");
}
