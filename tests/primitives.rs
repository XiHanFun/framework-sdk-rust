//! 原语层：一致性向量与 Go SDK `primitives` 测试的移植。

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Deserialize;
use serde_json::json;
use xihan_framework::primitives::{
    self, BAGGAGE_CORRELATION, BAGGAGE_TENANT, Catalog, Details, ERROR_CODE_INVALID_ARGUMENT, Error, FrameworkError, LifecycleState, PipelineStage,
    REGISTERED_BAGGAGE_KEYS, Result, Stability, TypedId, WireResult,
};

/// 改进程默认消息设置的测试串行执行，读 `Error::message` 的测试也排在它们之间
static DEFAULT_MESSAGES: Mutex<()> = Mutex::new(());

fn default_messages() -> MutexGuard<'static, ()> {
    DEFAULT_MESSAGES.lock().unwrap_or_else(PoisonError::into_inner)
}

fn details(value: serde_json::Value) -> Details {
    serde_json::from_value(value).unwrap()
}

#[derive(Deserialize)]
struct ErrorCodeVectors {
    valid: Vec<String>,
    invalid: Vec<String>,
}

#[test]
fn error_codes_follow_the_vectors() {
    let vectors: ErrorCodeVectors = common::vector("primitives/error-codes.json");
    for code in &vectors.valid {
        assert!(primitives::is_error_code(code), "{code:?} must be valid");
        assert_eq!(primitives::validate_error_code(code).unwrap(), code);
    }
    for code in &vectors.invalid {
        assert!(!primitives::is_error_code(code), "{code:?} must be invalid");
        assert_eq!(primitives::validate_error_code(code).unwrap_err().code(), ERROR_CODE_INVALID_ARGUMENT);
        assert_eq!(primitives::error_code_kind(code), None);
    }
    assert_eq!(primitives::error_code_kind("PERSISTENCE_CONFLICT_001"), Some("CONFLICT"));
    assert_eq!(primitives::error_code_domain("PERSISTENCE_CONFLICT_001"), Some("PERSISTENCE"));
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BaggageKeyVectors {
    well_formed: Vec<String>,
    ill_formed: Vec<String>,
    registered: Vec<String>,
}

#[test]
fn baggage_keys_follow_the_vectors() {
    let vectors: BaggageKeyVectors = common::vector("primitives/baggage-keys.json");
    for key in &vectors.well_formed {
        assert!(primitives::is_baggage_key_well_formed(key), "{key:?} must be well-formed");
    }
    for key in &vectors.ill_formed {
        assert!(!primitives::is_baggage_key_well_formed(key), "{key:?} must be ill-formed");
    }
    assert_eq!(REGISTERED_BAGGAGE_KEYS, vectors.registered.iter().map(String::as_str).collect::<Vec<_>>());
    for key in &vectors.registered {
        assert!(primitives::is_baggage_key_registered(key), "{key:?} must be registered");
    }
    assert!(!primitives::is_baggage_key_registered("xihan.feature-flag"));
}

#[test]
fn baggage_values_follow_adr_0204() {
    assert!(primitives::is_baggage_value_acceptable("t1"));
    assert!(primitives::is_baggage_value_acceptable(&"租".repeat(primitives::BAGGAGE_MAX_VALUE_LENGTH)));
    assert!(!primitives::is_baggage_value_acceptable(""));
    assert!(!primitives::is_baggage_value_acceptable(&"a".repeat(primitives::BAGGAGE_MAX_VALUE_LENGTH + 1)));
    assert!(!primitives::is_baggage_value_acceptable("a\nforged log line"));
    assert!(!primitives::is_baggage_value_acceptable("a\u{85}b"));
}

#[derive(Deserialize)]
struct LifecycleVectors {
    allowed: Vec<(String, String)>,
}

#[test]
fn lifecycle_transitions_follow_the_vectors() {
    let vectors: LifecycleVectors = common::vector("primitives/lifecycle-transitions.json");
    let allowed: BTreeSet<(String, String)> = vectors.allowed.iter().cloned().collect();
    for (from, to) in &vectors.allowed {
        let from: LifecycleState = from.parse().unwrap();
        let to: LifecycleState = to.parse().unwrap();
        assert!(from.can_transition_to(to), "{from} -> {to} must be allowed");
    }
    for &from in LifecycleState::ALL {
        for &to in LifecycleState::ALL {
            if from.can_transition_to(to) {
                assert!(allowed.contains(&(from.to_string(), to.to_string())), "{from} -> {to} is allowed by the SDK but absent from the vector");
            }
        }
    }
    assert!(LifecycleState::Ready.is_serving() && LifecycleState::Degraded.is_serving() && !LifecycleState::Started.is_serving());
    assert!(LifecycleState::Stopped.is_terminal() && !LifecycleState::Failed.is_terminal());
    assert_eq!((LifecycleState::Declared.value(), LifecycleState::Failed.value()), (0, 8), "state numbers must match the protocol enum");
    assert_eq!(LifecycleState::from_value(4), Some(LifecycleState::Ready));
    assert_eq!(LifecycleState::from_value(9), None);
    assert_eq!(serde_json::to_string(&LifecycleState::Draining).unwrap(), r#""Draining""#);
    assert_eq!(serde_json::from_str::<LifecycleState>(r#""Failed""#).unwrap(), LifecycleState::Failed);
    assert!(serde_json::from_str::<LifecycleState>(r#""failed""#).is_err());
    assert_eq!("Running".parse::<LifecycleState>().unwrap_err().to_string(), "PRIMITIVES_VALIDATION_001: 'Running' is not a lifecycle state.");
}

#[test]
fn pipeline_stages_are_ordered() {
    let names = ["ingress", "observe", "resilience", "authenticate", "tenant", "authorize", "validate", "idempotency", "transaction", "handle", "egress"];
    assert_eq!(PipelineStage::ALL.len(), names.len());
    for (index, (&stage, name)) in PipelineStage::ALL.iter().zip(names).enumerate() {
        assert_eq!(stage.manifest_name(), name, "stage {index}");
        assert_eq!(PipelineStage::parse(name), Some(stage));
        assert_eq!(stage as usize, index);
    }
    assert!(PipelineStage::ALL.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(PipelineStage::parse("Handle"), None, "stage names are lowercase");
    assert_eq!(serde_json::to_string(&PipelineStage::Idempotency).unwrap(), r#""idempotency""#);
}

#[test]
fn error_factories_check_the_kind() {
    let _guard = default_messages();
    let error = Error::validation("ORDERS_VALIDATION_001", "orders.quantity.invalid").with_detail("min", 1);
    assert_eq!((error.kind(), error.domain()), ("VALIDATION", "ORDERS"));
    assert_eq!(error.message(), "orders.quantity.invalid (min=1)");
    assert_eq!(error.to_string(), "ORDERS_VALIDATION_001: orders.quantity.invalid (min=1)");

    let message = common::panic_message(|| {
        Error::not_found("ORDERS_VALIDATION_001", "orders.missing");
    });
    assert!(message.starts_with(ERROR_CODE_INVALID_ARGUMENT), "a mismatched kind must panic with PRIMITIVES_VALIDATION_001: {message}");
    assert!(message.contains("must have the kind NOTFOUND"), "{message}");
    for (code, key) in [("orders", "orders.missing"), ("ORDERS_NOTFOUND_001", "Orders")] {
        assert!(common::panic_message(move || drop(Error::new(code, key))).starts_with(ERROR_CODE_INVALID_ARGUMENT));
        assert_eq!(Error::try_new(code, key).unwrap_err().code(), ERROR_CODE_INVALID_ARGUMENT);
    }
    assert!(common::panic_message(|| drop(Error::literal("ORDERS_NOTFOUND_001", ""))).contains("A literal error needs a text."));
    assert!(common::panic_message(|| drop(error.clone().with_code("bad"))).starts_with(ERROR_CODE_INVALID_ARGUMENT));

    for (factory, kind) in [
        (Error::conflict as fn(&'static str, &'static str) -> Error, "CONFLICT"),
        (Error::denied, "DENIED"),
        (Error::unavailable, "UNAVAILABLE"),
        (Error::timeout, "TIMEOUT"),
        (Error::cancelled, "CANCELLED"),
        (Error::unexpected, "UNEXPECTED"),
    ] {
        let code: &'static str = Box::leak(format!("ORDERS_{kind}_001").into_boxed_str());
        assert_eq!(factory(code, "orders.x").kind(), kind);
    }
    let renamed = error.with_code("ORDERS_CONFLICT_007");
    assert_eq!((renamed.code(), renamed.message_key()), ("ORDERS_CONFLICT_007", Some("orders.quantity.invalid")));
}

#[test]
fn error_messages_resolve_through_the_default_catalog() {
    let _guard = default_messages();
    let catalog = Catalog::new().with("en-US", "orders.missing", "Order {id} was not found.").with("zh-CN", "orders.missing", "订单 {id} 不存在。");
    primitives::set_default_messages(Some(Arc::new(catalog)), Some("zh-CN"));
    let error = Error::not_found("ORDERS_NOTFOUND_001", "orders.missing").with_detail("id", 42);
    let resolved = error.message();
    let literal = error.clone().with_text("literal").message();
    let without_arguments = Error::not_found("ORDERS_NOTFOUND_001", "orders.missing").message();
    let locale = primitives::default_locale();
    primitives::set_default_messages(None, Some(primitives::DEFAULT_LOCALE));

    assert_eq!(resolved, "订单 42 不存在。");
    assert_eq!(literal, "literal", "text wins");
    assert_eq!(without_arguments, "orders.missing", "a template with missing arguments falls back to the description");
    assert_eq!(locale, "zh-CN");
    assert_eq!(primitives::default_locale(), "en-US");
    assert_eq!(error.message(), "orders.missing (id=42)");
    assert!(common::panic_message(|| primitives::set_default_messages(None, Some("zh_CN"))).starts_with(ERROR_CODE_INVALID_ARGUMENT));
    assert_eq!(primitives::format_message("{0} of {count:D3}", &details(json!({"0": "a", "count": 7}))), "a of 007");
    assert_eq!(primitives::format_message("{missing}", &Details::new()), "{missing}", "missing placeholders stay");
}

#[test]
fn error_wire_encoding() {
    let _guard = default_messages();
    let inner = Error::literal("KERNEL_UNEXPECTED_001", "boom");
    let error = Error::unexpected("ORDERS_UNEXPECTED_001", "orders.failed").with_inner(inner).with_detail("id", "o1");
    let data = serde_json::to_string(&error).unwrap();
    let want = r#"{"code":"ORDERS_UNEXPECTED_001","message":"orders.failed (id=o1)","messageKey":"orders.failed","details":{"id":"o1"},"inner":{"code":"KERNEL_UNEXPECTED_001","message":"boom"}}"#;
    assert_eq!(data, want);

    let decoded: Error = serde_json::from_str(&data).unwrap();
    assert_eq!(decoded.code(), "ORDERS_UNEXPECTED_001");
    assert_eq!(decoded.text(), Some("orders.failed (id=o1)"));
    assert_eq!(decoded.message_key(), Some("orders.failed"));
    assert_eq!(decoded.details(), Some(&details(json!({"id": "o1"}))));
    assert_eq!(decoded.inner().map(Error::code), Some("KERNEL_UNEXPECTED_001"));
    assert_eq!(serde_json::to_string(&decoded).unwrap(), want, "a decoded error encodes the same");

    let rejected = serde_json::from_str::<Error>(r#"{"code":"bad"}"#).unwrap_err().to_string();
    assert!(rejected.contains(ERROR_CODE_INVALID_ARGUMENT), "an invalid code on the wire must be rejected: {rejected}");
    let minimal: Error = serde_json::from_str(r#"{"code":"ORDERS_CONFLICT_001","message":"stale","extra":1}"#).unwrap();
    assert_eq!((minimal.text(), minimal.message_key()), (Some("stale"), None), "unknown fields are ignored");
    assert!(serde_json::from_str::<Error>(r#"["ORDERS_CONFLICT_001","stale"]"#).is_err(), "the array form of a struct is rejected");
    assert!(serde_json::from_str::<Error>(r#"{"code":"ORDERS_CONFLICT_001","message":"a","message":"b"}"#).is_err(), "duplicate members are rejected");
    assert_eq!(
        serde_json::to_string(&Error::literal("ORDERS_CONFLICT_001", "stale").with_details(Details::new())).unwrap(),
        r#"{"code":"ORDERS_CONFLICT_001","message":"stale"}"#
    );
}

#[test]
fn result_wire_encoding() {
    let ok: Result<Details> = Ok(details(json!({"id": "o1"})));
    assert_eq!(serde_json::to_string(&WireResult(ok)).unwrap(), r#"{"ok":true,"value":{"id":"o1"}}"#);
    let failed: Result<Details> = Err(Error::literal("ORDERS_CONFLICT_001", "stale"));
    let data = serde_json::to_string(&WireResult(failed)).unwrap();
    assert_eq!(data, r#"{"ok":false,"error":{"code":"ORDERS_CONFLICT_001","message":"stale"}}"#);
    let decoded: WireResult<Details> = serde_json::from_str(&data).unwrap();
    assert_eq!(decoded.into_inner().unwrap_err().code(), "ORDERS_CONFLICT_001");

    // 不带值的成功只写 {"ok":true}（RFC-0001「Result」）
    assert_eq!(serde_json::to_string(&WireResult(Ok(()))).unwrap(), r#"{"ok":true}"#);
    assert!(serde_json::from_str::<WireResult<()>>(r#"{"ok":true}"#).unwrap().0.is_ok());
    assert_eq!(serde_json::from_str::<WireResult<Option<u32>>>(r#"{"ok":true,"value":null}"#).unwrap().0.unwrap(), None);
    assert_eq!(serde_json::to_string(&WireResult(Ok(None::<u32>))).unwrap(), r#"{"ok":true,"value":null}"#);
    assert_eq!(serde_json::from_str::<WireResult<u32>>(r#"{"ok":true,"value":5,"extra":[1]}"#).unwrap().0.unwrap(), 5);
    for (text, fragment) in [
        (r#"{"ok":true}"#, "missing field `value`"),
        (r#"{"value":1}"#, "missing field `ok`"),
        (r#"{"ok":false}"#, "A failed result on the wire needs an error."),
        (r#"{"ok":false,"error":null}"#, "expected an error object"),
        (r#"{"ok":true,"value":null}"#, "must not be null unless the value type is a Maybe"),
        (r#"{"ok":true,"ok":true,"value":1}"#, "duplicate field `ok`"),
        (r#"[true,1]"#, "expected a result object"),
    ] {
        let error = serde_json::from_str::<WireResult<u32>>(text).unwrap_err().to_string();
        assert!(error.contains(fragment), "{text}: {error}");
    }

    let doubled: Result<i32> = Ok(21).map(|value: i32| value * 2);
    assert_eq!(doubled.unwrap(), 42);
    let bound: Result<i32> = Err::<i32, _>(Error::literal("ORDERS_CONFLICT_001", "stale"))
        .and_then(|value: i32| value.checked_mul(2).ok_or_else(|| Error::literal("ORDERS_CONFLICT_001", "overflow")));
    assert!(bound.is_err(), "and_then propagates failures");
}

#[derive(Deserialize)]
struct WireVectors {
    results: WireCases<ResultCase>,
    errors: WireCases<ErrorCase>,
}

#[derive(Deserialize)]
struct WireCases<T> {
    valid: Vec<T>,
    invalid: Vec<T>,
}

#[derive(Deserialize)]
struct ResultCase {
    name: String,
    kind: String,
    json: serde_json::Value,
    #[serde(default)]
    canonical: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ErrorCase {
    name: String,
    json: serde_json::Value,
    #[serde(default)]
    canonical: Option<serde_json::Value>,
}

/// 从线上文本读入再写出，返回写出的 JSON 值；读不出时返回错误文本。
fn round_trip<T: serde::de::DeserializeOwned + serde::Serialize>(json: &serde_json::Value) -> std::result::Result<serde_json::Value, String> {
    let decoded: T = serde_json::from_str(&json.to_string()).map_err(|error| error.to_string())?;
    Ok(serde_json::to_value(&decoded).unwrap())
}

fn round_trip_result(kind: &str, json: &serde_json::Value) -> std::result::Result<serde_json::Value, String> {
    match kind {
        "value" => round_trip::<WireResult<serde_json::Value>>(json),
        "unit" => round_trip::<WireResult<()>>(json),
        "maybe" => round_trip::<WireResult<Option<serde_json::Value>>>(json),
        other => panic!("unknown result kind {other:?}"),
    }
}

#[test]
fn wire_encoding_follows_the_vectors() {
    let _guard = default_messages();
    let vectors: WireVectors = common::vector("primitives/wire-encoding.json");
    for case in &vectors.results.valid {
        let written = round_trip_result(&case.kind, &case.json).unwrap_or_else(|error| panic!("{} must be read: {error}", case.name));
        assert_eq!(&written, case.canonical.as_ref().unwrap_or(&case.json), "{}", case.name);
    }
    for case in &vectors.results.invalid {
        assert!(round_trip_result(&case.kind, &case.json).is_err(), "{} must be rejected", case.name);
    }
    for case in &vectors.errors.valid {
        let written = round_trip::<Error>(&case.json).unwrap_or_else(|error| panic!("{} must be read: {error}", case.name));
        assert_eq!(&written, case.canonical.as_ref().unwrap_or(&case.json), "{}", case.name);
    }
    for case in &vectors.errors.invalid {
        assert!(round_trip::<Error>(&case.json).is_err(), "{} must be rejected", case.name);
    }
}

#[test]
fn maybe_wire_encoding() {
    assert_eq!(serde_json::to_string(&Some("x")).unwrap(), r#""x""#);
    assert_eq!(serde_json::to_string(&None::<&str>).unwrap(), "null");
    assert_eq!(serde_json::from_str::<primitives::Maybe<i32>>("null").unwrap(), None);
    assert_eq!(serde_json::from_str::<primitives::Maybe<i32>>("5").unwrap(), Some(5));
    assert!(None::<i32>.ok_or(Error::literal("ORDERS_NOTFOUND_001", "missing")).is_err(), "none to result is a failure");
}

struct Order;

#[test]
fn typed_ids_encode_as_their_value() {
    let id: TypedId<Order, String> = TypedId::new("o1".to_owned());
    assert_eq!(serde_json::to_string(&id).unwrap(), r#""o1""#);
    let decoded: TypedId<Order, String> = serde_json::from_str(r#""o1""#).unwrap();
    assert_eq!(decoded, id);
    assert_eq!(decoded.value(), "o1");
    let numeric: TypedId<Order, i64> = serde_json::from_str("42").unwrap();
    assert_eq!((numeric.to_string(), numeric.into_value()), ("42".to_owned(), 42));
}

#[test]
fn stability_names() {
    for name in ["stable", "preview", "experimental", "internal", "deprecated"] {
        let stability: Stability = name.parse().unwrap();
        assert_eq!(stability.to_string(), name);
    }
    assert_eq!(serde_json::to_string(&Stability::Experimental).unwrap(), r#""experimental""#);
    assert!(serde_json::from_str::<Stability>(r#""solid""#).unwrap_err().to_string().contains("'solid' is not a stability."));
}

#[test]
fn framework_errors_convert_to_literal_errors() {
    let error = FrameworkError::new("KERNEL_PIPELINE_001", "the constraints form a cycle.").with_cause(std::io::Error::other("disk"));
    assert_eq!(error.to_string(), "KERNEL_PIPELINE_001: the constraints form a cycle. (disk)");
    assert!(std::error::Error::source(&error).is_some());
    let converted = error.to_error();
    assert_eq!((converted.code(), converted.text(), converted.message_key()), ("KERNEL_PIPELINE_001", Some("the constraints form a cycle."), None));
    assert_eq!(FrameworkError::new("bad", "x").to_error().code(), ERROR_CODE_INVALID_ARGUMENT);
    assert_eq!(primitives::SDK_NAME, "framework-sdk-rust");
    assert_eq!(primitives::SDK_VERSION, env!("CARGO_PKG_VERSION"));
    assert_eq!(BAGGAGE_TENANT, "xihan.tenant");
    assert_eq!(BAGGAGE_CORRELATION, "xihan.correlation");
}

#[derive(Deserialize)]
struct TemplateVectors {
    keys: KeyCases,
    format: Vec<FormatCase>,
    describe: Vec<DescribeCase>,
}

#[derive(Deserialize)]
struct KeyCases {
    valid: Vec<String>,
    invalid: Vec<String>,
}

#[derive(Deserialize)]
struct FormatCase {
    name: String,
    template: String,
    arguments: Option<Details>,
    lenient: String,
    strict: Option<String>,
}

#[derive(Deserialize)]
struct DescribeCase {
    key: String,
    arguments: Option<Details>,
    text: String,
}

#[test]
fn message_templates_follow_the_vectors() {
    let vectors: TemplateVectors = common::vector("primitives/message-templates.json");
    for key in &vectors.keys.valid {
        assert!(primitives::is_message_key(key), "{key:?} must be valid");
    }
    for key in &vectors.keys.invalid {
        assert!(!primitives::is_message_key(key), "{key:?} must be invalid");
    }
    for case in &vectors.format {
        let arguments = case.arguments.clone().unwrap_or_default();
        assert_eq!(primitives::format_message(&case.template, &arguments), case.lenient, "lenient: {}", case.name);
        assert_eq!(primitives::try_format_message(&case.template, &arguments), case.strict, "strict: {}", case.name);
    }
    for case in &vectors.describe {
        assert_eq!(primitives::describe(&case.key, &case.arguments.clone().unwrap_or_default()), case.text, "{}", case.key);
    }
}
