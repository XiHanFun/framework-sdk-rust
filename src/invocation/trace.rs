use std::fmt;

use crate::grammar::lower_hex;

/// W3C Trace Context 的一段：追踪 ID、Span ID 与采样决定（RFC-0003「追踪」）。
///
/// `traceparent` 只认 `^00-[0-9a-f]{32}-[0-9a-f]{16}-[0-9a-f]{2}$`，追踪 ID 或 Span ID 全零也不合法；是否采样只看标志的最低位，
/// 其余位不进上下文。采样决定为空表示没有上游，这次调用是链路的根（[`TraceContext::generate`]）。写出时版本是 `00`，
/// 标志只写采样位：`00` 或 `01`，采样决定为空时写 `01`，即按已采样传下去，由下游的采样器决定。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TraceContext {
    trace_id: String,
    span_id: String,
    sampled: Option<bool>,
}

impl TraceContext {
    /// 用追踪 ID（32 位小写十六进制）、Span ID（16 位小写十六进制）与采样决定创建；格式不对或全零时为 `None`。
    pub fn new(trace_id: impl Into<String>, span_id: impl Into<String>, sampled: bool) -> Option<Self> {
        Self::with_sampling(trace_id, span_id, Some(sampled))
    }

    /// 同 [`TraceContext::new`]，`sampled` 为 `None` 表示没有上游的采样决定（写出时按已采样）。
    pub fn with_sampling(trace_id: impl Into<String>, span_id: impl Into<String>, sampled: Option<bool>) -> Option<Self> {
        let (trace_id, span_id) = (trace_id.into(), span_id.into());
        (is_id(&trace_id, 32) && is_id(&span_id, 16)).then_some(Self { trace_id, span_id, sampled })
    }

    /// 解析 W3C `traceparent`；不合法时为 `None`，调用方当作没有给出。
    pub fn parse(trace_parent: &str) -> Option<Self> {
        let mut parts = trace_parent.split('-');
        let (version, trace_id, span_id, flags) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        // 只认版本 00：其他版本（含 W3C 定为不合法的 ff）与多余的段都不合法
        if parts.next().is_some() || version != "00" || !is_hex(flags, 2) {
            return None;
        }
        let sampled = u8::from_str_radix(flags, 16).ok()? & 0x01 == 0x01;
        Self::new(trace_id, span_id, sampled)
    }

    /// 生成新的追踪：随机的追踪 ID 与 Span ID，没有上游的采样决定（链路的根）。
    ///
    /// # Panics
    ///
    /// 操作系统的随机数源不可用时 panic。
    pub fn generate() -> Self {
        Self { trace_id: new_trace_id(), span_id: new_span_id(), sampled: None }
    }

    /// 追踪 ID：32 位小写十六进制。
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// Span ID：16 位小写十六进制。
    pub fn span_id(&self) -> &str {
        &self.span_id
    }

    /// 上游的采样决定；没有上游时为 `None`。
    pub fn sampled(&self) -> Option<bool> {
        self.sampled
    }

    /// 是否采样；没有上游的采样决定时按已采样。
    pub fn is_sampled(&self) -> bool {
        self.sampled.unwrap_or(true)
    }

    /// W3C `traceparent`：`00-<trace>-<span>-<00|01>`。
    pub fn trace_parent(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for TraceContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "00-{}-{}-{}", self.trace_id, self.span_id, if self.is_sampled() { "01" } else { "00" })
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
