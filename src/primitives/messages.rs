use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use serde_json::Value;

use super::framework::invalid_argument;
use crate::grammar::{digit, lower, lower_digit, word};

/// 模板参数，也是错误的结构化细节：名字到 JSON 值，按名字排序。
///
/// 只允许 JSON 可表示的值（RFC-0001）；用 `BTreeMap` 让编码与描述的顺序确定，不受 `serde_json` 的特性开关影响。
pub type Details = BTreeMap<String, Value>;

/// 消息键的最大长度（字节，键只含 ASCII）。
pub const MESSAGE_KEY_MAX_LENGTH: usize = 128;

/// 默认区域，也是消息目录的基准与回退区域（RFC-0010）。
pub const DEFAULT_LOCALE: &str = "en-US";

/// 区域名的最大长度（RFC-0010 `LocaleName.MaxLength`）。
const LOCALE_MAX_LENGTH: usize = 35;

/// 判断消息键是否合法。
///
/// 文法 `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$`，最长 [`MESSAGE_KEY_MAX_LENGTH`]（RFC-0001、RFC-0010）：
/// 首段与后段同形，操作 ID 的资源段把连字符换成下划线后可直接作首段，如 `tenant_members.list.description`。
pub fn is_message_key(key: &str) -> bool {
    let segment = |part: &str| word(part, lower, |byte| lower_digit(byte) || byte == b'_');
    let mut parts = key.split('.');
    key.len() <= MESSAGE_KEY_MAX_LENGTH && parts.next().is_some_and(segment) && {
        // 至少两段：首段之后还要有一段
        let mut rest = parts.peekable();
        rest.peek().is_some() && rest.all(segment)
    }
}

/// 返回消息键的领域段（第一个点之前）；不合法时为 `None`。
pub fn message_key_domain(key: &str) -> Option<&str> {
    if !is_message_key(key) {
        return None;
    }
    key.split('.').next()
}

/// 模板里的一段：字面量（含转义还原的单个花括号），或占位符。
enum Segment<'a> {
    Literal(&'a str),
    Placeholder { name: &'a str, format: Option<&'a str> },
}

/// 按 RFC-0010 的模板语法逐段读取：`{name}` / `{0}` 占位符可带 `:format`，`{{` 与 `}}` 是转义。
///
/// 没有闭合或名字不合法的花括号按字面量处理，与 .NET `MessageTemplate` 一致。
struct Segments<'a> {
    text: &'a str,
    index: usize,
}

impl<'a> Iterator for Segments<'a> {
    type Item = Segment<'a>;

    fn next(&mut self) -> Option<Segment<'a>> {
        let (text, start) = (self.text, self.index);
        let bytes = text.as_bytes();
        let &current = bytes.get(start)?;
        if (current == b'{' || current == b'}') && bytes.get(start + 1) == Some(&current) {
            self.index = start + 2;
            return Some(Segment::Literal(&text[start..=start]));
        }
        if current != b'{' {
            // 落单的 '}' 自成一段字面量；普通字符成段读到下一个花括号，当前字符不是花括号，所以找到的位置总在它之后
            let end = if current == b'}' { start + 1 } else { text[start..].find(['{', '}']).map_or(text.len(), |offset| start + offset) };
            self.index = end;
            return Some(Segment::Literal(&text[start..end]));
        }
        let Some(offset) = text[start + 1..].find('}') else {
            self.index = text.len();
            return Some(Segment::Literal(&text[start..]));
        };
        let end = start + 1 + offset;
        let body = &text[start + 1..end];
        let (name, format) = match body.split_once(':') {
            Some((name, format)) => (name, Some(format)),
            None => (body, None),
        };
        self.index = end + 1;
        if is_placeholder_name(name) {
            Some(Segment::Placeholder { name, format })
        } else {
            // 花括号里不是合法名字（如 JSON 片段）按字面量输出
            Some(Segment::Literal(&text[start..=end]))
        }
    }
}

/// 占位符名文法 `^([A-Za-z][A-Za-z0-9_]*|[0-9]+)$`：按名字或按位置。
fn is_placeholder_name(name: &str) -> bool {
    word(name, |byte| byte.is_ascii_alphabetic(), |byte| byte.is_ascii_alphanumeric() || byte == b'_') || (!name.is_empty() && name.bytes().all(digit))
}

fn segments(template: &str) -> Segments<'_> {
    Segments { text: template, index: 0 }
}

/// 列出模板里的占位符名（不含格式说明符），按出现顺序去重。
pub fn placeholders(template: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for segment in segments(template) {
        if let Segment::Placeholder { name, .. } = segment {
            if !names.iter().any(|known| known == name) {
                names.push(name.to_owned());
            }
        }
    }
    names
}

