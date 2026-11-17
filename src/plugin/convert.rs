//! 原语层与协议消息之间的转换：错误与 `Status`（RFC-0001 的编码，`invocation.md` §3）、调用上下文与 `InvocationMetadata`
//! （`invocation.md` §2、RFC-0003）、信封与健康。

use std::collections::HashMap;
use std::time::SystemTime;

use super::messages::{PHASE_INVOKE, REASON_NO_ERROR, failed};
use super::time::{UtcTime, effective_deadline, format_deadline};
use super::types::{Envelope, Health};
use super::v1;
use crate::invocation::{CancellationToken, Context};
use crate::primitives::json::{self, MAX_DEPTH};
use crate::primitives::{Details, Error, is_baggage_key_registered, is_error_code, is_message_key};

/// 错误 → 协议的 `Error`（`invocation.md` §3「写出」）：`code`、`message_key` 原样；带消息键的错误 `text` 留空，由宿主按目录翻译成
/// 调用方的语言（RFC-0010），不按任何区域在这里渲染；只有自由文本错误写 `text`；`details` 是 JSON 对象文本，没有时为空；内部错误递归。
pub(crate) fn to_proto_error(error: &Error) -> v1::Error {
    let text = match error.message_key() {
        Some(_) => String::new(),
        None => error.text().map_or_else(|| error.code().to_owned(), str::to_owned),
    };
    v1::Error {
        code: error.code().to_owned(),
        message_key: error.message_key().unwrap_or_default().to_owned(),
        text,
        details: error.details().filter(|details| !details.is_empty()).and_then(|details| serde_json::to_string(details).ok()).unwrap_or_default(),
        inner: error.inner().map(|inner| Box::new(to_proto_error(inner))),
    }
}

/// 成功的 `Status`。
pub(crate) fn ok_status() -> v1::Status {
    v1::Status { ok: true, error: None }
}

/// 失败的 `Status`。
pub(crate) fn error_status(error: &Error) -> v1::Status {
    v1::Status { ok: false, error: Some(to_proto_error(error)) }
}

/// 结果 → `Status`。
pub(crate) fn status_of(outcome: &crate::primitives::Result<()>) -> v1::Status {
    match outcome {
        Ok(()) => ok_status(),
        Err(error) => error_status(error),
    }
}

/// 宿主回来的 `Status` → 结果（`invocation.md` §3「读入」）；`ok` 为假却没有 `error` 时是 `phase` 的失败：
/// `PLUGINS_UNEXPECTED_001` + `plugins.process.failed`（`reason` 为 `the host reported a failure without an error`）。
pub(crate) fn result_of(status: Option<&v1::Status>, plugin: &str, phase: &str) -> crate::primitives::Result<()> {
    match status {
        Some(status) if status.ok => Ok(()),
        Some(v1::Status { error: Some(error), .. }) => Err(from_proto_error(error, plugin)),
        _ => Err(failed(plugin, phase, REASON_NO_ERROR)),
    }
}

/// 协议的 `Error` → 错误（`invocation.md` §3「读入」，与参考实现 `PluginErrorMapper.FromProto` 同一规则）：
///
/// - `code` 不合错误码文法时整个错误换成 `PLUGINS_UNEXPECTED_001` + `plugins.process.failed`（`phase` 为 `invoke`，`reason` 为
///   `invalid error code '<code>'`）；
/// - `message_key` 合消息键文法的按键还原，否则是自由文本错误，文案取 `text`，`text` 为空时取 `code`；
/// - `details` 为空、通不过共同的 JSON 读取检查或不是对象时当作没有；
/// - `inner` 同此，最多还原 [`MAX_DEPTH`] 层。
pub(crate) fn from_proto_error(error: &v1::Error, plugin: &str) -> Error {
    decode_error(error, plugin, 1)
}

