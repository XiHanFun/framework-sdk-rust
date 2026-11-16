use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use serde_json::Value;

use super::framework::invalid_argument;
use super::json;
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

/// 宽松格式化：用参数填充模板，缺参数的占位符连同格式说明符原样保留（RFC-0010「模板」），数字按不变文化。
///
/// `{name}` 取命名参数，`{0}` 取位置参数（键 `"0"`、`"1"`…，见 [`positional`]）；`{{`、`}}` 还原为单个花括号；参数存在而值为
/// `null` 不算缺值，写成空串。值的文本形式与格式说明符见 [`format_value`]。
pub fn format_message(template: &str, arguments: &Details) -> String {
    render(template, arguments, false, INVARIANT).unwrap_or_default()
}

/// 严格格式化：任一占位符缺参数即返回 `None`，供消息解析拒绝半截译文（RFC-0010），数字按不变文化。
pub fn try_format_message(template: &str, arguments: &Details) -> Option<String> {
    render(template, arguments, true, INVARIANT)
}

/// 按区域宽松格式化：同 [`format_message`]，小数点与分组符取区域的符号（见 [`format_value_in`]）。
pub fn format_message_in(template: &str, arguments: &Details, locale: &str) -> String {
    render(template, arguments, false, symbols(locale)).unwrap_or_default()
}

/// 按区域严格格式化：同 [`try_format_message`]，小数点与分组符取区域的符号。
pub fn try_format_message_in(template: &str, arguments: &Details, locale: &str) -> Option<String> {
    render(template, arguments, true, symbols(locale))
}

