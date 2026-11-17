//! 会话令牌（`handshake.md` §4，ADR-0202）：宿主调用 `Plugin` 服务的每个 RPC（含 `Handshake`）都在元数据
//! `xihan-plugin-session` 里带 `XIHAN_PLUGIN_SESSION`，插件按常数时间核对，缺失或不匹配以 `Unauthenticated` 拒绝、不执行。
//! 本机别的进程连上插件端点也冒充不了宿主。

use std::sync::Arc;

use tonic::service::Interceptor;
use tonic::{Request, Status};

use super::SESSION_METADATA_KEY;

/// 服务端拦截器：一元与流式调用都先经过它，令牌不对的调用碰不到生命周期与操作。
#[derive(Clone)]
pub(crate) struct SessionGuard {
    token: Arc<[u8]>,
}

impl SessionGuard {
    /// 以宿主经环境变量下发的令牌创建。
    pub(crate) fn new(token: &str) -> Self {
        Self { token: Arc::from(token.as_bytes()) }
    }

    /// 元数据里恰好有一个与令牌相同的值；没有令牌的守卫一律拒绝。
    fn accepts(&self, request: &Request<()>) -> bool {
        let mut values = request.metadata().get_all(SESSION_METADATA_KEY).iter();
        match (values.next(), values.next()) {
            (Some(value), None) => !self.token.is_empty() && constant_time_eq(value.as_bytes(), &self.token),
            _ => false,
        }
    }
}

impl Interceptor for SessionGuard {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        if self.accepts(&request) { Ok(request) } else { Err(Status::unauthenticated("The plugin session token is missing or invalid.")) }
    }
}

/// 常数时间比较：逐字节短路的比较会从耗时里泄露令牌的前缀；长度不同直接不等，只泄露长度。
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    // black_box 挡住编译器把累积的差异改写成提前返回
    let difference = left.iter().zip(right).fold(0u8, |difference, (a, b)| std::hint::black_box(difference | (a ^ b)));
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(values: &[&str]) -> Request<()> {
        let mut request = Request::new(());
        for value in values {
            request.metadata_mut().append(SESSION_METADATA_KEY, value.parse().unwrap());
        }
        request
    }

    #[test]
    fn only_the_exact_token_passes() {
        let mut guard = SessionGuard::new("token-1");
        assert!(guard.call(request(&["token-1"])).is_ok());
        for values in [&[][..], &["token-2"], &["token-1x"], &["token-"], &["token-1", "token-1"]] {
            assert_eq!(guard.call(request(values)).unwrap_err().code(), tonic::Code::Unauthenticated, "{values:?}");
        }
        assert!(SessionGuard::new("").call(request(&[""])).is_err(), "a guard without a token refuses everything");
        assert!(constant_time_eq(b"abc", b"abc") && !constant_time_eq(b"abc", b"abd") && !constant_time_eq(b"abc", b"ab"));
    }
}