fn decode_error(error: &v1::Error, plugin: &str, depth: usize) -> Error {
    if !is_error_code(&error.code) {
        return failed(plugin, PHASE_INVOKE, format!("invalid error code '{}'", error.code));
    }
    let details = (!error.details.is_empty()).then(|| json::from_str::<Details>(&error.details).ok()).flatten();
    let keyed = is_message_key(&error.message_key).then(|| Error::try_new(error.code.as_str(), error.message_key.as_str()).ok()).flatten();
    let mut decoded = match keyed {
        // 宿主写的文案（合规的宿主对带键的错误不写）留作本地的已解析文案；再写给宿主时带键的错误不写 text
        Some(keyed) => keyed.with_text(error.text.as_str()),
        // 码已核对过、文案非空，构造不会失败；文案为空时用错误码作文案：自由文本错误必须有文案
        None => Error::try_literal(error.code.as_str(), if error.text.is_empty() { error.code.as_str() } else { error.text.as_str() })
            .unwrap_or_else(|_| failed(plugin, PHASE_INVOKE, format!("invalid error code '{}'", error.code))),
    };
    if let Some(details) = details {
        decoded = decoded.with_details(details);
    }
    match &error.inner {
        Some(inner) if depth < MAX_DEPTH => decoded.with_inner(decode_error(inner, plugin, depth + 1)),
        _ => decoded,
    }
}

/// 从 `InvocationMetadata` 还原调用上下文（`invocation.md` §2「接收方还原上下文」、RFC-0003）：
///
/// - 追踪取 `trace_parent`，不合 RFC-0003 的文法即没有父 Span（开始新的追踪）；`trace_state` 不进上下文；
/// - Baggage 只收已登记的键（按键名过滤），已登记键不合值约束的成员按外部输入丢弃；`xihan.principal` 原样交给作者；
/// - 截止时间取 `deadline`（按严格文法读，读不出当作没有）与 gRPC 调用自身截止时间中较早的一个，两者都没有就没有；
/// - `token` 在宿主取消这次 RPC 时触发，是上下文的调用方取消。
pub(crate) fn context_of(metadata: Option<&v1::InvocationMetadata>, grpc_deadline: Option<SystemTime>, token: &CancellationToken) -> Context {
    let mut builder = Context::builder().cancellation(token);
    let mut deadline = grpc_deadline;
    if let Some(metadata) = metadata {
        builder = builder.trace_parent(&metadata.trace_parent);
        deadline = effective_deadline(&metadata.deadline, grpc_deadline);
        // 键先按已登记过滤：ContextBuilder::baggage 对不合 xihan.<name> 的键 panic，载体上的键是外部输入
        for (key, value) in &metadata.baggage {
            if is_baggage_key_registered(key) {
                builder = builder.baggage(key.as_str(), value.as_str());
            }
        }
    }
    if let Some(at) = deadline {
        builder = builder.deadline(at);
    }
    builder.build()
}

/// 调用上下文 → `InvocationMetadata`（回调宿主时，`invocation.md` §5）：当前 Span 的 traceparent、按 §2 写法的截止时间（UTC、七位小数、
/// `+00:00`）、上下文里已登记的 Baggage；`trace_state` 为空（上下文没有它，§2）。
pub(crate) fn metadata_of(ctx: &Context) -> v1::InvocationMetadata {
    let baggage: HashMap<String, String> = ctx.without_unregistered_baggage().baggage().iter().map(|(key, value)| (key.clone(), value.clone())).collect();
    v1::InvocationMetadata {
        trace_parent: ctx.trace_parent(),
        trace_state: String::new(),
        deadline: ctx.deadline().map(|at| format_deadline(UtcTime::from_system_time(at))).unwrap_or_default(),
        baggage,
    }
}

