use std::fmt;

use crate::grammar::lower_hex;

/// W3C trace-flags 的采样位。
const SAMPLED: u8 = 0x01;

/// W3C Trace Context 的一段：追踪 ID、Span ID 与 trace-flags（RFC-0003「追踪」）。
///
/// 标志为空表示没有上游，这次调用是链路的根（[`TraceContext::generate`]）；写出的 `traceparent` 总带两位十六进制的标志，
/// 标志为空时写 `01`，即按已采样传下去，由下游的采样器决定。上游给的标志原样保留，是否采样只看最低位。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TraceContext {
    trace_id: String,
    span_id: String,
    flags: Option<u8>,
}

impl TraceContext {
    /// 用追踪 ID（32 位小写十六进制）、Span ID（16 位小写十六进制）与采样决定创建；格式不对或全零时为 `None`。
    pub fn new(trace_id: impl Into<String>, span_id: impl Into<String>, sampled: bool) -> Option<Self> {
        Self::with_flags(trace_id, span_id, Some(if sampled { SAMPLED } else { 0 }))
    }

    /// 用追踪 ID、Span ID 与 trace-flags 创建；`flags` 为 `None` 表示没有上游的采样决定。格式不对或全零时为 `None`。
    pub fn with_flags(trace_id: impl Into<String>, span_id: impl Into<String>, flags: Option<u8>) -> Option<Self> {
        let (trace_id, span_id) = (trace_id.into(), span_id.into());
        (is_id(&trace_id, 32) && is_id(&span_id, 16)).then_some(Self { trace_id, span_id, flags })
    }

    /// 解析 W3C `traceparent`：`version-traceid-spanid-flags`；不合法时为 `None`。标志的全部位原样保留。
    pub fn parse(trace_parent: &str) -> Option<Self> {
        let mut parts = trace_parent.split('-');
        let (version, trace_id, span_id, flags) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || !is_hex(version, 2) || !is_hex(flags, 2) {
            return None;
        }
        Self::with_flags(trace_id, span_id, Some(u8::from_str_radix(flags, 16).ok()?))
    }

    /// 生成新的追踪：随机的追踪 ID 与 Span ID，没有上游的标志（链路的根）。
    ///
    /// # Panics
    ///
    /// 操作系统的随机数源不可用时 panic。
    pub fn generate() -> Self {
        Self { trace_id: new_trace_id(), span_id: new_span_id(), flags: None }
    }

    /// 追踪 ID：32 位小写十六进制。
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// Span ID：16 位小写十六进制。
    pub fn span_id(&self) -> &str {
        &self.span_id
    }

    /// trace-flags；没有上游时为 `None`。
    pub fn flags(&self) -> Option<u8> {
        self.flags
    }

    /// 是否采样：标志的最低位；没有上游的标志时按已采样。
    pub fn is_sampled(&self) -> bool {
        self.flags.is_none_or(|flags| flags & SAMPLED == SAMPLED)
    }

    /// W3C `traceparent`：`00-<trace>-<span>-<flags>`，标志为空时写 `01`。
    pub fn trace_parent(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for TraceContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "00-{}-{}-{:02x}", self.trace_id, self.span_id, self.flags.unwrap_or(SAMPLED))
    }
}

/// 生成 32 位小写十六进制的追踪 ID。
///
/// # Panics
///
/// 操作系统的随机数源不可用时 panic。
pub fn new_trace_id() -> String {
    random_hex::<16>()
}

/// 生成 16 位小写十六进制的 Span ID。
///
/// # Panics
///
/// 操作系统的随机数源不可用时 panic。
pub fn new_span_id() -> String {
    random_hex::<8>()
}

/// 生成 N 字节随机数的十六进制；全零的 ID 在 W3C 里不合法，碰上就重来。
fn random_hex<const N: usize>() -> String {
    let mut bytes = [0u8; N];
    loop {
        getrandom::fill(&mut bytes).unwrap_or_else(|error| panic!("The system random number generator is unavailable: {error}."));
        if bytes.iter().any(|&byte| byte != 0) {
            break;
        }
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 定长小写十六进制。
fn is_hex(text: &str, length: usize) -> bool {
    text.len() == length && text.bytes().all(lower_hex)
}

/// 定长小写十六进制且不全为零。
fn is_id(text: &str, length: usize) -> bool {
    is_hex(text, length) && text.bytes().any(|byte| byte != b'0')
}
