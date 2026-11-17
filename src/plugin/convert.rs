//! 原语层与协议消息之间的转换：错误与 `Status`（RFC-0001 的编码，`invocation.md` §3）、调用上下文与 `InvocationMetadata`
//! （`invocation.md` §2、RFC-0003）、信封与健康。

use std::collections::HashMap;
use std::time::SystemTime;

use super::messages::failed;
use super::time::{format_rfc3339, parse_rfc3339};
use super::types::{Envelope, Health};
use super::v1;
use crate::invocation::{CancellationToken, Context};
use crate::primitives::json::{self, MAX_DEPTH};
use crate::primitives::{Details, Error, is_baggage_key_registered, is_error_code, is_message_key};

/// 对象袋里上游的 W3C `tracestate`：上下文没有这个字段，回调时原样带回。
#[derive(Clone)]
pub(crate) struct TraceState(pub(crate) String);

/// 错误 → 协议的 `Error`：`text` 是已解析的文案（线上编码的 `message`），`details` 是 JSON 对象文本，内部错误递归。
pub(crate) fn to_proto_error(error: &Error) -> v1::Error {
    v1::Error {
        code: error.code().to_owned(),
        message_key: error.message_key().unwrap_or_default().to_owned(),
        text: error.message(),
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

/// 宿主回来的 `Status` → 结果；失败而没有错误时是阶段失败。
pub(crate) fn result_of(status: Option<&v1::Status>, plugin: &str, phase: &str) -> crate::primitives::Result<()> {
    match status {
        Some(status) if status.ok => Ok(()),
        Some(v1::Status { error: Some(error), .. }) => Err(from_proto_error(error, plugin, phase)),
        _ => Err(failed(plugin, phase, "the host reported a failure without an error")),
    }
}

/// 协议的 `Error` → 错误：有合法的消息键按键还原（文案保留为已解析文案），否则按文案字面还原；`details` 不是 JSON 对象时没有细节；
/// 错误码不合法时整体是阶段失败。内部错误最多还原 [`MAX_DEPTH`] 层。
pub(crate) fn from_proto_error(error: &v1::Error, plugin: &str, phase: &str) -> Error {
    decode_error(error, plugin, phase, 1)
}

fn decode_error(error: &v1::Error, plugin: &str, phase: &str, depth: usize) -> Error {
    if !is_error_code(&error.code) {
        return failed(plugin, phase, format!("the host returned an invalid error code '{}'", error.code));
    }
    let details = (!error.details.is_empty()).then(|| json::from_str::<Details>(&error.details).ok()).flatten();
    let keyed =
        (!error.message_key.is_empty() && is_message_key(&error.message_key)).then(|| Error::try_new(error.code.as_str(), error.message_key.as_str()).ok());
    let mut decoded = match keyed.flatten() {
        Some(keyed) => keyed.with_text(error.text.as_str()),
        // 码已核对过，文案为空时用错误码作文案：字面错误必须有文案
        None => Error::try_literal(error.code.as_str(), if error.text.is_empty() { error.code.as_str() } else { error.text.as_str() })
            .unwrap_or_else(|_| failed(plugin, phase, "the host returned an error that cannot be decoded")),
    };
    if let Some(details) = details {
        decoded = decoded.with_details(details);
    }
    match &error.inner {
        Some(inner) if depth < MAX_DEPTH => decoded.with_inner(decode_error(inner, plugin, phase, depth + 1)),
        _ => decoded,
    }
}

/// 从 `InvocationMetadata` 还原调用上下文（`invocation.md` §2、RFC-0003）：
///
/// - 追踪取 `trace_parent`（不合法时开始新的追踪，规则 7），`trace_state` 放进对象袋供回调带回；
/// - 截止时间取 `deadline`（RFC 3339），没有或不合法时取 gRPC 调用自己的截止时间；
/// - Baggage 只收已登记的键（规则 3），值不合约束的成员丢弃；
/// - `token` 在宿主取消这次 RPC 时触发，是上下文的调用方取消。
pub(crate) fn context_of(metadata: Option<&v1::InvocationMetadata>, grpc_deadline: Option<SystemTime>, token: &CancellationToken) -> Context {
    let mut builder = Context::builder().cancellation(token);
    let mut deadline = grpc_deadline;
    let mut trace_state = None;
    if let Some(metadata) = metadata {
        builder = builder.trace_parent(&metadata.trace_parent);
        if let Some(at) = parse_rfc3339(&metadata.deadline) {
            deadline = Some(at);
        }
        if !metadata.trace_state.is_empty() {
            trace_state = Some(TraceState(metadata.trace_state.clone()));
        }
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
    let ctx = builder.build();
    if let Some(trace_state) = trace_state {
        ctx.insert_item(trace_state);
    }
    ctx
}

/// 调用上下文 → `InvocationMetadata`（回调宿主时）：追踪、截止时间与已登记的 Baggage（RFC-0003 规则 3）。
pub(crate) fn metadata_of(ctx: &Context) -> v1::InvocationMetadata {
    let baggage: HashMap<String, String> = ctx.without_unregistered_baggage().baggage().iter().map(|(key, value)| (key.clone(), value.clone())).collect();
    v1::InvocationMetadata {
        trace_parent: ctx.trace_parent(),
        trace_state: ctx.item::<TraceState>().map(|state| state.0.clone()).unwrap_or_default(),
        deadline: ctx.deadline().map(format_rfc3339).unwrap_or_default(),
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

    #[test]
    fn errors_round_trip_through_the_protocol() {
        let error =
            Error::validation("ECHO_VALIDATION_001", "echo.text.empty").with_detail("length", 0).with_inner(Error::literal("ECHO_UNEXPECTED_001", "inner"));
        let proto = to_proto_error(&error);
        assert_eq!((proto.code.as_str(), proto.message_key.as_str(), proto.details.as_str()), ("ECHO_VALIDATION_001", "echo.text.empty", r#"{"length":0}"#));
        assert_eq!(proto.text, "echo.text.empty (length=0)");
        let decoded = from_proto_error(&proto, "xihan.samples.echo", "invoke");
        assert_eq!(
            (decoded.code(), decoded.message_key(), decoded.text()),
            ("ECHO_VALIDATION_001", Some("echo.text.empty"), Some("echo.text.empty (length=0)"))
        );
        assert_eq!(decoded.details().unwrap()["length"], json!(0));
        assert_eq!(decoded.inner().unwrap().message(), "inner");

        let invalid = from_proto_error(&v1::Error { code: "bad".to_owned(), ..Default::default() }, "xihan.samples.echo", "Host.Invoke");
        assert_eq!((invalid.code(), invalid.message_key()), ("PLUGINS_UNEXPECTED_001", Some("plugins.process.failed")));
        let literal = from_proto_error(&v1::Error { code: "ECHO_CONFLICT_001".to_owned(), details: "[1]".to_owned(), ..Default::default() }, "p", "x");
        assert_eq!((literal.message_key(), literal.text(), literal.details()), (None, Some("ECHO_CONFLICT_001"), None));
    }

    #[test]
    fn metadata_restores_the_context() {
        let token = CancellationToken::new();
        let metadata = v1::InvocationMetadata {
            trace_parent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-00".to_owned(),
            trace_state: "vendor=1".to_owned(),
            deadline: "2099-01-01T00:00:00.000Z".to_owned(),
            baggage: HashMap::from([
                ("xihan.tenant".to_owned(), "t1".to_owned()),
                ("xihan.correlation".to_owned(), "c\n1".to_owned()),
                ("xihan.unregistered".to_owned(), "x".to_owned()),
                ("Not-A-Key".to_owned(), "x".to_owned()),
            ]),
        };
        let ctx = context_of(Some(&metadata), None, &token);
        assert_eq!((ctx.trace_id(), ctx.span_id(), ctx.trace().sampled()), ("0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331", Some(false)));
        assert_eq!(ctx.deadline(), parse_rfc3339("2099-01-01T00:00:00Z"));
        assert_eq!(ctx.baggage().keys().collect::<Vec<_>>(), ["xihan.tenant"], "unregistered keys and unacceptable values are dropped");
        let back = metadata_of(&ctx);
        assert_eq!(
            (back.trace_parent.as_str(), back.trace_state.as_str(), back.deadline.as_str()),
            (metadata.trace_parent.as_str(), "vendor=1", "2099-01-01T00:00:00.000Z")
        );
        token.cancel();
        assert!(ctx.is_cancelled());

        let grpc = parse_rfc3339("2098-01-01T00:00:00Z");
        let fallback = context_of(Some(&v1::InvocationMetadata { deadline: "not a time".to_owned(), ..Default::default() }), grpc, &CancellationToken::new());
        assert_eq!(fallback.deadline(), grpc);
        assert_eq!(context_of(None, None, &CancellationToken::new()).deadline(), None);
    }
}
