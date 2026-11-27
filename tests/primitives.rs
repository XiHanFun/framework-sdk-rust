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
    // 孤立的代理项（RFC-0001「孤立的代理项」，ADR-0321）：Rust 的字符串组不出它，判定不用改；成对的代理项是一个字符，照收
    assert!(!vectors.values.acceptable_utf16.is_empty() && !vectors.values.unacceptable_utf16.is_empty());
    for case in &vectors.values.acceptable_utf16 {
        let value = case.text().unwrap_or_else(|error| panic!("{}: {error}", case.name));
        assert!(primitives::is_baggage_value_acceptable(&value), "{}: {value:?} must be acceptable", case.name);
    }
    for case in &vectors.values.unacceptable_utf16 {
        assert!(case.text().is_err(), "{}: a lone surrogate cannot be a Rust string", case.name);
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

/// 套 `depth` 层数组，最里面是 0。
fn nested(depth: usize) -> serde_json::Value {
    (0..depth).fold(json!(0), |value, _| json!([value]))
}

/// 线上编码写不出时 `serde_json` 报的错误：带 `PRIMITIVES_VALIDATION_001`，不交出文本。
fn assert_not_written<T: serde::Serialize>(what: &str, value: &T) {
    match serde_json::to_string(value) {
        Ok(text) => panic!("{what}: must not be written, got {text}"),
        Err(error) => assert!(error.to_string().contains(ERROR_CODE_INVALID_ARGUMENT), "{what}: {error}"),
    }
}

#[test]
fn wire_encoding_that_is_not_json_text_is_not_written() {
    // RFC-0001「写出与转交」（ADR-0320）：Rust 的字符串总是合法的 UTF-8，写不出的是嵌套超过 64 层（从结果或错误的根算起）与
    // RawValue 原样写进去的重复成员、孤立代理项的转义；写出的整段文本过共同检查
    let raw = |text: &str| serde_json::value::RawValue::from_string(text.to_owned()).unwrap();
    // 错误单独写是 64 层（根、细节、62 层数组），放进结果里多一层
    let deep_error = Error::literal("ORDERS_CONFLICT_001", "stale").with_detail("deep", nested(62));
    assert!(serde_json::to_string(&deep_error).is_ok(), "the error alone is 64 levels deep");
    assert_not_written("an error 65 levels from its root", &Error::literal("ORDERS_CONFLICT_001", "stale").with_detail("deep", nested(63)));
    assert_not_written("an inner error 65 levels deep", &Error::literal("ORDERS_CONFLICT_001", "stale").with_inner(deep_error.clone()));
    assert_not_written("an error 65 levels from the root of the result", &WireResult::<u32>(Err(deep_error.clone())));
    assert_not_written("a value 65 levels from the root", &WireResult(Ok(nested(64))));
    assert_not_written("raw duplicate members", &WireResult(Ok(raw(r#"{"a":1,"a":2}"#))));
    assert_not_written("a raw lone surrogate escape", &WireResult(Ok(raw(r#"["\udc00"]"#))));
    assert_not_written("a raw value too deep", &WireResult(Ok(vec![raw(&format!("{}0{}", "[".repeat(63), "]".repeat(63)))])));

    let deepest = format!("{}0{}", "[".repeat(63), "]".repeat(63));
    assert_eq!(serde_json::to_string(&WireResult(Ok(nested(63)))).unwrap(), format!(r#"{{"ok":true,"value":{deepest}}}"#));
    assert_eq!(serde_json::to_string(&WireResult(Ok(raw(r#""😀""#)))).unwrap(), r#"{"ok":true,"value":"😀"}"#);
    // 嵌在结构体里的结果照样核对，层数从结果的根算起
    #[derive(serde::Serialize)]
    struct Envelope {
        #[serde(with = "primitives::wire_result")]
        outcome: Result<serde_json::Value>,
    }
    assert_not_written("a result in a struct", &Envelope { outcome: Ok(nested(64)) });
    assert!(serde_json::to_string(&Envelope { outcome: Ok(nested(63)) }).is_ok());
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

/// `json::to_vec` 是类型化的写法（RFC-0011 规则 2 与规则 10「编码」），调用器编码响应用的就是它：非有限的浮点与序列化不支持的值
/// 写不出、报错，64 位整数与单精度照原样，读回来是同一个值。
#[test]
fn typed_values_are_written_strictly() {
    #[derive(Debug, PartialEq, Deserialize, serde::Serialize)]
    struct Line {
        sku: String,
        quantity: u64,
        price: f64,
    }
    let line = |price: f64| Line { sku: "a\"b".to_owned(), quantity: 9_007_199_254_740_993, price };
    let text = json::to_vec(&line(2.0)).unwrap();
    assert_eq!(String::from_utf8(text.clone()).unwrap(), r#"{"sku":"a\"b","quantity":9007199254740993,"price":2.0}"#);
    assert_eq!(json::from_slice::<Line>(&text).unwrap(), line(2.0));
    for unwritable in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(json::to_vec(&line(unwritable)).is_err(), "{unwritable} cannot be written");
        // 值模型的写法（json::Model）把它写成字符串，不是这里的事
        assert!(serde_json::to_string(&json::Model(&unwritable)).unwrap().starts_with('"'));
    }
    assert_eq!(json::to_vec(&0.1f32).unwrap(), b"0.1", "single precision is written as it is");
    assert_eq!(json::to_vec("text").unwrap(), b"\"text\"", "unsized values are written too");
    assert!(json::to_vec(&std::collections::BTreeMap::from([((1, 2), 3)])).is_err(), "a map whose keys are not strings cannot be written");
}

/// 两次写出同一个成员名的自定义 `Serialize`。
struct Twice;

impl serde::Serialize for Twice {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("a", &1)?;
        map.serialize_entry("a", &2)?;
        map.end()
    }
}

/// `#[serde(flatten)]` 的映射键可以与字段同名。
#[derive(serde::Serialize)]
struct Flattened {
    a: u32,
    #[serde(flatten)]
    extra: std::collections::BTreeMap<String, u32>,
}

/// 原样交出 `text` 的 `RawValue`。
fn raw_value(text: &str) -> Box<serde_json::value::RawValue> {
    serde_json::value::RawValue::from_string(text.to_owned()).unwrap()
}

/// `json::to_vec` 写出的整段文本过 RFC-0001「JSON 文本」的共同检查（ADR-0340，与调用器编码响应相同）：嵌套超过 64 层（从值的根算起，
/// `RawValue` 也算）、任何一层有重复的成员名、`RawValue` 里孤立代理项的转义都写不出，返回带 `PRIMITIVES_VALIDATION_001` 的错误；
/// 恰好 64 层照常写出。`Host::invoke_json` 与 `Envelope::with_json_data` 经它写出
#[test]
fn typed_values_that_fail_the_json_text_check_are_not_written() {
    let unwritable: [(&str, std::result::Result<Vec<u8>, serde_json::Error>); 7] = [
        ("65 levels", json::to_vec(&nested(65))),
        ("65 levels through a raw value", json::to_vec(&vec![raw_value(&format!("{}0{}", "[".repeat(64), "]".repeat(64)))])),
        ("a custom serializer writing a name twice", json::to_vec(&Twice)),
        ("a duplicate nested in an array", json::to_vec(&vec![std::collections::BTreeMap::from([("b", Twice)])])),
        ("a flattened key equal to a field", json::to_vec(&Flattened { a: 1, extra: std::collections::BTreeMap::from([("a".to_owned(), 2)]) })),
        ("raw duplicate members", json::to_vec(&raw_value(r#"{"a":1,"a":2}"#))),
        ("a raw lone surrogate escape", json::to_vec(&raw_value(r#"{"a":"\ud800"}"#))),
    ];
    for (what, written) in unwritable {
        match written {
            Ok(text) => panic!("{what}: must not be written, got {}", String::from_utf8_lossy(&text)),
            Err(error) => assert!(error.to_string().contains(ERROR_CODE_INVALID_ARGUMENT), "{what}: {error}"),
        }
    }
    let deepest = format!("{}0{}", "[".repeat(64), "]".repeat(64));
    assert_eq!(String::from_utf8(json::to_vec(&nested(64)).unwrap()).unwrap(), deepest, "64 levels are written");
    assert_eq!(json::to_vec(&Flattened { a: 1, extra: std::collections::BTreeMap::from([("b".to_owned(), 2)]) }).unwrap(), br#"{"a":1,"b":2}"#);
    assert_eq!(json::to_vec(&raw_value(r#"{"t":"\ud83d\ude00"}"#)).unwrap(), br#"{"t":"\ud83d\ude00"}"#, "a surrogate pair escape is text");
}

thread_local! {
    /// [`Counted`] 在这个线程上被写出的次数。
    static WRITES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 每写出一次就记一次的值：看得出线上编码在写出之前有没有先在缓冲里多写一遍。
struct Counted;

impl serde::Serialize for Counted {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        WRITES.set(WRITES.get() + 1);
        serializer.serialize_str("counted")
    }
}

/// 经 `json::to_vec` 写出 `WireResult` 与 `Error` 时只写一遍（ADR-0340）：以前每次顶层写出先在缓冲里写一遍过共同检查、再写出；
/// 现在由 `to_vec` 写完之后核对整段文本。交给别的序列化器时照旧先核对，两条路径对同一个值的结果相同：写得出的字节相同，写不出的都报错
#[test]
fn wire_encoding_through_to_vec_is_written_once_with_the_same_result() {
    // 失败的结果写出 Error::message，按进程默认的消息设置解析：与改设置的测试串行，两条路径写出的文案才相同
    let _guard = default_messages();
    let writes = |write: &dyn Fn()| {
        WRITES.set(0);
        write();
        WRITES.get()
    };
    assert_eq!(writes(&|| drop(serde_json::to_vec(&WireResult(Ok(Counted))).unwrap())), 2, "another serializer checks in a buffer first");
    assert_eq!(writes(&|| drop(json::to_vec(&WireResult(Ok(Counted))).unwrap())), 1, "json::to_vec writes once");
    assert_eq!(writes(&|| drop(json::to_vec(&vec![WireResult(Ok(Counted))]).unwrap())), 1, "a nested result is written once too");

    let raw = |text: &str| raw_value(text);
    let deep_error = Error::literal("ORDERS_CONFLICT_001", "stale").with_detail("deep", nested(62));
    #[derive(serde::Serialize)]
    struct Envelope {
        #[serde(with = "primitives::wire_result")]
        outcome: Result<serde_json::Value>,
    }
    // 同一个值经两条路径写出的结果：(值, json::to_vec, serde_json::to_vec)
    let same = |what: &str, by_to_vec: std::result::Result<Vec<u8>, serde_json::Error>, by_serde: std::result::Result<Vec<u8>, serde_json::Error>| match (
        by_to_vec, by_serde,
    ) {
        (Ok(left), Ok(right)) => assert_eq!(left, right, "{what}"),
        (Err(left), Err(right)) => {
            assert!(left.to_string().contains(ERROR_CODE_INVALID_ARGUMENT), "{what}: {left}");
            assert!(right.to_string().contains(ERROR_CODE_INVALID_ARGUMENT), "{what}: {right}");
        }
        (left, right) => panic!("{what}: json::to_vec gave {left:?}, serde_json gave {right:?}"),
    };
    macro_rules! both {
        ($what:expr, $value:expr) => {{
            let value = $value;
            same($what, json::to_vec(&value), serde_json::to_vec(&value));
        }};
    }
    both!("a value", WireResult(Ok(json!({"id": "o1", "n": 2.0, "f": f64::NAN}))));
    both!("a unit success", WireResult::<()>(Ok(())));
    both!("a failure", WireResult::<u32>(Err(Error::new("ORDERS_NOTFOUND_001", "orders.missing").with_detail("id", "o1"))));
    both!("a value 64 levels from the root", WireResult(Ok(nested(63))));
    both!("a value 65 levels from the root", WireResult(Ok(nested(64))));
    both!("an error 64 levels deep", deep_error.clone());
    both!("an error 65 levels deep", Error::literal("ORDERS_CONFLICT_001", "stale").with_detail("deep", nested(63)));
    both!("an error 65 levels from the root of the result", WireResult::<u32>(Err(deep_error.clone())));
    both!("an inner error 65 levels deep", Error::literal("ORDERS_CONFLICT_001", "stale").with_inner(deep_error.clone()));
    both!("raw duplicate members", WireResult(Ok(raw(r#"{"a":1,"a":2}"#))));
    both!("a raw lone surrogate escape", WireResult(Ok(raw(r#"["\udc00"]"#))));
    both!("a raw value too deep", WireResult(Ok(vec![raw(&format!("{}0{}", "[".repeat(63), "]".repeat(63)))])));
    both!("a duplicate inside the value", WireResult(Ok(Twice)));
    both!("a result in a struct", Envelope { outcome: Ok(nested(62)) });
    both!("a result in a struct too deep from its own root", Envelope { outcome: Ok(nested(64)) });
    // json::to_vec 的层数从整个值的根算起：结果自己是 64 层、外面再包一层的，别的序列化器照写，json::to_vec 写不出
    let wrapped = Envelope { outcome: Ok(nested(63)) };
    assert!(serde_json::to_vec(&wrapped).is_ok(), "the result alone is 64 levels deep");
    assert!(json::to_vec(&wrapped).is_err(), "json::to_vec counts from the root of the whole value");
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
