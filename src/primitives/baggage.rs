use crate::grammar::kebab;

/// 当前租户 ID。
pub const BAGGAGE_TENANT: &str = "xihan.tenant";
/// 当前主体 ID。
pub const BAGGAGE_PRINCIPAL: &str = "xihan.principal";
/// 业务关联 ID。
pub const BAGGAGE_CORRELATION: &str = "xihan.correlation";
/// 幂等键。
pub const BAGGAGE_IDEMPOTENCY_KEY: &str = "xihan.idempotency-key";
/// 请求文化（BCP 47）。
pub const BAGGAGE_LOCALE: &str = "xihan.locale";

/// 已登记的键（`conventions/baggage-keys.md`），按登记顺序。
pub const REGISTERED_BAGGAGE_KEYS: &[&str] = &[BAGGAGE_TENANT, BAGGAGE_PRINCIPAL, BAGGAGE_CORRELATION, BAGGAGE_IDEMPOTENCY_KEY, BAGGAGE_LOCALE];

/// 已登记键的值的最大长度（UTF-16 码元，与 .NET `BaggageKeys.MaxValueLength` 一致）。
pub const BAGGAGE_MAX_VALUE_LENGTH: usize = 256;

/// 判断键是否符合 `xihan.<name>`：小写，连字符分隔，文法 `^xihan\.[a-z][a-z0-9]*(-[a-z0-9]+)*$`。
pub fn is_baggage_key_well_formed(key: &str) -> bool {
    key.strip_prefix("xihan.").is_some_and(kebab)
}

/// 判断键是否已登记。
pub fn is_baggage_key_registered(key: &str) -> bool {
    REGISTERED_BAGGAGE_KEYS.contains(&key)
}

/// 判断值是否可以进入调用上下文：非空、按 UTF-16 码元计不超过 [`BAGGAGE_MAX_VALUE_LENGTH`]、不含控制字符
/// U+0000–U+001F 与 U+007F–U+009F（`conventions/baggage-keys.md`，ADR-0204、ADR-0233）。
///
/// 这些值会写进日志、审计与消息信封，换行能伪造日志行。[`invocation::Context`](crate::invocation::Context) 在构建与派生时
/// 丢弃不合的已登记键成员（幂等键除外，它由 Idempotency 阶段以校验错误拒绝），上下文里只有通过的值。
pub fn is_baggage_value_acceptable(value: &str) -> bool {
    !value.is_empty() && value.encode_utf16().count() <= BAGGAGE_MAX_VALUE_LENGTH && !value.chars().any(char::is_control)
}
