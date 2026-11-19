//! 规范里各种文法的手写判定：都是 ASCII 文法，逐字节判断，不引入正则依赖；规范的空白字符（[`is_whitespace`]）按字符判断。
//!
//! 每个函数的文档写出它对应的正则；非 ASCII 字节一律不属于任何字符类，所以多字节 UTF-8 文本总是不匹配。

/// 小写字母 `[a-z]`。
pub(crate) fn lower(byte: u8) -> bool {
    byte.is_ascii_lowercase()
}

/// 大写字母 `[A-Z]`。
pub(crate) fn upper(byte: u8) -> bool {
    byte.is_ascii_uppercase()
}

/// 数字 `[0-9]`。
pub(crate) fn digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

/// 小写字母或数字 `[a-z0-9]`。
pub(crate) fn lower_digit(byte: u8) -> bool {
    lower(byte) || digit(byte)
}

/// 小写十六进制 `[0-9a-f]`。
pub(crate) fn lower_hex(byte: u8) -> bool {
    digit(byte) || (b'a'..=b'f').contains(&byte)
}

/// 小写字母、数字或连字符 `[a-z0-9-]`。
pub(crate) fn lower_digit_hyphen(byte: u8) -> bool {
    lower_digit(byte) || byte == b'-'
}

/// 非空且首字节满足 `first`、其余字节满足 `rest`：即 `first rest*`。
pub(crate) fn word(text: &str, first: fn(u8) -> bool, rest: fn(u8) -> bool) -> bool {
    match text.as_bytes().split_first() {
        Some((&head, tail)) => first(head) && tail.iter().all(|&byte| rest(byte)),
        None => false,
    }
}

/// 非空且全部字节满足 `class`：即 `class+`。
pub(crate) fn all(text: &str, class: fn(u8) -> bool) -> bool {
    !text.is_empty() && text.bytes().all(class)
}

/// 连字符分隔的小写词 `[a-z][a-z0-9]*(-[a-z0-9]+)*`。
pub(crate) fn kebab(text: &str) -> bool {
    let mut parts = text.split('-');
    // 首段必须以字母开头，之后每段非空：连续、开头或结尾的连字符都会产生空段
    parts.next().is_some_and(|head| word(head, lower, lower_digit)) && parts.all(|part| all(part, lower_digit))
}

/// 操作 ID `<resource>.<verb>`：`^[a-z][a-z0-9]*(-[a-z0-9]+)*\.[a-z][a-z0-9]*(-[a-z0-9]+)*$`（RFC-0011）。
///
/// 操作契约与清单的 `contributes.operations` 用同一个文法（RFC-0002 解析规则 2）。
pub(crate) fn operation_id(id: &str) -> bool {
    // 动词段里再有点即不合法：kebab 不收点
    id.split_once('.').is_some_and(|(resource, verb)| kebab(resource) && kebab(verb))
}

/// 接口版本 `[1-9][0-9]*`：不带前导零、符号与空白，在 32 位有符号整数范围内（不超过 2147483647，ADR-0236）。
pub(crate) fn positive(text: &str) -> Option<u32> {
    if !word(text, |byte| (b'1'..=b'9').contains(&byte), digit) {
        return None;
    }
    text.parse::<i32>().ok().and_then(|version| u32::try_from(version).ok())
}

/// 规范所说的空白（RFC-0001「空白字符」）：U+0009–U+000D、U+0020、U+0085、U+00A0、U+1680、U+2000–U+200A、U+2028、U+2029、
/// U+202F、U+205F、U+3000，共 25 个码位；其余字符（含 U+001C–U+001F、U+200B、U+180E 与 U+FEFF）都不是空白。
///
/// 集合按规范逐个写出，不用 [`char::is_whitespace`]：后者跟着 Unicode 的版本走，规范的集合是固定的。
/// Error 的文案、插件握手行与回调端点都用它。
pub(crate) fn is_whitespace(c: char) -> bool {
    matches!(c, '\u{09}'..='\u{0D}' | '\u{20}' | '\u{85}' | '\u{A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
}

/// 文本是否为空或只有空白（RFC-0001「空白字符」）：Error 的文案不能是这样。
pub(crate) fn is_blank(text: &str) -> bool {
    text.chars().all(is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_parses_within_i32() {
        assert_eq!(positive("1"), Some(1));
        assert_eq!(positive("12"), Some(12));
        assert_eq!(positive("2147483647"), Some(2_147_483_647));
        for text in [
            "",
            "0",
            "01",
            "-1",
            "+1",
            "1.0",
            "^1",
            "2147483648",
            "4294967295",
            " 1",
            "1
",
        ] {
            assert_eq!(positive(text), None, "{text}");
        }
    }

    #[test]
    fn kebab_rejects_empty_segments() {
        for text in ["a", "a1", "a-b", "a-1", "feature-flag-2"] {
            assert!(kebab(text), "{text}");
        }
        for text in ["", "-a", "a-", "a--b", "1a", "A", "a_b", "é"] {
            assert!(!kebab(text), "{text}");
        }
    }
}
