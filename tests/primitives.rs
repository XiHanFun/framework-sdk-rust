//! 原语层（RFC-0001、RFC-0010）：回放 `primitives/` 下的一致性向量，以及错误、结果、标识、Baggage 与消息原语的单元测试。

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Deserialize;
use serde_json::json;
use xihan_framework::primitives::{
    self, BAGGAGE_CORRELATION, BAGGAGE_TENANT, Catalog, Details, ERROR_CODE_INVALID_ARGUMENT, Error, FrameworkError, LifecycleState, MessageResolver,
    PipelineStage, REGISTERED_BAGGAGE_KEYS, Result, Stability, TypedId, WireResult, json,
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
    values: common::BaggageValues,
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
    for value in &vectors.values.acceptable {
        assert!(primitives::is_baggage_value_acceptable(value), "{value:?} must be acceptable");
    }
    for value in &vectors.values.unacceptable {
        assert!(!primitives::is_baggage_value_acceptable(value), "{value:?} must be unacceptable");
    }
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
    assert!(
        common::panic_message(|| drop(Error::literal("ORDERS_NOTFOUND_001", ""))).contains("A literal error needs a text that is not empty or whitespace.")
    );
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

/// Literal 与 `with_text` 的自由文本不能为空或只有空白（RFC-0001「空白字符」的 25 个码位）：两者都报参数非法，`literal` 与 `with_text`
/// panic，`try_literal` 与 `try_with_text` 返回 `PRIMITIVES_VALIDATION_001`；U+001C 与 U+FEFF 不是空白，照常接受。
#[test]
fn free_text_must_not_be_empty_or_whitespace() {
    let _guard = default_messages();
    let every_whitespace: String =
        ['\u{09}', '\u{0A}', '\u{0B}', '\u{0C}', '\u{0D}', ' ', '\u{85}', '\u{A0}', '\u{1680}', '\u{2028}', '\u{2029}', '\u{202F}', '\u{205F}', '\u{3000}']
            .into_iter()
            .chain('\u{2000}'..='\u{200A}')
            .collect();
    assert_eq!(every_whitespace.chars().count(), 25);
    let keyed = Error::validation("ORDERS_VALIDATION_001", "orders.quantity.invalid");
    for text in ["", " ", "\u{A0}", every_whitespace.as_str(), "\u{85}"] {
        let rejected = Error::try_literal("ORDERS_VALIDATION_001", text).expect_err("a blank literal text must be rejected");
        assert_eq!(rejected.code(), ERROR_CODE_INVALID_ARGUMENT, "{text:?}");
        let message = common::panic_message(move || drop(Error::literal("ORDERS_VALIDATION_001", text)));
        assert!(message.contains("A literal error needs a text that is not empty or whitespace."), "{message}");
        let rejected = keyed.clone().try_with_text(text).expect_err("a blank text must be rejected");
        assert_eq!(rejected.code(), ERROR_CODE_INVALID_ARGUMENT, "{text:?}");
        let error = keyed.clone();
        let message = common::panic_message(move || drop(error.with_text(text)));
        assert!(message.starts_with(ERROR_CODE_INVALID_ARGUMENT), "{message}");
        assert!(message.contains("The text of an error must not be empty or whitespace."), "{message}");
    }
    for text in ["\u{1C}", "\u{FEFF}"] {
        assert_eq!(Error::literal("ORDERS_VALIDATION_001", text).text(), Some(text));
        assert_eq!(keyed.clone().with_text(text).text(), Some(text));
        assert_eq!(keyed.clone().try_with_text(text).unwrap().text(), Some(text));
    }
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

/// 目录的读取与合并（RFC-0010）：`Catalog::from_json` 每种区域一份「消息键 → 模板」的 JSON，先过共同的读取检查；不是这样的对象、
/// 键重复、区域名或消息键不合文法都是 `LOCALIZATION_VALIDATION_002`。`extend` 并入另一份目录，同一区域同一键以并入的为准。
#[test]
fn catalogs_are_read_from_json_and_merged() {
    let english = r#"{"orders.missing":"Order {id} was not found.","orders.empty":"No orders."}"#;
    let base = Catalog::from_json([("en-US", english), ("zh-CN", r#"{"orders.missing":"订单 {id} 不存在。"}"#)]).unwrap();
    assert_eq!(base.locales().collect::<Vec<_>>(), ["en-US", "zh-CN"]);
    assert_eq!(base.templates("en-US").collect::<Vec<_>>(), [("orders.empty", "No orders."), ("orders.missing", "Order {id} was not found.")]);
    assert_eq!(base.templates("fr-FR").count(), 0, "a locale the catalog does not have is empty");
    let by_hand = Catalog::new().with("en-US", "orders.missing", "Order {id} was not found.").with("en-US", "orders.empty", "No orders.").with(
        "zh-CN",
        "orders.missing",
        "订单 {id} 不存在。",
    );
    assert_eq!(base, by_hand);

    for (name, files) in [
        ("not an object", vec![("en-US", "[]")]),
        ("a template that is not a string", vec![("en-US", r#"{"orders.missing":1}"#)]),
        ("a duplicate key", vec![("en-US", r#"{"orders.missing":"a","orders.missing":"b"}"#)]),
        ("not JSON", vec![("en-US", "{")]),
        ("an invalid locale name", vec![("en_US", r#"{"orders.missing":"a"}"#)]),
        ("an invalid message key", vec![("en-US", r#"{"Orders.missing":"a"}"#)]),
        ("an invalid file after a valid one", vec![("en-US", english), ("zh-CN", "null")]),
    ] {
        let error = Catalog::from_json(files).unwrap_err();
        assert_eq!(error.code(), primitives::ERROR_CODE_INVALID_CATALOG, "{name}: {error}");
    }
    // 原语层的目录不要求有 en-US：那是握手交给宿主的目录的要求（plugin::MessageCatalog）
    assert_eq!(Catalog::from_json([("zh-CN", r#"{"orders.missing":"订单"}"#)]).unwrap().locales().collect::<Vec<_>>(), ["zh-CN"]);
    // 同一区域给了两份：键取并集，同名的以后一份为准
    let twice = Catalog::from_json([("en-US", english), ("en-US", r#"{"orders.empty":"Nothing."}"#)]).unwrap();
    assert_eq!((twice.template("en-US", "orders.empty"), twice.template("en-US", "orders.missing").is_some()), (Some("Nothing."), true));

    let mut merged = base.clone();
    merged.extend(Catalog::new().with("zh-CN", "orders.missing", "没有订单 {id}。").with("ja-JP", "orders.empty", "注文はありません。"));
    assert_eq!(merged.template("zh-CN", "orders.missing"), Some("没有订单 {id}。"), "the merged-in template wins");
    assert_eq!(merged.template("en-US", "orders.missing"), Some("Order {id} was not found."), "what the other catalog lacks is kept");
    assert_eq!(merged.locales().collect::<Vec<_>>(), ["en-US", "ja-JP", "zh-CN"]);
    assert_eq!(merged.resolve("orders.empty", "ja-JP", &Details::new()).as_deref(), Some("注文はありません。"));
    assert_eq!(base.template("zh-CN", "orders.missing"), Some("订单 {id} 不存在。"), "the source of a clone is untouched");
    merged.extend(Catalog::new());
    assert_eq!(merged.locales().count(), 3, "an empty catalog adds nothing");
}

/// 内置的 10 种区域、区域名文法与规范的空白（RFC-0010、RFC-0001「空白字符」）是原语层的公开项，不打开 feature `plugin` 也能用；
/// `set_default_messages` 对不合文法的区域名 panic，外来的区域名先用 `is_locale_name` 判断。
#[test]
fn locales_and_whitespace_belong_to_the_primitives() {
    let _guard = default_messages();
    assert_eq!(primitives::LOCALES, ["en-US", "zh-CN", "zh-TW", "ja-JP", "ko-KR", "de-DE", "es-ES", "fr-FR", "pt-BR", "ru-RU"]);
    assert_eq!(primitives::LOCALES[0], primitives::DEFAULT_LOCALE, "the fallback locale comes first");
    assert!(primitives::LOCALES.iter().all(|locale| primitives::is_locale_name(locale)));
    for locale in ["en-US", "zh-Hant-TW", "de", "EN-us"] {
        assert!(primitives::is_locale_name(locale), "{locale}");
    }
    for locale in ["", "e", "english", "en_US", "en-", "en-x", "zh CN"] {
        assert!(!primitives::is_locale_name(locale), "{locale:?}");
        let message = common::panic_message(move || primitives::set_default_messages(None, Some(locale)));
        assert!(message.starts_with(ERROR_CODE_INVALID_ARGUMENT), "{locale:?}: {message}");
    }

    // 规范的空白是写死的 25 个码位，逐个核对全部 Unicode 标量值
    let whitespace: Vec<char> = (0..=u32::from(char::MAX)).filter_map(char::from_u32).filter(|char| primitives::is_whitespace(*char)).collect();
    let expected: Vec<char> = ('\u{09}'..='\u{0D}')
        .chain([' ', '\u{85}', '\u{A0}', '\u{1680}'])
        .chain('\u{2000}'..='\u{200A}')
        .chain(['\u{2028}', '\u{2029}', '\u{202F}', '\u{205F}', '\u{3000}'])
        .collect();
    assert_eq!((whitespace.len(), &whitespace), (25, &expected));
    for char in ['\u{1C}', '\u{1F}', '\u{180E}', '\u{200B}', '\u{FEFF}'] {
        assert!(!primitives::is_whitespace(char), "{char:?} is whitespace for Unicode or for some languages, not for the spec");
    }
    for blank in ["", " ", "\t\r\n", "\u{A0}\u{3000}"] {
        assert!(primitives::is_blank(blank), "{blank:?}");
    }
    for text in ["a", " a ", "\u{200B}", "\u{FEFF} "] {
        assert!(!primitives::is_blank(text), "{text:?}");
    }
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
    duplicates: Vec<DuplicateCase>,
}

#[derive(Deserialize)]
struct DuplicateCase {
    name: String,
    target: String,
    #[serde(default)]
    kind: Option<String>,
    text: String,
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

/// 从线上文本读入（先过共同的 JSON 读取检查）再写出，返回写出的 JSON 值；读不出时返回错误文本。
fn round_trip<T: serde::de::DeserializeOwned + serde::Serialize>(json: &serde_json::Value) -> std::result::Result<serde_json::Value, String> {
    let decoded: T = json::from_str(&json.to_string()).map_err(|error| error.to_string())?;
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
    // 任何一层的重复成员都不合法，含值、细节与内部错误里面（RFC-0001「Result」，ADR-0235、ADR-0236）；JSON 值表达不了，按原文读入
    for case in &vectors.duplicates {
        let rejected = match (case.target.as_str(), case.kind.as_deref()) {
            ("result", Some("value")) => json::from_str::<WireResult<serde_json::Value>>(&case.text).is_err(),
            ("result", Some("unit")) => json::from_str::<WireResult<()>>(&case.text).is_err(),
            ("result", Some("maybe")) => json::from_str::<WireResult<Option<serde_json::Value>>>(&case.text).is_err(),
            ("error", None) => json::from_str::<Error>(&case.text).is_err(),
            other => panic!("unknown duplicate case {other:?}"),
        };
        assert!(rejected, "{} must be rejected", case.name);
    }
}

/// 值按模型读写（RFC-0001「Result」，ADR-0236）：非有限的浮点写成字符串，单精度换成双精度，-0 读成整数 0，整数值的浮点仍是浮点。
#[test]
fn wire_values_follow_the_value_model() {
    let text = serde_json::to_string(&WireResult::<Vec<f64>>(Ok(vec![f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 2.0, -0.0]))).unwrap();
    assert_eq!(text, r#"{"ok":true,"value":["NaN","Infinity","-Infinity",2.0,-0.0]}"#);
    assert_eq!(serde_json::to_string(&WireResult::<f32>(Ok(0.1))).unwrap(), r#"{"ok":true,"value":0.10000000149011612}"#);
    let read: WireResult<serde_json::Value> = json::from_str(r#"{"ok":true,"value":[9007199254740993,-0,2.0,1e17]}"#).unwrap();
    assert_eq!(serde_json::to_string(&read).unwrap(), r#"{"ok":true,"value":[9007199254740993,0,2.0,1e+17]}"#);
    assert_eq!(json::to_value(&[f64::NAN]).unwrap(), json!(["NaN"]));
    // 对象位置只收对象：Error 写成数组即不合法
    assert!(json::from_str::<Error>(r#"["KERNEL_UNEXPECTED_001","x"]"#).is_err());
}

/// 任意 JSON（json::Json）照原文保留数字：从本 SDK 的读取与从 serde_json 读都一样，写出照原文，也能作线上结果的值。
#[test]
fn any_json_keeps_the_numbers() {
    let text = r#"{"big":1e400,"exact":9007199254740993,"huge":123456789012345678901234567890,"float":2.0}"#;
    let ours: json::Json = json::from_str(text).unwrap();
    let theirs: json::Json = serde_json::from_str(&format!(" {text} ")).unwrap();
    assert_eq!((ours.as_str(), theirs.as_str()), (text, text));
    assert_eq!(serde_json::to_string(&ours).unwrap(), text);
    assert_eq!(serde_json::to_string(&WireResult::<json::Json>(Ok(ours.clone()))).unwrap(), format!(r#"{{"ok":true,"value":{text}}}"#));
    let read: WireResult<json::Json> = json::from_str(&format!(r#"{{"ok":true,"value":{text}}}"#)).unwrap();
    assert_eq!(read.into_inner().unwrap(), ours);
    // null 是一个值；接受 null 表示没有时用 Option
    assert_eq!(json::from_str::<json::Json>("null").unwrap().as_str(), "null");
    assert_eq!(json::from_str::<Option<json::Json>>("null").unwrap(), None);
    // 共同读取检查照样适用
    assert_eq!(json::Json::from_text(r#"{"a":1,"a":2}"#).unwrap_err().path(), "$.a");
    assert!(serde_json::from_str::<json::Json>(r#"{"a":1,"a":2}"#).is_err());
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
    // as_str 与 name 是同一个访问器，原语层的三个枚举也有
    assert_eq!((Stability::Preview.as_str(), PipelineStage::Egress.as_str(), LifecycleState::Ready.as_str()), ("preview", "egress", "Ready"));
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
    // 说明只有空白时与空串一样用错误码作文案（RFC-0001「空白字符」）
    assert_eq!(FrameworkError::new("KERNEL_PIPELINE_001", " \u{3000}").to_error().text(), Some("KERNEL_PIPELINE_001"));
    assert_eq!(primitives::SDK_NAME, "framework-sdk-rust");
    assert_eq!(primitives::SDK_VERSION, env!("CARGO_PKG_VERSION"));
    assert_eq!(BAGGAGE_TENANT, "xihan.tenant");
    assert_eq!(BAGGAGE_CORRELATION, "xihan.correlation");
}

/// 配置期的 `FrameworkError` 可以在返回 `primitives::Result` 的函数里用 `?`：转换同 `to_error`，带上消息键、参数与已解析的文案。
#[test]
fn framework_errors_propagate_with_the_question_mark() {
    fn configure(error: FrameworkError) -> Result<()> {
        Err(error)?;
        Ok(())
    }
    let keyed = FrameworkError::with_message("KERNEL_OPERATION_002", "kernel.operation.registry_full", details(json!({"capacity": 10000})));
    let expected = keyed.to_error();
    let converted = configure(keyed).unwrap_err();
    assert_eq!(converted, expected);
    assert_eq!((converted.code(), converted.message_key()), ("KERNEL_OPERATION_002", Some("kernel.operation.registry_full")));
    assert_eq!(converted.text(), Some("The operation registry has reached its capacity of 10000."));
    // 没有消息键的是自由文本错误；底层错误不随之带上，错误码不合文法的换成 PRIMITIVES_VALIDATION_001
    let literal = Error::from(FrameworkError::new("KERNEL_PIPELINE_001", "The constraints form a cycle.").with_cause(std::io::Error::other("disk")));
    assert_eq!(
        (literal.code(), literal.message_key(), literal.text(), literal.inner()),
        ("KERNEL_PIPELINE_001", None, Some("The constraints form a cycle."), None)
    );
    assert_eq!(Error::from(FrameworkError::new("bad", "x")).code(), ERROR_CODE_INVALID_ARGUMENT);

    // Error 是全等关系：可以作映射的键的一部分、放进要求 Eq 的容器
    fn assert_eq_type<T: Eq>() {}
    assert_eq_type::<Error>();
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
    #[serde(default)]
    locale: Option<String>,
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
        // 带区域时按区域取小数点与分组符（ADR-0236），没有时按不变文化
        let (lenient, strict) = match &case.locale {
            Some(locale) => {
                (primitives::format_message_in(&case.template, &arguments, locale), primitives::try_format_message_in(&case.template, &arguments, locale))
            }
            None => (primitives::format_message(&case.template, &arguments), primitives::try_format_message(&case.template, &arguments)),
        };
        assert_eq!(lenient, case.lenient, "lenient: {}", case.name);
        assert_eq!(strict, case.strict, "strict: {}", case.name);
    }
    for case in &vectors.describe {
        assert_eq!(primitives::describe(&case.key, &case.arguments.clone().unwrap_or_default()), case.text, "{}", case.key);
    }
}

/// SDK 自带的 en-US 模板与向量逐字相同，参数与模板的占位符一一对应。
#[test]
fn builtin_templates_follow_kernel_messages() {
    let _guard = default_messages();
    let messages = common::kernel_messages();
    let catalog = primitives::builtin_catalog();
    assert_eq!(catalog.locales().collect::<Vec<_>>(), [primitives::DEFAULT_LOCALE]);
    for message in &messages {
        assert!(primitives::is_error_code(&message.code) && primitives::is_message_key(&message.key), "{}", message.key);
        assert_eq!(catalog.template(primitives::DEFAULT_LOCALE, &message.key), Some(message.template.as_str()), "{}", message.key);
        assert_eq!(primitives::placeholders(&message.template), message.arguments, "{}", message.key);
        // 没有进程默认解析器时 Error::message 用自带的模板
        let arguments: Details = message.arguments.iter().map(|name| (name.clone(), json!(format!("<{name}>")))).collect();
        let error = Error::new(message.code.as_str(), message.key.as_str()).with_details(arguments.clone());
        assert_eq!(error.message(), primitives::format_message(&message.template, &arguments), "{}", message.key);
    }
    assert_eq!(
        catalog.template(primitives::DEFAULT_LOCALE, primitives::MESSAGE_UNINITIALIZED_RESULT),
        Some("The result is uninitialized: a default value is not a valid result.")
    );
    assert_eq!(
        Error::new("KERNEL_NOTFOUND_001", "kernel.operation.unknown").message(),
        "kernel.operation.unknown",
        "missing arguments fall back to the description"
    );
}

#[test]
fn framework_errors_carry_message_keys() {
    let error = FrameworkError::with_message("KERNEL_OPERATION_002", "kernel.operation.registry_full", details(json!({"capacity": 10000})));
    assert_eq!(
        (error.code(), error.message(), error.message_key()),
        ("KERNEL_OPERATION_002", "The operation registry has reached its capacity of 10000.", Some("kernel.operation.registry_full"))
    );
    let converted = error.to_error();
    assert_eq!(
        (converted.code(), converted.message_key(), converted.text()),
        ("KERNEL_OPERATION_002", Some("kernel.operation.registry_full"), Some("The operation registry has reached its capacity of 10000."))
    );
    assert_eq!(converted.details(), Some(&details(json!({"capacity": 10000}))));
    let unknown = FrameworkError::with_message("ORDERS_CONFLICT_001", "orders.duplicate", details(json!({"id": "o1"})));
    assert_eq!(unknown.message(), "orders.duplicate (id=o1)");
}

#[derive(Deserialize)]
struct JsonTextVectors {
    cases: Vec<JsonTextCase>,
}

#[derive(Deserialize)]
struct JsonTextCase {
    name: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    hex: Option<String>,
    expect: String,
    #[serde(default)]
    path: Option<String>,
}

/// 共同的 JSON 读取检查按 json-text.json（RFC-0001「JSON 文本」）：text 按 UTF-8 编码，hex 是原始字节。
#[test]
fn json_text_follows_the_vectors() {
    let vectors: JsonTextVectors = common::vector("primitives/json-text.json");
    for case in &vectors.cases {
        let bytes = match (&case.text, &case.hex) {
            (Some(text), None) => text.clone().into_bytes(),
            (None, Some(hex)) => (0..hex.len()).step_by(2).map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap()).collect(),
            _ => panic!("{} must give text or hex", case.name),
        };
        let expected = match case.expect.as_str() {
            "valid" => Ok(()),
            "syntax" => Err(json::ReadError::Syntax),
            "duplicate" => Err(json::ReadError::Duplicate { path: case.path.clone().expect("a duplicate case has a path") }),
            other => panic!("unknown expectation {other:?}"),
        };
        assert_eq!(json::check(&bytes), expected, "{}", case.name);
        assert_eq!(json::check(&bytes).err().map(|error| error.path().to_owned()), expected.err().map(|error| error.path().to_owned()), "{}", case.name);
    }
}
