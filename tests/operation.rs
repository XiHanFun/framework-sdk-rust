//! 操作：Go SDK `operation` 测试的移植，以及容量、短路、嵌套调用与 `'static` 调用。

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;
use xihan_framework::invocation::Context;
use xihan_framework::operation::{
    self, CAPACITY, Contract, ERROR_CODE_DUPLICATE, ERROR_CODE_INVALID_PAYLOAD, ERROR_CODE_NOT_FOUND, ERROR_CODE_REGISTRY_FULL, ERROR_CODE_TYPE_MISMATCH,
    EXPOSE_AI, EXPOSE_HTTP, Invoker, MESSAGE_INVALID_PAYLOAD, MESSAGE_NOT_FOUND, MESSAGE_TYPE_MISMATCH, Registry, Request, Schema,
};
use xihan_framework::pipeline::{self, APP_OWNER, Builder, Placement};
use xihan_framework::primitives::{BAGGAGE_CORRELATION, ERROR_CODE_INVALID_ARGUMENT, ERROR_CODE_UNINITIALIZED_RESULT, Error, PipelineStage, Stability};

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
    let invoker = Invoker::new(registry(), &Builder::new().use_at(observe, PipelineStage::Observe, Placement::End, APP_OWNER)).unwrap();

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
    assert_eq!(seen.lock().unwrap().len(), 2, "an unknown operation does not enter the pipeline");

    for payload in ["null", " null ", "[true]", "[1, 2]", "{", r#"{"quantity":"two"}"#] {
        let invalid = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.create", payload))).unwrap_err();
        assert_eq!((invalid.code(), invalid.message_key()), (ERROR_CODE_INVALID_PAYLOAD, Some(MESSAGE_INVALID_PAYLOAD)), "payload {payload:?}");
        let details = invalid.details().unwrap();
        assert_eq!(details["id"], json!("orders.create"));
        let reason = details["reason"].as_str().unwrap();
        assert!(!reason.contains("CreateOrder"), "the reason must not name the request type: {reason}");
    }
    let empty = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.create", " \n"))).unwrap_err();
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
    let details = mismatch.details().unwrap();
    assert_eq!((details["expected"].as_str(), details["actual"].as_str()), (Some("CreateOrder -> OrderCreated"), Some("CreateOrder -> String")));

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
    assert_eq!((error.code(), error.message()), (ERROR_CODE_DUPLICATE, "The operation 'orders.create' is already registered."));
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
    assert_eq!((full.code(), full.message()), (ERROR_CODE_REGISTRY_FULL, "The operation registry is full (10000 operations)."));
    assert_eq!(registry.len(), CAPACITY);
}

#[test]
fn short_circuits_and_nested_invocations() {
    // 以成功短路、没有执行操作：没有响应可交出
    let skip = pipeline::from_fn("Skip", |_ctx, _next| Box::pin(async { Ok(()) }));
    let invoker = Invoker::new(registry(), &Builder::new().use_at(skip, PipelineStage::Idempotency, Placement::End, APP_OWNER)).unwrap();
    let skipped = common::block_on(invoker.invoke(&Context::background(), Request::new("orders.create", r#"{"quantity":1}"#))).unwrap_err();
    assert_eq!(skipped.code(), ERROR_CODE_UNINITIALIZED_RESULT);
    let skipped = common::block_on(invoker.invoke_typed::<CreateOrder, OrderCreated>(&Context::background(), "orders.create", CreateOrder { quantity: 1 }));
    assert_eq!(skipped.unwrap_err().code(), ERROR_CODE_UNINITIALIZED_RESULT);

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
    let invoker = Arc::new(Invoker::new(Arc::clone(&registry), &Builder::new().use_at(observe, PipelineStage::Observe, Placement::End, APP_OWNER)).unwrap());
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