/// 宽松格式化：用参数填充模板，缺参数的占位符原样保留。
///
/// `{name}` 取命名参数，`{0}` 取位置参数（键 `"0"`、`"1"`…，见 [`positional`]）；`{{`、`}}` 还原为单个花括号。
/// 字符串原样输出，其他值输出 JSON 文本。`:format` 以 `%` 开头时按 printf 风格解释，支持标志 `-`、`+`、`0`、空格，
/// 宽度、精度与动词 `d`、`x`、`X`、`f`、`s`、`v`（如 `{count:%03d}`）；其他写法或类型不合时按默认形式输出。
pub fn format_message(template: &str, arguments: &Details) -> String {
    render(template, arguments, false).unwrap_or_default()
}

/// 严格格式化：任一占位符缺参数即返回 `None`，供消息解析拒绝半截译文（RFC-0010）。
pub fn try_format_message(template: &str, arguments: &Details) -> Option<String> {
    render(template, arguments, true)
}

/// 逐段渲染；严格模式下缺参数返回 `None`。
fn render(template: &str, arguments: &Details, strict: bool) -> Option<String> {
    let mut output = String::with_capacity(template.len() + 16);
    for segment in segments(template) {
        match segment {
            Segment::Literal(text) => output.push_str(text),
            Segment::Placeholder { name, format } => match arguments.get(name) {
                Some(value) => output.push_str(&format_value(value, format)),
                None if strict => return None,
                None => {
                    // 宽松模式把占位符原样留在文本里，让遗漏可见而不是悄悄吞掉
                    output.push('{');
                    output.push_str(name);
                    if let Some(format) = format {
                        output.push(':');
                        output.push_str(format);
                    }
                    output.push('}');
                }
            },
        }
    }
    Some(output)
}

/// 按格式说明符输出一个值。
fn format_value(value: &Value, format: Option<&str>) -> String {
    format.filter(|format| format.starts_with('%')).and_then(|format| printf(format, value)).unwrap_or_else(|| display(value))
}

/// 值的默认文本：字符串原样，其他为 JSON 文本。
pub(crate) fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// printf 风格的一个说明符 `%[flags][width][.precision]verb`；不支持的写法或类型不合时返回 `None`。
fn printf(spec: &str, value: &Value) -> Option<String> {
    let bytes = spec.as_bytes();
    let (mut minus, mut plus, mut zero, mut space) = (false, false, false, false);
    let mut index = 1;
    while let Some(&flag) = bytes.get(index) {
        match flag {
            b'-' => minus = true,
            b'+' => plus = true,
            b'0' => zero = true,
            b' ' => space = true,
            _ => break,
        }
        index += 1;
    }
    let width_start = index;
    while bytes.get(index).is_some_and(|&byte| digit(byte)) {
        index += 1;
    }
    let width: usize = if index > width_start { spec[width_start..index].parse().ok()? } else { 0 };
    let mut precision = None;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let precision_start = index;
        while bytes.get(index).is_some_and(|&byte| digit(byte)) {
            index += 1;
        }
        precision = Some(if index > precision_start { spec[precision_start..index].parse().ok()? } else { 0 });
    }
    // 说明符必须恰好以一个动词结束
    if index + 1 != bytes.len() {
        return None;
    }
    let positive_sign = if plus {
        "+"
    } else if space {
        " "
    } else {
        ""
    };
    let (sign, body, numeric) = match bytes[index] {
        verb @ (b'd' | b'x' | b'X') => {
            let number = value.as_i64().map(i128::from).or_else(|| value.as_u64().map(i128::from))?;
            let magnitude = number.unsigned_abs();
            let body = match verb {
                b'd' => magnitude.to_string(),
                b'x' => format!("{magnitude:x}"),
                _ => format!("{magnitude:X}"),
            };
            (if number < 0 { "-" } else { positive_sign }, body, true)
        }
        b'f' => {
            let number = value.as_f64()?;
            let text = format!("{:.*}", precision.unwrap_or(6), number);
            match text.strip_prefix('-') {
                Some(body) => ("-", body.to_owned(), true),
                None => (positive_sign, text, true),
            }
        }
        b's' | b'v' => {
            let text = display(value);
            let text = match precision {
                Some(limit) => text.chars().take(limit).collect(),
                None => text,
            };
            ("", text, false)
        }
        _ => return None,
    };
    let length = sign.chars().count() + body.chars().count();
    let padding = width.saturating_sub(length);
    Some(if minus {
        format!("{sign}{body}{}", " ".repeat(padding))
    } else if zero && numeric {
        format!("{sign}{}{body}", "0".repeat(padding))
    } else {
        format!("{}{sign}{body}", " ".repeat(padding))
    })
}

/// 把位置参数转成以 `"0"`、`"1"`… 为键的参数表。
pub fn positional<I, V>(values: I) -> Details
where
    I: IntoIterator<Item = V>,
    V: Into<Value>,
{
    values.into_iter().enumerate().map(|(index, value)| (index.to_string(), value.into())).collect()
}

