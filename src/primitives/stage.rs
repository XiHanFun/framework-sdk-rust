use super::named::named_enum;

named_enum! {
    /// 管道的十一个阶段（RFC-0004），按执行顺序；名称与顺序是规范性内容，名字即清单里的小写阶段名。
    pub enum PipelineStage ("pipeline stage") {
        /// 协议消息归一化、大小限制、构造上下文。
        Ingress = 0 => "ingress",
        /// Trace、日志、指标。
        Observe = 1 => "observe",
        /// 超时、限流、并发上限、熔断。
        Resilience = 2 => "resilience",
        /// 主体解析。
        Authenticate = 3 => "authenticate",
        /// 租户解析与隔离上下文。
        Tenant = 4 => "tenant",
        /// 策略判定。
        Authorize = 5 => "authorize",
        /// 契约校验。
        Validate = 6 => "validate",
        /// 幂等键去重。
        Idempotency = 7 => "idempotency",
        /// 工作单元。
        Transaction = 8 => "transaction",
        /// 调用操作。
        Handle = 9 => "handle",
        /// 结果与错误翻译、响应信封、写回传播字段。
        Egress = 10 => "egress",
    }
}

impl PipelineStage {
    /// 清单里的阶段名，同 [`PipelineStage::name`]。
    pub const fn manifest_name(self) -> &'static str {
        self.name()
    }
}