/// 逐段渲染；严格模式下缺参数返回 `None`。
fn render(template: &str, arguments: &Details, strict: bool, symbols: Symbols) -> Option<String> {
    let mut output = String::with_capacity(template.len() + 16);
    for segment in segments(template) {
        match segment {
            Segment::Literal(text) => output.push_str(text),
            Segment::Placeholder { name, format } => match arguments.get(name) {
                Some(value) => output.push_str(&value_text(value, format, symbols)),
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

/// 按格式说明符输出一个值（RFC-0010「模板」），数字按不变文化。
///
/// 值先归到值模型：空、布尔、字符串、64 位整数、双精度浮点、数组、对象。`serde_json::Value` 里超出 `i64` 的整数（`u64` 的上半段）
/// 按模型是浮点；单精度与其他数值类型先转成 `Value`（`Value::from(0.1f32)` 即双精度的 `0.10000000149011612`），日期、UUID 等
/// 语言自己的类型先按 `Serialize`（[`json::to_value`]）或 `to_string()` 转成字符串。
///
/// 没有说明符时是值的文本形式：字符串原样，整数十进制，布尔 `true` / `false`，`null` 为空串；浮点在 1e-4 ≤ |x| < 1e17
/// 时写最短往返的十进制、不用指数（`2.0` 写 `2`），范围之外写 `1E+17`、`9.9E-05` 这样的指数形式，负零写 `-0`，非数与无穷写
/// `NaN`、`Infinity`、`-Infinity`；数组与对象写紧凑的 JSON 文本，对象成员按名字的 UTF-16 码元序，超过 64 层的容器写 `null`。
///
/// 说明符只认 `^[DdFfNnXx][0-9]{0,2}$`（精度至多两位，可有前导零），其余（`E2`、`0.00`、`yyyy`、空说明符、带空格、`D100`）
/// 一律按无格式输出：
///
/// - `Dn`：整数，至少 n 位，不足补零，负号在前；`{n:D3}` 取 -7 得 `-007`。
/// - `Xn` / `xn`：非负整数的十六进制，大写或小写，至少 n 位；`{n:x4}` 取 255 得 `00ff`。
/// - `Fn`：整数与浮点的定点小数，n 位小数，缺省 2，写出全部整数位。
/// - `Nn`：同 `F`，整数部分带千分位，负号在前。
///
/// `F` 与 `N` 按值的精确十进制展开舍入，正好一半时取偶：0.125 写 `F2` 得 `0.12`，2.5 写 `F0` 得 `2`，2.675 写 `F2` 得 `2.67`；
/// 负数舍入成零时保留负号。`D`、`F`、`N` 不区分大小写。`D` 与 `X` 用在浮点上、`X` 用在负数上、任何说明符用在字符串、布尔、
/// `null`、数组与对象上都按无格式输出，不报错。
pub fn format_value(value: &Value, format: Option<&str>) -> String {
    value_text(value, format, INVARIANT)
}

/// 按区域输出一个值：同 [`format_value`]，区域只改变小数点与分组符（RFC-0010「模板」的固定表，区域名不区分大小写）。
///
/// de-DE、es-ES、pt-BR 是 `,` 与 `.`，fr-FR 是 `,` 与 U+202F，ru-RU 是 `,` 与 U+00A0；en-US、zh-CN、zh-TW、ja-JP、ko-KR 与表外的
/// 区域都是 `.` 与 `,`。小数点用在无格式的浮点（含指数写法的尾数，de-DE 的 1.5e20 写 `1,5E+20`）与 `F`、`N` 上，分组符只用在 `N` 上；
/// 整数无格式时不分组，`D`、`X` 与数组、对象里的数字不受区域影响，负号、`NaN`、`Infinity` 也不变。
pub fn format_value_in(value: &Value, format: Option<&str>, locale: &str) -> String {
    value_text(value, format, symbols(locale))
}

fn value_text(value: &Value, format: Option<&str>, symbols: Symbols) -> String {
    format.and_then(|format| portable(format, value, symbols)).unwrap_or_else(|| display(value, symbols))
}

/// 区域的数字符号：只有小数点与分组符随区域变化。
#[derive(Clone, Copy)]
pub(crate) struct Symbols {
    decimal: char,
    group: char,
}

/// 不变文化的符号，也是表外区域的符号。
pub(crate) const INVARIANT: Symbols = Symbols { decimal: '.', group: ',' };

/// 小数点与分组符不同于不变文化的内置区域（RFC-0010「模板」，取自 .NET 10 的 ICU 实测）；en-US、zh-CN、zh-TW、ja-JP、ko-KR 同不变文化。
const LOCALE_SYMBOLS: &[(&str, Symbols)] = &[
    ("de-DE", Symbols { decimal: ',', group: '.' }),
    ("es-ES", Symbols { decimal: ',', group: '.' }),
    ("pt-BR", Symbols { decimal: ',', group: '.' }),
    ("fr-FR", Symbols { decimal: ',', group: '\u{202F}' }),
    ("ru-RU", Symbols { decimal: ',', group: '\u{A0}' }),
];

/// 区域名不区分大小写地查表；表外的区域按不变文化。
fn symbols(locale: &str) -> Symbols {
    LOCALE_SYMBOLS.iter().find(|(name, _)| name.eq_ignore_ascii_case(locale)).map_or(INVARIANT, |(_, symbols)| *symbols)
}

/// 值模型（RFC-0010「模板」）。
enum Model<'a> {
    Null,
    Bool(bool),
    Text(&'a str),
    Integer(i64),
    Float(f64),
    Container,
}

fn model(value: &Value) -> Model<'_> {
    match value {
        Value::Null => Model::Null,
        Value::Bool(flag) => Model::Bool(*flag),
        Value::String(text) => Model::Text(text),
        // 64 位有符号范围之外的整数（u64 的上半段）按模型是浮点
        Value::Number(number) => match number.as_i64() {
            Some(integer) => Model::Integer(integer),
            None => Model::Float(number.as_f64().unwrap_or(f64::NAN)),
        },
        Value::Array(_) | Value::Object(_) => Model::Container,
    }
}

/// 值的文本形式（无格式说明符时，`Describe` 同样适用）。
pub(crate) fn display(value: &Value, symbols: Symbols) -> String {
    match model(value) {
        Model::Null => String::new(),
        Model::Bool(flag) => flag.to_string(),
        Model::Text(text) => text.to_owned(),
        Model::Integer(integer) => integer.to_string(),
        Model::Float(float) => localize(&float_text(float), symbols, false),
        Model::Container => {
            let mut text = String::new();
            write_json(&mut text, value, 1);
            text
        }
    }
}

/// 数组与对象的紧凑 JSON 文本：没有空白，对象成员按名字的序数顺序（逐个 UTF-16 码元比较），数字的写法同顶层、按不变文化，
/// 字符串按 JSON 字符串写法；`depth` 是这个值所在的层数，超过 64 层的容器写 `null`。
fn write_json(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::Array(_) | Value::Object(_) if depth > json::MAX_DEPTH => out.push_str("null"),
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(_) => out.push_str(&display(value, INVARIANT)),
        Value::String(text) => json::write_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(out, item, depth + 1);
            }
            out.push(']');
        }
        Value::Object(members) => {
            // Value 的映射按字节序（或插入顺序）排列，与 UTF-16 码元序在基本多文种平面以外的字符上不同
            let mut names: Vec<&String> = members.keys().collect();
            names.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
            out.push('{');
            for (index, name) in names.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                json::write_string(out, name);
                out.push(':');
                write_json(out, &members[name], depth + 1);
            }
            out.push('}');
        }
    }
}