/// 在没有文案时描述一个消息：`键 (k=v, ...)`，参数按名字排序；没有参数时只有键。
pub fn describe(key: &str, arguments: &Details) -> String {
    if arguments.is_empty() {
        return key.to_owned();
    }
    let parts: Vec<String> = arguments.iter().map(|(name, value)| format!("{name}={}", display(value))).collect();
    format!("{key} ({})", parts.join(", "))
}

/// 按区域把消息键解析成已格式化的文案（RFC-0010 `IMessageResolver`）。
pub trait MessageResolver: Send + Sync {
    /// 返回文案；没有该键、没有该区域或占位符缺参数时返回 `None`。
    fn resolve(&self, key: &str, locale: &str, arguments: &Details) -> Option<String>;
}

impl<F> MessageResolver for F
where
    F: Fn(&str, &str, &Details) -> Option<String> + Send + Sync,
{
    fn resolve(&self, key: &str, locale: &str, arguments: &Details) -> Option<String> {
        self(key, locale, arguments)
    }
}

/// 进程默认的解析器与区域（RFC-0001 `MessageScope.Default`）。
struct DefaultMessages {
    resolver: Option<Arc<dyn MessageResolver>>,
    locale: Cow<'static, str>,
}

static DEFAULT_MESSAGES: RwLock<DefaultMessages> = RwLock::new(DefaultMessages { resolver: None, locale: Cow::Borrowed(DEFAULT_LOCALE) });

/// 读默认设置。锁里只有整体赋值，panic 不会留下半改的状态，所以中毒时照常取值
fn read_defaults<T>(read: impl FnOnce(&DefaultMessages) -> T) -> T {
    read(&DEFAULT_MESSAGES.read().unwrap_or_else(PoisonError::into_inner))
}

/// 设置进程默认的消息解析器与区域；[`Error::message`](super::Error::message) 在没有已解析文案时用它。
///
/// `resolver` 为 `None` 时清除解析器；`locale` 为 `None` 时保留当前区域。
///
/// # Panics
///
/// `locale` 不合区域名文法 `^[A-Za-z]{2,3}(-[A-Za-z0-9]{2,8})*$`（最长 35 个字符，RFC-0010）时 panic。
pub fn set_default_messages(resolver: Option<Arc<dyn MessageResolver>>, locale: Option<&str>) {
    if let Some(locale) = locale {
        if !is_locale_name(locale) {
            invalid_argument(format!("The locale '{locale}' must match <language>[-<subtag>...]."));
        }
    }
    let mut defaults = DEFAULT_MESSAGES.write().unwrap_or_else(PoisonError::into_inner);
    defaults.resolver = resolver;
    if let Some(locale) = locale {
        defaults.locale = Cow::Owned(locale.to_owned());
    }
}

/// 返回进程默认区域；没有设置时是 [`DEFAULT_LOCALE`]。
pub fn default_locale() -> String {
    read_defaults(|defaults| defaults.locale.clone().into_owned())
}

/// 用进程默认解析器解析消息键；没有解析器或解析不出时返回 `None`。
pub fn resolve_message(key: &str, arguments: &Details) -> Option<String> {
    let (resolver, locale) = read_defaults(|defaults| (defaults.resolver.clone(), defaults.locale.clone()));
    // 在锁外调用解析器：解析器里再读写默认设置也不会死锁
    resolver?.resolve(key, &locale, arguments)
}

/// 区域名文法 `^[A-Za-z]{2,3}(-[A-Za-z0-9]{2,8})*$`，最长 35 个字符。
fn is_locale_name(locale: &str) -> bool {
    let mut parts = locale.split('-');
    locale.len() <= LOCALE_MAX_LENGTH
        && parts.next().is_some_and(|language| (2..=3).contains(&language.len()) && language.bytes().all(|byte| byte.is_ascii_alphabetic()))
        && parts.all(|subtag| (2..=8).contains(&subtag.len()) && subtag.bytes().all(|byte| byte.is_ascii_alphanumeric()))
}

/// 一份内存消息目录：区域 → 键 → 模板；区域没有该键时回退到 [`DEFAULT_LOCALE`]。
///
/// 格式化是严格的：占位符缺参数时解析不出（[`try_format_message`]），让 [`Error::message`](super::Error::message)
/// 退回到键与参数的描述，而不是给出半截译文。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    locales: BTreeMap<String, BTreeMap<String, String>>,
}

impl Catalog {
    /// 创建空目录。
    pub fn new() -> Self {
        Self::default()
    }

    /// 放入一条模板，返回同一区域同一键原来的模板。
    pub fn insert(&mut self, locale: impl Into<String>, key: impl Into<String>, template: impl Into<String>) -> Option<String> {
        self.locales.entry(locale.into()).or_default().insert(key.into(), template.into())
    }

