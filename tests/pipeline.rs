//! 管道：Go SDK `pipeline` 测试的移植、ordering.json 的回放，以及按名字的约束、多次进入下游与派生上下文。

mod common;

use std::sync::{Arc, Mutex};

use serde::Deserialize;
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

fn passthrough(name: impl Into<String>) -> impl Middleware + 'static {
    pipeline::from_fn(name, |ctx, next| Box::pin(async move { next.run(ctx).await }))
}

fn terminal(log: &Log) -> impl Step + 'static {
    let log = log.clone();
    pipeline::step_fn(move |_ctx| {
        log.push("terminal");
        Box::pin(async { Ok(()) })
    })
}

/// 排序时报出的错误：Describe、Dump 与 Build 都报同一个码。
fn assert_sort_error(builder: &Builder, code: &str) -> String {
    let error = builder.describe().expect_err(code);
    assert_eq!(error.code(), code, "{error}");
    assert_eq!(builder.dump().unwrap_err().code(), code);
    assert_eq!(builder.build().unwrap_err().code(), code);
    error.message().to_owned()
}

/// 依次登记同一阶段末尾、归属 app 的中间件。
fn validate_stage(names: &[&'static str]) -> Builder {
    let mut builder = Builder::new();
    for &name in names {
        builder.use_at(passthrough(name), PipelineStage::Validate, Placement::End, APP_OWNER).unwrap();
    }
    builder
}

fn order(builder: &Builder) -> Vec<String> {
    builder.describe().unwrap().into_iter().map(|descriptor| descriptor.display_name()).collect()
}

#[test]
fn order_follows_stage_placement_owner_and_registration() {
    let log = Log::default();
    let mut builder = Builder::new();
    builder
        .use_at(recording("Egress", &log), PipelineStage::Egress, Placement::End, APP_OWNER)
        .unwrap()
        .use_at(recording("ObserveLate", &log), PipelineStage::Observe, Placement::End, "xihan.b")
        .unwrap()
        .use_at(recording("ObserveEarly", &log), PipelineStage::Observe, Placement::Beginning, "xihan.z")
        .unwrap()
        .use_at(recording("ObserveA", &log), PipelineStage::Observe, Placement::End, "xihan.a")
        .unwrap()
        .use_middleware(recording("Transaction", &log))
        .unwrap();
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
    let mut builder = validate_stage(&["A", "B", "C"]);
    builder.before("C", "A").unwrap().after("B", "C").unwrap();
    assert_eq!(order(&builder), ["C", "A", "B"]);

    // 约束只调整必要的相对位置，其余保持基准次序
    let mut builder = validate_stage(&["A", "B", "C", "D"]);
    builder.after("A", "D").unwrap();
    assert_eq!(order(&builder), ["B", "C", "D", "A"]);
}

#[test]
fn sort_errors_are_reported_when_sorting() {
    let mut cycle = validate_stage(&["A", "B"]);
    cycle.before("A", "B").unwrap().before("B", "A").unwrap();
    assert_eq!(assert_sort_error(&cycle, ERROR_CODE_CYCLE), "Middleware ordering constraints in stage 'validate' form a cycle: 'A', 'B'.");
    let mut itself = validate_stage(&["A"]);
    itself.before("A", "A").unwrap();
    assert_eq!(assert_sort_error(&itself, ERROR_CODE_CYCLE), "Middleware ordering constraints in stage 'validate' form a cycle: 'A'.");
    // 成环时列出全部仍被卡住的中间件，含排在环后面的
    let mut downstream = validate_stage(&["A", "B", "C", "D"]);
    downstream.before("B", "C").unwrap().before("C", "B").unwrap().before("C", "D").unwrap();
    assert_eq!(assert_sort_error(&downstream, ERROR_CODE_CYCLE), "Middleware ordering constraints in stage 'validate' form a cycle: 'B', 'C', 'D'.");

    let mut unregistered = validate_stage(&["A"]);
    unregistered.before("A", "Missing").unwrap();
    assert_eq!(
        assert_sort_error(&unregistered, ERROR_CODE_REGISTRATION),
        "The ordering constraint of middleware 'A' references unregistered middleware 'Missing'."
    );

    let mut cross_stage = validate_stage(&["A"]);
    cross_stage.use_at(passthrough("B"), PipelineStage::Egress, Placement::End, APP_OWNER).unwrap().before("A", "B").unwrap();
    assert_eq!(
        assert_sort_error(&cross_stage, ERROR_CODE_REGISTRATION),
        "Middleware 'A' (stage 'validate') and 'B' (stage 'egress') are not in the same stage, so no relative order can be declared between them."
    );

    // 与 .NET 一样逐个阶段处理：前面阶段的成环先于后面阶段的目标未登记
    let mut both = validate_stage(&["A", "B"]);
    both.before("A", "B").unwrap().before("B", "A").unwrap();
    both.use_at(passthrough("H"), PipelineStage::Handle, Placement::End, APP_OWNER).unwrap().before("H", "Missing").unwrap();
    assert_sort_error(&both, ERROR_CODE_CYCLE);
    let mut first = Builder::new();
    first.use_at(passthrough("H"), PipelineStage::Handle, Placement::End, APP_OWNER).unwrap().before("H", "Missing").unwrap();
    first.use_at(passthrough("V"), PipelineStage::Validate, Placement::End, APP_OWNER).unwrap().before("V", "Gone").unwrap();
    assert!(assert_sort_error(&first, ERROR_CODE_REGISTRATION).contains("'V' references unregistered middleware 'Gone'"));
}

/// 登记时立即报出的错误：这一项不生效，构建器照常可用（RFC-0004 排序规则第 8 条）。
#[test]
fn registration_errors_are_reported_immediately() {
    let mut builder = validate_stage(&["A"]);
    let error = builder.use_at(passthrough("A"), PipelineStage::Observe, Placement::End, APP_OWNER).map(|_| ()).unwrap_err();
    assert_eq!(
        (error.code(), error.message()),
        (
            ERROR_CODE_REGISTRATION,
            "Middleware 'A' is already registered; each middleware type may appear only once in a pipeline unless its instances are named."
        )
    );
    assert_eq!(order(&builder), ["A"], "the failed registration is not applied");

    for error in [
        builder.use_at(passthrough("B"), PipelineStage::Validate, Placement::End, "").map(|_| ()).unwrap_err(),
        builder.use_at(passthrough(""), PipelineStage::Validate, Placement::End, APP_OWNER).map(|_| ()).unwrap_err(),
        builder.use_named_instance("", passthrough("B"), PipelineStage::Validate, Placement::End, APP_OWNER).map(|_| ()).unwrap_err(),
        builder.before("Missing", "A").map(|_| ()).unwrap_err(),
        builder.after("Missing", "A").map(|_| ()).unwrap_err(),
    ] {
        assert_eq!(error.code(), ERROR_CODE_REGISTRATION, "{error}");
    }
    assert_eq!(
        builder.before("Missing", "A").map(|_| ()).unwrap_err().message(),
        "Middleware 'Missing' is not registered, so no ordering constraint can be declared for it."
    );
    builder.use_at(passthrough("B"), PipelineStage::Validate, Placement::End, APP_OWNER).unwrap().before("B", "A").unwrap();
    assert_eq!(order(&builder), ["B", "A"], "the builder keeps working after errors");

    let error = pipeline::parse_stage("middle").unwrap_err();
    assert_eq!((error.code(), error.message()), (ERROR_CODE_UNKNOWN_STAGE, "Unknown pipeline stage 'middle'."));
    assert_eq!(pipeline::parse_stage("handle").unwrap(), PipelineStage::Handle);
}

#[test]
fn named_instances_are_distinct_and_constrained_by_instance_name() {
    let log = Log::default();
    let mut builder = Builder::new();
    builder
        .use_named_instance("guard-a", recording("Guard", &log), PipelineStage::Observe, Placement::End, "xihan.plugins.a")
        .unwrap()
        .use_named_instance("guard-b", recording("Guard", &log), PipelineStage::Observe, Placement::End, "xihan.plugins.b")
        .unwrap();
    assert_eq!(builder.dump().unwrap(), "observe      Guard[guard-a]\nobserve      Guard[guard-b]\n");
    builder.use_at(passthrough("Other"), PipelineStage::Observe, Placement::End, APP_OWNER).unwrap();
    // 按名字登记的实例只认实例名（RFC-0004 排序规则第 7 条）
    assert_eq!(builder.clone().before("Guard", "Other").map(|_| ()).unwrap_err().code(), ERROR_CODE_REGISTRATION);
    let mut by_type_target = builder.clone();
    by_type_target.before("Other", "Guard").unwrap();
    assert_sort_error(&by_type_target, ERROR_CODE_REGISTRATION);

    let mut ordered = builder.clone();
    ordered.before("Other", "guard-a").unwrap().after("guard-a", "guard-b").unwrap();
    assert_eq!(order(&ordered), ["Other", "Guard[guard-b]", "Guard[guard-a]"]);
    let duplicate = builder.use_named_instance("guard-a", passthrough("Guard"), PipelineStage::Observe, Placement::End, APP_OWNER);
    assert_eq!(duplicate.map(|_| ()).unwrap_err().code(), ERROR_CODE_REGISTRATION);

    // 实例名与另一个中间件名相同即歧义：主体在声明时报，目标在排序时报
    let mut clash = Builder::new();
    clash
        .use_named_instance("Other", passthrough("Guard"), PipelineStage::Observe, Placement::End, APP_OWNER)
        .unwrap()
        .use_at(passthrough("Other"), PipelineStage::Observe, Placement::End, APP_OWNER)
        .unwrap()
        .use_at(passthrough("Third"), PipelineStage::Observe, Placement::End, APP_OWNER)
        .unwrap();
    assert_eq!(clash.clone().before("Other", "Third").map(|_| ()).unwrap_err().code(), ERROR_CODE_REGISTRATION);
    clash.before("Third", "Other").unwrap();
    assert_eq!(
        assert_sort_error(&clash, ERROR_CODE_REGISTRATION),
        "The middleware name 'Other' matches several registrations, so ordering constraints by name are ambiguous for it."
    );
}

#[test]
fn short_circuit_skips_downstream() {
    let log = Log::default();
    let deny = pipeline::from_fn("Deny", |_ctx, _next| Box::pin(async { Err(Error::denied("ORDERS_DENIED_001", "orders.denied")) }));
    let mut builder = Builder::new();
    builder
        .use_at(deny, PipelineStage::Authorize, Placement::End, APP_OWNER)
        .unwrap()
        .use_at(recording("Handle", &log), PipelineStage::Handle, Placement::End, APP_OWNER)
        .unwrap();
    let pipeline = builder.build().unwrap();
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
    let mut builder = Builder::new();
    builder
        .use_at(retry, PipelineStage::Resilience, Placement::Beginning, APP_OWNER)
        .unwrap()
        .use_at(correlate, PipelineStage::Observe, Placement::End, APP_OWNER)
        .unwrap();
    let pipeline = builder.build().unwrap();
    let ctx = Context::background();
    common::block_on(pipeline.run(&ctx, &terminal)).unwrap();
    assert_eq!(log.entries(), ["terminal:c1", "terminal:c1"]);
    assert_eq!(ctx.correlation(), None, "the derived context does not leak back");

    // 管道与终点可以放进 Arc，跨线程共享
    let shared: Arc<dyn Step> = Arc::new(terminal);
    let pipeline = Arc::new(pipeline);
    std::thread::spawn(move || common::block_on(pipeline.run(&Context::background(), &shared))).join().unwrap().unwrap();
}

#[derive(Deserialize)]
struct OrderingVectors {
    cases: Vec<OrderingCase>,
    errors: Vec<OrderingError>,
}

#[derive(Deserialize)]
struct OrderingCase {
    name: String,
    steps: Vec<OrderingStep>,
    dump: String,
}

#[derive(Deserialize)]
struct OrderingError {
    name: String,
    steps: Vec<OrderingStep>,
    error: String,
    at: serde_json::Value,
    message: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OrderingStep {
    Use {
        #[serde(rename = "use")]
        instance: String,
        stage: String,
        placement: String,
        owner: String,
    },
    Before {
        before: (String, String),
    },
    After {
        after: (String, String),
    },
}

/// 按名字登记的实例用的中间件名，即向量里的 `{type}`。
const PROBE: &str = "Probe";

/// 执行一步；登记失败时返回错误。
fn apply(builder: &mut Builder, step: &OrderingStep) -> Result<(), xihan_framework::primitives::FrameworkError> {
    match step {
        OrderingStep::Use { instance, stage, placement, owner } => {
            let stage = pipeline::parse_stage(stage)?;
            let placement = match placement.as_str() {
                "beginning" => Placement::Beginning,
                "end" => Placement::End,
                other => panic!("unknown placement {other}"),
            };
            builder.use_named_instance(instance, passthrough(PROBE), stage, placement, owner).map(|_| ())
        }
        OrderingStep::Before { before: (subject, target) } => builder.before(subject, target).map(|_| ()),
        OrderingStep::After { after: (subject, target) } => builder.after(subject, target).map(|_| ()),
    }
}

/// 排序、打印与报错时机按 ordering.json（RFC-0004「排序规则」「打印格式」）。
#[test]
fn ordering_follows_the_vectors() {
    let vectors: OrderingVectors = common::vector("pipeline/ordering.json");
    for case in &vectors.cases {
        let mut builder = Builder::new();
        for step in &case.steps {
            apply(&mut builder, step).unwrap_or_else(|error| panic!("{}: {error}", case.name));
        }
        assert_eq!(builder.dump().unwrap(), case.dump.replace("{type}", PROBE), "{}", case.name);
        assert_eq!(builder.build().unwrap().dump(), builder.dump().unwrap());
    }
    for case in &vectors.errors {
        let mut builder = Builder::new();
        let mut failed = None;
        for (index, step) in case.steps.iter().enumerate() {
            if let Err(error) = apply(&mut builder, step) {
                failed = Some((index, error));
                break;
            }
        }
        // 消息是英文原文，逐字相等（ADR-0236）
        let message = case.message.replace("{type}", PROBE);
        match (&case.at, failed) {
            (serde_json::Value::String(at), None) if at == "sort" => {
                assert_eq!(assert_sort_error(&builder, &case.error), message, "{}", case.name);
            }
            (serde_json::Value::Number(at), Some((index, error))) => {
                assert_eq!((Some(index as u64), error.code(), error.message()), (at.as_u64(), case.error.as_str(), message.as_str()), "{}", case.name);
            }
            (at, failed) => panic!("{}: expected the error at {at}, got {:?}", case.name, failed.map(|(index, error)| (index, error.to_string()))),
        }
    }
}