/// 浮点的文本形式（RFC-0010「模板」，取自 .NET 10 的实测），按不变文化。
///
/// 1e-4 ≤ |x| < 1e17 或 x = 0 时写最短往返的十进制、不用指数，整数值不带小数点；范围之外写最短往返的有效数字加指数：
/// `E`、总写的符号、至少两位的指数，如 `1E+17`、`9.9E-05`、`5E-324`。负零写 `-0`，非数与无穷写 `NaN`、`Infinity`、`-Infinity`。
fn float_text(number: f64) -> String {
    if number.is_nan() {
        return "NaN".to_owned();
    }
    if number.is_infinite() {
        return if number > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    // f64 的 Display 就是能往返的最短十进制、不用指数（负零写 -0）
    if number == 0.0 || (1e-4..1e17).contains(&number.abs()) {
        return number.to_string();
    }
    // `{:e}` 给出最短往返的有效数字与指数，如 1.5e-7；改写成 .NET 的 1.5E-07
    let scientific = format!("{number:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    format!("{mantissa}E{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.unsigned_abs())
}

/// 可移植的格式说明符 `^[DdFfNnXx][0-9]{0,2}$`；不认识或不适用时返回 `None`，由调用方按无格式输出。
fn portable(spec: &str, value: &Value, symbols: Symbols) -> Option<String> {
    let (&letter, digits) = spec.as_bytes().split_first()?;
    if digits.len() > 2 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let precision = (!digits.is_empty()).then(|| digits.iter().fold(0, |precision, digit| precision * 10 + usize::from(digit - b'0')));
    let width = precision.unwrap_or(0);
    match (letter, model(value)) {
        (b'D' | b'd', Model::Integer(number)) => Some(format!("{}{:0>width$}", if number < 0 { "-" } else { "" }, number.unsigned_abs())),
        (b'X', Model::Integer(number)) if number >= 0 => Some(format!("{number:0width$X}")),
        (b'x', Model::Integer(number)) if number >= 0 => Some(format!("{number:0width$x}")),
        (b'F' | b'f', number) => fixed(number, precision.unwrap_or(2)).map(|text| localize(&text, symbols, false)),
        (b'N' | b'n', number) => fixed(number, precision.unwrap_or(2)).map(|text| localize(&text, symbols, true)),
        _ => None,
    }
}

/// 定点小数（按不变文化）：整数按十进制补零位；浮点用 Rust 的定点格式化——它按精确的二进制值舍入、正好一半时取偶，写出全部整数位，
/// 负数舍入成零时保留负号（-0.001 写 F2 得 -0.00），与 .NET 一致；非数与无穷按文本形式。其余的值不适用。
fn fixed(number: Model<'_>, precision: usize) -> Option<String> {
    match number {
        Model::Integer(integer) if precision == 0 => Some(integer.to_string()),
        Model::Integer(integer) => Some(format!("{integer}.{}", "0".repeat(precision))),
        Model::Float(float) if float.is_finite() => Some(format!("{float:.precision$}")),
        Model::Float(float) => Some(float_text(float)),
        _ => None,
    }
}

/// 把按不变文化写出的数换成区域的符号：小数点换成区域的小数点，`grouped` 时整数部分每三位加区域的分组符。
///
/// 不以数字开头（`NaN`、`Infinity`）的文本原样返回；指数写法的尾数照样换小数点（`1.5E+20` 在 de-DE 写 `1,5E+20`）。
fn localize(text: &str, symbols: Symbols, grouped: bool) -> String {
    let (sign, unsigned) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    if !unsigned.starts_with(|char: char| char.is_ascii_digit()) {
        return text.to_owned();
    }
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (unsigned, None),
    };
    // 整数部分的数字到第一个非数字（指数写法的 E）为止
    let digits = whole.find(|char: char| !char.is_ascii_digit()).unwrap_or(whole.len());
    let mut localized = String::with_capacity(text.len() + digits);
    localized.push_str(sign);
    for (index, digit) in whole[..digits].char_indices() {
        // 从右往左每三位一组：剩下的位数是三的倍数时插入分组符
        if grouped && index > 0 && (digits - index) % 3 == 0 {
            localized.push(symbols.group);
        }
        localized.push(digit);
    }
    localized.push_str(&whole[digits..]);
    if let Some(fraction) = fraction {
        localized.push(symbols.decimal);
        localized.push_str(fraction);
    }
    localized
}

/// 把位置参数转成以 `"0"`、`"1"`… 为键的参数表。
pub fn positional<I, V>(values: I) -> Details
where
    I: IntoIterator<Item = V>,
    V: Into<Value>,
{
    values.into_iter().enumerate().map(|(index, value)| (index.to_string(), value.into())).collect()
}

/// 在没有文案时描述一个消息：`键 (k=v, ...)`，参数按名字的序数顺序（逐个 UTF-16 码元）排列，值按文本形式；没有参数时只有键。
pub fn describe(key: &str, arguments: &Details) -> String {
    if arguments.is_empty() {
        return key.to_owned();
    }
    // 按名字的序数顺序（逐个 UTF-16 码元比较，与 .NET 的 Ordinal 一致）：BTreeMap 的字节序在基本多文种平面以外的字符上与它不同
    let mut names: Vec<&String> = arguments.keys().collect();
    names.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    let parts: Vec<String> = names.into_iter().map(|name| format!("{name}={}", display(&arguments[name], INVARIANT))).collect();
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

/// 原语层的消息键：读取未初始化的结果（Rust 的 `Result` 没有缺省值，SDK 不产生它，保留供解码与翻译）。
pub const MESSAGE_UNINITIALIZED_RESULT: &str = "primitives.result.uninitialized";

/// SDK 自带的 en-US 模板：SDK 与宿主共用的内核错误（RFC-0011「错误码」），逐字取自向量 `kernel-messages.json`；
/// 另加调用期间未处理失败的模板（规则 5），取自 .NET 的语言文件。
const BUILTIN_TEMPLATES: &[(&str, &str)] = &[
    (MESSAGE_UNINITIALIZED_RESULT, "The result is uninitialized: a default value is not a valid result."),
    ("kernel.invocation.deadline_exceeded", "The invocation exceeded its deadline."),
    ("kernel.invocation.deadline_before_start", "The invocation exceeded its deadline before it started."),
    ("kernel.invocation.cancelled", "The invocation was canceled."),
    ("kernel.operation.unknown", "Operation '{id}' is not registered."),
    ("kernel.operation.invalid_payload", "The payload of operation '{id}' is not valid: {reason}"),
    ("kernel.operation.type_mismatch", "Operation '{id}' was invoked with request or response types that do not match its registration."),
    ("kernel.operation.no_response", "Operation '{id}' completed without producing a response."),
    ("kernel.operation.duplicate", "Operation '{id}' is already registered by '{owner}'."),
    ("kernel.operation.registry_full", "The operation registry has reached its capacity of {capacity}."),
    // 不在向量里：调用期间的未处理失败（RFC-0011 规则 5），模板取自 .NET 的 Localization/en-US.json
    ("kernel.invocation.unhandled_exception", "An unhandled exception occurred during the invocation."),
];

/// SDK 自带的消息目录：只有 [`DEFAULT_LOCALE`]，内容是 SDK 与宿主共用的内核错误模板。
///
/// [`Error::message`](super::Error::message) 在进程默认解析器解析不出时用它，再不行才退回描述；宿主收到错误后按键用自己的目录再翻译。
pub fn builtin_catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| BUILTIN_TEMPLATES.iter().fold(Catalog::new(), |catalog, (key, template)| catalog.with(DEFAULT_LOCALE, *key, *template)))
}

