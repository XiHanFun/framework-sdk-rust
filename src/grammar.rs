//! 规范里各种文法的手写判定：都是 ASCII 文法，逐字节判断，不引入正则依赖。
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

/// 正整数 `[1-9][0-9]*`，并且能放进 `u32`。
pub(crate) fn positive(text: &str) -> Option<u32> {
    if !word(text, |byte| (b'1'..=b'9').contains(&byte), digit) {
        return None;
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_parses_within_u32() {
        assert_eq!(positive("1"), Some(1));
        assert_eq!(positive("12"), Some(12));
        assert_eq!(positive("4294967295"), Some(u32::MAX));
        for text in ["", "0", "01", "-1", "+1", "1.0", "^1", "4294967296"] {
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