    /// 放入一条模板并返回目录本身，便于链式构建。
    pub fn with(mut self, locale: impl Into<String>, key: impl Into<String>, template: impl Into<String>) -> Self {
        self.insert(locale, key, template);
        self
    }

    /// 取一条模板，不回退。
    pub fn template(&self, locale: &str, key: &str) -> Option<&str> {
        self.locales.get(locale)?.get(key).map(String::as_str)
    }

    /// 按名字排序列出目录里的区域。
    pub fn locales(&self) -> impl Iterator<Item = &str> {
        self.locales.keys().map(String::as_str)
    }
}

impl MessageResolver for Catalog {
    fn resolve(&self, key: &str, locale: &str, arguments: &Details) -> Option<String> {
        let template = self.template(locale, key).or_else(|| if locale == DEFAULT_LOCALE { None } else { self.template(DEFAULT_LOCALE, key) })?;
        try_format_message(template, arguments)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn arguments(value: Value) -> Details {
        match value {
            Value::Object(map) => map.into_iter().collect(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn message_keys_follow_the_grammar() {
        for key in ["orders.missing", "tenant_members.list.description", "kernel.invocation.deadline_exceeded", "a1.b_2"] {
            assert!(is_message_key(key), "{key}");
        }
        for key in ["", "orders", "Orders.missing", "orders.", ".orders", "orders..missing", "orders.Missing", "1orders.x", "orders.1x", "orders-x.y"] {
            assert!(!is_message_key(key), "{key}");
        }
        assert!(!is_message_key(&format!("a.{}", "b".repeat(MESSAGE_KEY_MAX_LENGTH))));
        assert_eq!(message_key_domain("tenant_members.list"), Some("tenant_members"));
        assert_eq!(message_key_domain("bad"), None);
    }

    #[test]
    fn templates_follow_rfc_0010() {
        let values = arguments(json!({"0": "a", "count": 7, "name": "x", "price": 12.34567, "big": -255}));
        assert_eq!(format_message("{0} of {count:%03d}", &values), "a of 007");
        assert_eq!(format_message("{{name}} is {name}", &values), "{name} is x");
        assert_eq!(format_message("订单 {name} 不存在。}", &values), "订单 x 不存在。}");
        assert_eq!(format_message("}} {", &values), "} {");
        assert_eq!(format_message("{not valid} {_x} {name", &values), "{not valid} {_x} {name");
        assert_eq!(format_message("{price:%.2f}|{price:%8.3f}|{count:%-4d}|{big:%x}|{count:%+d}", &values), "12.35|  12.346|7   |-ff|+7");
        assert_eq!(format_message("{name:%5s}|{name:N2}|{count:%q}", &values), "    x|x|7");
        assert_eq!(format_message("{missing} {missing:N2}", &values), "{missing} {missing:N2}");
        assert_eq!(try_format_message("{missing}", &values), None);
        assert_eq!(try_format_message("{name}", &values).as_deref(), Some("x"));
        assert_eq!(placeholders("{a} {b:N2} {a} {{c}} {0}"), ["a", "b", "0"]);
    }

    #[test]
    fn describe_sorts_arguments() {
        assert_eq!(describe("orders.failed", &Details::new()), "orders.failed");
        assert_eq!(describe("orders.failed", &arguments(json!({"z": true, "a": 1, "m": "x", "n": null}))), "orders.failed (a=1, m=x, n=null, z=true)");
        assert_eq!(positional(["a", "b"]), arguments(json!({"0": "a", "1": "b"})));
    }

    #[test]
    fn catalogs_fall_back_to_en_us_and_reject_missing_arguments() {
        let catalog = Catalog::new().with("en-US", "orders.missing", "Order {id} was not found.").with("zh-CN", "orders.other", "其他");
        let values = arguments(json!({"id": 42}));
        assert_eq!(catalog.resolve("orders.missing", "zh-CN", &values).as_deref(), Some("Order 42 was not found."));
        assert_eq!(catalog.resolve("orders.missing", "en-US", &Details::new()), None);
        assert_eq!(catalog.resolve("orders.unknown", "zh-CN", &values), None);
        assert_eq!(catalog.locales().collect::<Vec<_>>(), ["en-US", "zh-CN"]);
    }

    #[test]
    fn locale_names_follow_the_grammar() {
        for locale in ["en-US", "zh-CN", "zh-Hant-TW", "de"] {
            assert!(is_locale_name(locale), "{locale}");
        }
        for locale in ["", "e", "english", "en_US", "en-", "en-x", &format!("en-{}", "abcdefgh-".repeat(4))] {
            assert!(!is_locale_name(locale), "{locale}");
        }
    }
}