/// 解析一条消息的文案：进程默认解析器，再 SDK 自带的 en-US 模板，都解析不出（含占位符缺参数）时为 `None`。
pub(crate) fn resolve_with_builtin(key: &str, arguments: &Details) -> Option<String> {
    resolve_message(key, arguments).or_else(|| builtin_catalog().resolve(key, DEFAULT_LOCALE, arguments))
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
        // 数字的符号按请求的区域，模板回退到 en-US 时也一样
        try_format_message_in(template, arguments, locale)
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
        let values = arguments(json!({"0": "a", "count": 7, "name": "x", "price": 12.34567, "big": -255, "large": 18446744073709551615u64}));
        assert_eq!(format_message("{0} of {count:D3}", &values), "a of 007");
        assert_eq!(format_message("{{name}} is {name}", &values), "{name} is x");
        assert_eq!(format_message("订单 {name} 不存在。}", &values), "订单 x 不存在。}");
        assert_eq!(format_message("}} {", &values), "} {");
        assert_eq!(format_message("{not valid} {_x} {name", &values), "{not valid} {_x} {name");
        assert_eq!(format_message("{price:F2}|{price:f3}|{price:N0}|{count:d}|{count:F0}|{large:X}", &values), "12.35|12.346|12|7|7|1.8446744073709552E+19");
        // 不支持的说明符与类型不合按无格式输出
        assert_eq!(format_message("{name:N2}|{count:yyyy-MM-dd}|{big:X}|{price:D}|{count:D1000}|{count:%03d}", &values), "x|7|-255|12.34567|7|7");
        assert_eq!(format_message("{missing} {missing:N2}", &values), "{missing} {missing:N2}");
        assert_eq!(try_format_message("{missing}", &values), None);
        assert_eq!(try_format_message("{name}", &values).as_deref(), Some("x"));
        assert_eq!(placeholders("{a} {b:N2} {a} {{c}} {0}"), ["a", "b", "0"]);
        assert_eq!(localize("-1234567.89", INVARIANT, true), "-1,234,567.89");
        assert_eq!(localize("123", INVARIANT, true), "123");
        assert_eq!(localize("1000", INVARIANT, true), "1,000");
        assert_eq!(localize("1.5E+20", symbols("DE-de"), true), "1,5E+20");
        assert_eq!(localize("-Infinity", symbols("fr-FR"), true), "-Infinity");
        assert_eq!(localize("1234567.5", symbols("fr-FR"), true), "1 234 567,5");
        // JSON 带不出非数与无穷，直接核对
        assert_eq!(
            (float_text(f64::NAN), float_text(f64::INFINITY), float_text(f64::NEG_INFINITY)),
            ("NaN".to_owned(), "Infinity".to_owned(), "-Infinity".to_owned())
        );
        assert_eq!(
            (float_text(-1e-5), float_text(1e-4), float_text(99999999999999990.0)),
            ("-1E-05".to_owned(), "0.0001".to_owned(), "99999999999999980".to_owned())
        );
    }

    #[test]
    fn values_follow_the_model_and_the_locale() {
        // 超过 64 层的容器写 null：最外层是第 1 层
        let deep = (0..65).fold(json!(1), |inner, _| json!([inner]));
        assert_eq!(format_value(&deep, None), format!("{}null{}", "[".repeat(64), "]".repeat(64)));
        // 单精度先换成最接近的双精度
        assert_eq!(format_value(&Value::from(0.1f32), None), "0.10000000149011612");
        // 目录解析按请求的区域取符号，模板回退到 en-US 时也一样
        let catalog = Catalog::new().with("en-US", "orders.total", "{n:N2} {n}");
        assert_eq!(catalog.resolve("orders.total", "de-DE", &arguments(json!({"n": 1234.5}))).as_deref(), Some("1.234,50 1234,5"));
        assert_eq!(format_value_in(&json!(-1234.5), Some("N1"), "ru-ru"), "-1 234,5");
    }

    #[test]
    fn describe_sorts_arguments() {
        assert_eq!(describe("orders.failed", &Details::new()), "orders.failed");
        assert_eq!(describe("orders.failed", &arguments(json!({"z": true, "a": 1, "m": "x", "n": null}))), "orders.failed (a=1, m=x, n=, z=true)");
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
