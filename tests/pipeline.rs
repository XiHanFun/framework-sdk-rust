//! 管道：Go SDK `pipeline` 测试的移植，以及按名字的约束、多次进入下游与派生上下文。

mod common;

use std::sync::{Arc, Mutex};

use xihan_framework::invocation::Context;
use xihan_framework::pipeline::{self, APP_OWNER, Builder, ERROR_CODE_CYCLE, ERROR_CODE_REGISTRATION, ERROR_CODE_UNKNOWN_STAGE, Middleware, Placement, Step};
use xihan_framework::primitives::{BAGGAGE_CORRELATION, Error, PipelineStage};

/// 中间件与终点写下的执行记录。
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn push(&self, entry: impl Into<String>) {
        self.0.lock().unwrap().push(entry.into());
    }

    fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

fn recording(name: &'static str, log: &Log) -> impl Middleware + 'static {
    let log = log.clone();
    pipeline::from_fn(name, move |ctx, next| {
        let log = log.clone();
        Box::pin(async move {
            log.push(format!("{name}:before"));
            let outcome = next.run(ctx).await;
            log.push(format!("{name}:after"));
            outcome
        })
    })
}

fn passthrough(name: &'static str) -> impl Middleware + 'static {
    pipeline::from_fn(name, |ctx, next| Box::pin(async move { next.run(ctx).await }))
}

fn terminal(log: &Log) -> impl Step + 'static {
    let log = log.clone();
    pipeline::step_fn(move |_ctx| {
        log.push("terminal");
        Box::pin(async { Ok(()) })
    })
}

fn assert_code(builder: &Builder, code: &str) {
    let error = builder.describe().expect_err(code);
    assert_eq!(error.code(), code, "{error}");
    assert_eq!(builder.build().unwrap_err().code(), code);
}

#[test]
fn order_follows_stage_placement_owner_and_registration() {
    let log = Log::default();
    let builder = Builder::new()
        .use_at(recording("Egress", &log), PipelineStage::Egress, Placement::End, APP_OWNER)
        .use_at(recording("ObserveLate", &log), PipelineStage::Observe, Placement::End, "xihan.b")
        .use_at(recording("ObserveEarly", &log), PipelineStage::Observe, Placement::Beginning, "xihan.z")
        .use_at(recording("ObserveA", &log), PipelineStage::Observe, Placement::End, "xihan.a")
        .use_middleware(recording("Transaction", &log));
    let dump = builder.dump().unwrap();
    assert_eq!(dump, "observe      ObserveEarly\nobserve      ObserveA\nobserve      ObserveLate\ntransaction  Transaction\negress       Egress\n");

    let pipeline = builder.build().unwrap();
    assert_eq!(pipeline.dump(), dump);
    let transaction = &pipeline.descriptors()[3];
    assert_eq!(
        (transaction.stage, transaction.owner.as_str(), transaction.placement, transaction.instance.as_deref()),
        (PipelineStage::Transaction, APP_OWNER, Placement::End, None)
    );
    common::block_on(pipeline.run(&Context::background(), &terminal(&log))).unwrap();
    assert_eq!(
        log.entries().join(","),
        "ObserveEarly:before,ObserveA:before,ObserveLate:before,Transaction:before,Egress:before,terminal,Egress:after,Transaction:after,ObserveLate:after,ObserveA:after,ObserveEarly:after"
    );
}

#[test]
fn constraints_reorder_within_a_stage() {
    let builder = Builder::new()
        .use_at(passthrough("A"), PipelineStage::Validate, Placement::End, APP_OWNER)
        .use_at(passthrough("B"), PipelineStage::Validate, Placement::End, APP_OWNER)
        .use_at(passthrough("C"), PipelineStage::Validate, Placement::End, APP_OWNER)
        .before("C", "A")
        .after("B", "C");
    let order: Vec<String> = builder.describe().unwrap().into_iter().map(|descriptor| descriptor.middleware).collect();
    assert_eq!(order, ["C", "A", "B"]);

    // 约束只调整必要的相对位置，其余保持基准次序
    let builder = Builder::new()
        .use_at(passthrough("A"), PipelineStage::Validate, Placement::End, APP_OWNER)
        .use_at(passthrough("B"), PipelineStage::Validate, Placement::End, APP_OWNER)
        .use_at(passthrough("C"), PipelineStage::Validate, Placement::End, APP_OWNER)
        .use_at(passthrough("D"), PipelineStage::Validate, Placement::End, APP_OWNER)
        .after("A", "D");
    let order: Vec<String> = builder.describe().unwrap().into_iter().map(|descriptor| descriptor.middleware).collect();
    assert_eq!(order, ["B", "C", "D", "A"]);
}

#[test]
fn cycles_and_bad_constraints_are_rejected() {
    let cycle = Builder::new()
        .use_at(passthrough("A"), PipelineStage::Validate, Placement::End, APP_OWNER)
        .use_at(passthrough("B"), PipelineStage::Validate, Placement::End, APP_OWNER)
        .before("A", "B")
        .before("B", "A");
    assert_code(&cycle, ERROR_CODE_CYCLE);
    assert_eq!(cycle.describe().unwrap_err().message(), "the ordering constraints in stage 'validate' form a cycle.");
    assert_code(&Builder::new().use_at(passthrough("A"), PipelineStage::Validate, Placement::End, APP_OWNER).before("A", "A"), ERROR_CODE_CYCLE);

    let unregistered = Builder::new().use_at(passthrough("A"), PipelineStage::Validate, Placement::End, APP_OWNER).before("A", "Missing");
    assert_code(&unregistered, ERROR_CODE_REGISTRATION);
    assert_eq!(unregistered.describe().unwrap_err().message(), "the ordering constraint references the unregistered middleware 'Missing'.");

    let cross_stage = Builder::new()
        .use_at(passthrough("A"), PipelineStage::Validate, Placement::End, APP_OWNER)
        .use_at(passthrough("B"), PipelineStage::Egress, Placement::End, APP_OWNER)
        .before("A", "B");
    assert_code(&cross_stage, ERROR_CODE_REGISTRATION);

    let duplicate = Builder::new().use_at(passthrough("A"), PipelineStage::Validate, Placement::End, APP_OWNER).use_at(
        passthrough("A"),
        PipelineStage::Observe,
        Placement::End,
        APP_OWNER,
    );
    assert_code(&duplicate, ERROR_CODE_REGISTRATION);
    assert_eq!(duplicate.describe().unwrap_err().message(), "the middleware 'A' is registered more than once.");

    assert_code(&Builder::new().use_at(passthrough("A"), PipelineStage::Validate, Placement::End, ""), ERROR_CODE_REGISTRATION);
    assert_code(&Builder::new().use_at(passthrough(""), PipelineStage::Validate, Placement::End, APP_OWNER), ERROR_CODE_REGISTRATION);
    assert_code(&Builder::new().use_named_instance("", passthrough("A"), PipelineStage::Validate, Placement::End, APP_OWNER), ERROR_CODE_REGISTRATION);

    let error = pipeline::parse_stage("middle").unwrap_err();
    assert_eq!((error.code(), error.message()), (ERROR_CODE_UNKNOWN_STAGE, "'middle' is not a pipeline stage."));
    assert_eq!(pipeline::parse_stage("handle").unwrap(), PipelineStage::Handle);
}

#[test]
fn named_instances_are_distinct_but_ambiguous_for_type_constraints() {
    let log = Log::default();
    let builder = Builder::new()
        .use_named_instance("guard-a", recording("Guard", &log), PipelineStage::Observe, Placement::End, "xihan.plugins.a")
        .use_named_instance("guard-b", recording("Guard", &log), PipelineStage::Observe, Placement::End, "xihan.plugins.b");
    assert_eq!(builder.dump().unwrap(), "observe      Guard[guard-a]\nobserve      Guard[guard-b]\n");
    let builder = builder.use_at(passthrough("Other"), PipelineStage::Observe, Placement::End, APP_OWNER);
    assert_code(&builder.clone().before("Guard", "Other"), ERROR_CODE_REGISTRATION);

    // 按实例名的约束（RFC-0004 排序规则第 7 条）
    let ordered = builder.clone().before("Other", "guard-a").after("guard-a", "guard-b");
    let order: Vec<String> = ordered.describe().unwrap().iter().map(|descriptor| descriptor.display_name()).collect();
    assert_eq!(order, ["Other", "Guard[guard-b]", "Guard[guard-a]"]);
    assert_code(
        &Builder::new().use_named_instance("x", passthrough("Guard"), PipelineStage::Observe, Placement::End, APP_OWNER).use_named_instance(
            "x",
            passthrough("Guard"),
            PipelineStage::Observe,
            Placement::End,
            APP_OWNER,
        ),
        ERROR_CODE_REGISTRATION,
    );
}

#[test]
fn short_circuit_skips_downstream() {
    let log = Log::default();
    let deny = pipeline::from_fn("Deny", |_ctx, _next| Box::pin(async { Err(Error::denied("ORDERS_DENIED_001", "orders.denied")) }));
    let pipeline = Builder::new()
        .use_at(deny, PipelineStage::Authorize, Placement::End, APP_OWNER)
        .use_at(recording("Handle", &log), PipelineStage::Handle, Placement::End, APP_OWNER)
        .build()
        .unwrap();
    let outcome = common::block_on(pipeline.run(&Context::background(), &terminal(&log)));
    assert_eq!(outcome.unwrap_err().code(), "ORDERS_DENIED_001");
    assert!(log.entries().is_empty(), "{:?}", log.entries());
}

#[test]
fn next_can_run_again_and_with_a_derived_context() {
    let log = Log::default();
    let retry = pipeline::from_fn("Retry", |ctx, next| {
        Box::pin(async move {
            let first = next.run(ctx).await;
            if first.is_ok() { first } else { next.run(ctx).await }
        })
    });
    let correlate = pipeline::from_fn("Correlate", |ctx, next| {
        Box::pin(async move {
            let derived = ctx.with_baggage(BAGGAGE_CORRELATION, "c1");
            next.run(&derived).await
        })
    });
    let attempts = Arc::new(Mutex::new(0));
    let terminal = {
        let (log, attempts) = (log.clone(), Arc::clone(&attempts));
        pipeline::step_fn(move |ctx| {
            let (log, attempts) = (log.clone(), Arc::clone(&attempts));
            Box::pin(async move {
                log.push(format!("terminal:{}", ctx.correlation().unwrap_or("-")));
                let mut attempts = attempts.lock().unwrap();
                *attempts += 1;
                if *attempts == 1 { Err(Error::unavailable("ORDERS_UNAVAILABLE_001", "orders.busy")) } else { Ok(()) }
            })
        })
    };
    let pipeline = Builder::new()
        .use_at(retry, PipelineStage::Resilience, Placement::Beginning, APP_OWNER)
        .use_at(correlate, PipelineStage::Observe, Placement::End, APP_OWNER)
        .build()
        .unwrap();
    let ctx = Context::background();
    common::block_on(pipeline.run(&ctx, &terminal)).unwrap();
    assert_eq!(log.entries(), ["terminal:c1", "terminal:c1"]);
    assert_eq!(ctx.correlation(), None, "the derived context does not leak back");

    // 管道与终点可以放进 Arc，跨线程共享
    let shared: Arc<dyn Step> = Arc::new(terminal);
    let pipeline = Arc::new(pipeline);
    std::thread::spawn(move || common::block_on(pipeline.run(&Context::background(), &shared))).join().unwrap().unwrap();
}

/// 约束的主体在声明时必须已登记，目标在构建时解析；按名字登记的实例只认实例名，没有名字的登记认中间件名（RFC-0004 排序规则第 7 条）。
#[test]
fn constraint_names_follow_rule_7() {
    let early = Builder::new().before("A", "B").use_at(passthrough("A"), PipelineStage::Validate, Placement::End, APP_OWNER).use_at(
        passthrough("B"),
        PipelineStage::Validate,
        Placement::End,
        APP_OWNER,
    );
    assert_code(&early, ERROR_CODE_REGISTRATION);
    assert_eq!(early.describe().unwrap_err().message(), "the middleware 'A' is not registered, so no ordering constraint can be declared for it.");

    // 目标可以在约束之后登记
    let late_target = Builder::new().use_at(passthrough("A"), PipelineStage::Validate, Placement::End, APP_OWNER).after("A", "B").use_at(
        passthrough("B"),
        PipelineStage::Validate,
        Placement::End,
        APP_OWNER,
    );
    let order: Vec<String> = late_target.describe().unwrap().into_iter().map(|descriptor| descriptor.middleware).collect();
    assert_eq!(order, ["B", "A"]);

    // 只有一个命名实例时也不能按中间件名约束它
    let named = Builder::new().use_named_instance("guard-a", passthrough("Guard"), PipelineStage::Observe, Placement::End, APP_OWNER).use_at(
        passthrough("Other"),
        PipelineStage::Observe,
        Placement::End,
        APP_OWNER,
    );
    assert_code(&named.clone().before("Guard", "Other"), ERROR_CODE_REGISTRATION);
    assert_code(&named.clone().before("Other", "Guard"), ERROR_CODE_REGISTRATION);
    let order: Vec<String> = named.before("Other", "guard-a").describe().unwrap().iter().map(|descriptor| descriptor.display_name()).collect();
    assert_eq!(order, ["Other", "Guard[guard-a]"]);

    // 实例名与另一个中间件名相同即歧义
    let clash = Builder::new()
        .use_named_instance("Other", passthrough("Guard"), PipelineStage::Observe, Placement::End, APP_OWNER)
        .use_at(passthrough("Other"), PipelineStage::Observe, Placement::End, APP_OWNER)
        .use_at(passthrough("Third"), PipelineStage::Observe, Placement::End, APP_OWNER);
    assert_code(&clash.clone().before("Other", "Third"), ERROR_CODE_REGISTRATION);
    let error = clash.before("Third", "Other").describe().unwrap_err();
    assert_eq!(error.message(), "the middleware name 'Other' matches several registrations, so ordering constraints by name are ambiguous for it.");
}