/// 协议的信封 → 信封；空的 `subject` 与 `time` 是没有。
pub(crate) fn envelope_of(envelope: v1::Envelope) -> Envelope {
    let mut decoded = Envelope::new(envelope.topic, envelope.id, envelope.source, envelope.r#type);
    decoded.spec_version = envelope.spec_version;
    decoded.subject = Some(envelope.subject).filter(|subject| !subject.is_empty());
    decoded.time = Some(envelope.time).filter(|time| !time.is_empty());
    decoded.data_content_type = envelope.data_content_type;
    decoded.data = envelope.data;
    decoded.extensions = envelope.extensions.into_iter().collect();
    decoded
}

/// 信封 → 协议的信封；空的 `specversion` 与 `datacontenttype` 写成缺省的 `1.0` 与 `application/json`。
pub(crate) fn proto_envelope(envelope: Envelope) -> v1::Envelope {
    v1::Envelope {
        topic: envelope.topic,
        id: envelope.id,
        source: envelope.source,
        r#type: envelope.event_type,
        spec_version: if envelope.spec_version.is_empty() { "1.0".to_owned() } else { envelope.spec_version },
        subject: envelope.subject.unwrap_or_default(),
        time: envelope.time.unwrap_or_default(),
        data_content_type: if envelope.data_content_type.is_empty() { "application/json".to_owned() } else { envelope.data_content_type },
        data: envelope.data,
        extensions: envelope.extensions.into_iter().collect(),
    }
}

/// 健康 → 协议的 `HealthResponse`。
pub(crate) fn proto_health(health: &Health) -> v1::HealthResponse {
    v1::HealthResponse {
        state: health.state.value(),
        items: health
            .items
            .iter()
            .map(|item| v1::HealthItem { name: item.name.clone(), status: item.status.value(), description: item.description.clone().unwrap_or_default() })
            .collect(),
        error: health.error.as_ref().map(to_proto_error),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::plugin::time::parse_deadline;

    #[test]
    fn errors_are_written_with_keys_and_read_back() {
        let error =
            Error::validation("ECHO_VALIDATION_001", "echo.text.empty").with_detail("length", 0).with_inner(Error::literal("ECHO_UNEXPECTED_001", "inner"));
        let proto = to_proto_error(&error);
        assert_eq!((proto.code.as_str(), proto.message_key.as_str(), proto.details.as_str()), ("ECHO_VALIDATION_001", "echo.text.empty", r#"{"length":0}"#));
        // 带消息键的错误 text 留空，由宿主按目录翻译；自由文本错误写 text
        assert_eq!(proto.text, "");
        let inner = proto.inner.as_deref().unwrap();
        assert_eq!((inner.message_key.as_str(), inner.text.as_str(), inner.details.as_str()), ("", "inner", ""));
        assert_eq!(to_proto_error(&Error::literal("ECHO_UNEXPECTED_001", "x").with_text("y")).text, "y");
        let decoded = from_proto_error(&proto, "xihan.samples.echo");
        assert_eq!((decoded.code(), decoded.message_key(), decoded.text()), ("ECHO_VALIDATION_001", Some("echo.text.empty"), None));
        assert_eq!(decoded.details().unwrap()["length"], json!(0));
        assert_eq!(decoded.inner().unwrap().message(), "inner");
    }

    #[test]
    fn errors_from_the_host_follow_the_reading_rules() {
        let read = |error: v1::Error| from_proto_error(&error, "xihan.acme.echo");
        let details = |error: &Error| serde_json::to_value(error.details()).unwrap();

        // 错误码不合文法：整个错误换掉，phase 为 invoke
        let invalid = read(v1::Error { code: "bad".to_owned(), message_key: "echo.ok".to_owned(), ..Default::default() });
        assert_eq!((invalid.code(), invalid.message_key()), ("PLUGINS_UNEXPECTED_001", Some("plugins.process.failed")));
        assert_eq!(details(&invalid), json!({"id": "xihan.acme.echo", "phase": "invoke", "reason": "invalid error code 'bad'"}));
        // 键不合文法：自由文本错误，文案取 text，text 为空时取 code
        let literal =
            read(v1::Error { code: "ECHO_CONFLICT_001".to_owned(), message_key: "Not A Key".to_owned(), text: "stale".to_owned(), ..Default::default() });
        assert_eq!((literal.message_key(), literal.text()), (None, Some("stale")));
        let bare = read(v1::Error { code: "ECHO_CONFLICT_001".to_owned(), ..Default::default() });
        assert_eq!((bare.message_key(), bare.text()), (None, Some("ECHO_CONFLICT_001")));
        // details 为空、通不过共同读取检查或不是对象：当作没有
        for text in ["", "[1]", "{\"a\":1,\"a\":2}", "{", "null", "\"x\""] {
            let error = read(v1::Error { code: "ECHO_CONFLICT_001".to_owned(), details: text.to_owned(), ..Default::default() });
            assert_eq!(error.details(), None, "{text:?}");
        }
        // 内部错误同此
        let nested = read(v1::Error {
            code: "ECHO_CONFLICT_001".to_owned(),
            inner: Some(Box::new(v1::Error { code: "x".to_owned(), ..Default::default() })),
            ..Default::default()
        });
        assert_eq!(nested.inner().unwrap().code(), "PLUGINS_UNEXPECTED_001");
        // ok 为假却没有 error
        let missing = result_of(Some(&v1::Status { ok: false, error: None }), "xihan.acme.echo", "orders.create").unwrap_err();
        assert_eq!(details(&missing), json!({"id": "xihan.acme.echo", "phase": "orders.create", "reason": "the host reported a failure without an error"}));
    }

    #[test]
    fn metadata_restores_the_context() {
        let token = CancellationToken::new();
        let metadata = v1::InvocationMetadata {
            trace_parent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-00".to_owned(),
            trace_state: "vendor=1".to_owned(),
            deadline: "2099-01-01T08:00:00.1234567+08:00".to_owned(),
            baggage: HashMap::from([
                ("xihan.tenant".to_owned(), "t1".to_owned()),
                ("xihan.correlation".to_owned(), "c\n1".to_owned()),
                ("xihan.unregistered".to_owned(), "x".to_owned()),
                ("Not-A-Key".to_owned(), "x".to_owned()),
            ]),
        };
        let ctx = context_of(Some(&metadata), None, &token);
        assert_eq!((ctx.trace_id(), ctx.span_id(), ctx.trace().sampled()), ("0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331", Some(false)));
        assert_eq!(ctx.deadline(), parse_deadline("2099-01-01T00:00:00.1234567Z").map(UtcTime::to_system_time));
        assert_eq!(ctx.baggage().keys().collect::<Vec<_>>(), ["xihan.tenant"], "unregistered keys and unacceptable values are dropped");
        // 写回：trace_state 为空，截止时间是 UTC、七位小数、+00:00
        let back = metadata_of(&ctx);
        assert_eq!(
            (back.trace_parent.as_str(), back.trace_state.as_str(), back.deadline.as_str()),
            (metadata.trace_parent.as_str(), "", "2099-01-01T00:00:00.1234567+00:00")
        );
        token.cancel();
        assert!(ctx.is_cancelled());

        // 生效的截止时间取较早的一个；读不出的 deadline 当作没有
        let grpc = parse_deadline("2098-01-01T00:00:00Z").map(UtcTime::to_system_time);
        let earlier = context_of(Some(&metadata), grpc, &CancellationToken::new());
        assert_eq!(earlier.deadline(), grpc);
        let fallback = context_of(Some(&v1::InvocationMetadata { deadline: "not a time".to_owned(), ..Default::default() }), grpc, &CancellationToken::new());
        assert_eq!(fallback.deadline(), grpc);
        assert_eq!(context_of(None, None, &CancellationToken::new()).deadline(), None);
        assert_eq!(context_of(None, grpc, &CancellationToken::new()).deadline(), grpc);
    }
}
