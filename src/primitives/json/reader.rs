//! 按 RFC 8259 逐字节读取 JSON 文本，同时做共同读取检查（RFC-0001「JSON 文本」），得到保留文档顺序的值树。

use std::collections::HashSet;

use super::{MAX_DEPTH, ReadError, push_index, push_member};

/// 读出的 JSON 值：对象的成员保留在文本里的顺序，数字按值模型分成整数与浮点（RFC-0010「模板」）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Node {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Node>),
    Object(Vec<(String, Node)>),
}

/// 值模型里的数：整数写法（没有小数点与指数）在 64 位有符号范围内是整数，其余是双精度浮点。
///
/// 超出 `i64` 而在 `u64` 范围内的整数写法单独保留为 [`Number::Unsigned`]，让 `u64` 类型的成员照常读入；按值模型它是浮点。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Number {
    Integer(i64),
    Unsigned(u64),
    Float(f64),
}

/// 读取并检查：语法错误不论出现在哪里都优先，所以遇到重复成员也读到末尾。
pub(crate) fn parse(data: &[u8]) -> Result<Node, ReadError> {
    let mut reader = Reader { data, at: 0, path: String::from("$"), duplicate: None };
    reader.whitespace();
    let node = reader.value(0).ok_or(ReadError::Syntax)?;
    reader.whitespace();
    if reader.at != data.len() {
        return Err(ReadError::Syntax);
    }
    match reader.duplicate {
        Some(path) => Err(ReadError::Duplicate { path }),
        None => Ok(node),
    }
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
    // 当前位置的路径，进入成员或数组项时追加、离开时截回
    path: String,
    // 第一处重复（第二次出现的成员）的路径
    duplicate: Option<String>,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.data.get(self.at).copied()
    }

    /// 跳过 JSON 的四种空白。
    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    /// 读一个值；`depth` 是外层容器的层数，最外层的对象或数组是第 1 层。
    fn value(&mut self, depth: usize) -> Option<Node> {
        match self.peek()? {
            b'{' => self.object(depth + 1),
            b'[' => self.array(depth + 1),
            b'"' => self.string().map(Node::String),
            b'-' | b'0'..=b'9' => self.number().map(Node::Number),
            b't' => self.literal(b"true", Node::Bool(true)),
            b'f' => self.literal(b"false", Node::Bool(false)),
            b'n' => self.literal(b"null", Node::Null),
            _ => None,
        }
    }

    fn object(&mut self, depth: usize) -> Option<Node> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.at += 1;
        self.whitespace();
        let mut members = Vec::new();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Some(Node::Object(members));
        }
        // 名字按转义还原后比较：逐个 UTF-16 码元相等与 UTF-8 字节相等是同一回事
        let mut names = HashSet::new();
        loop {
            if self.peek() != Some(b'"') {
                return None;
            }
            let name = self.string()?;
            let parent = self.path.len();
            push_member(&mut self.path, &name);
            if !names.insert(name.clone()) && self.duplicate.is_none() {
                self.duplicate = Some(self.path.clone());
            }
            self.whitespace();
            if self.peek() != Some(b':') {
                return None;
            }
            self.at += 1;
            self.whitespace();
            let value = self.value(depth)?;
            self.path.truncate(parent);
            members.push((name, value));
            self.whitespace();
            match self.peek()? {
                b',' => {
                    self.at += 1;
                    self.whitespace();
                }
                b'}' => {
                    self.at += 1;
                    return Some(Node::Object(members));
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<Node> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.at += 1;
        self.whitespace();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Some(Node::Array(items));
        }
        loop {
            let parent = self.path.len();
            push_index(&mut self.path, items.len());
            let item = self.value(depth)?;
            self.path.truncate(parent);
            items.push(item);
            self.whitespace();
            match self.peek()? {
                b',' => {
                    self.at += 1;
                    self.whitespace();
                }
                b']' => {
                    self.at += 1;
                    return Some(Node::Array(items));
                }
                _ => return None,
            }
        }
    }

    /// 读一个字符串并还原转义：非法 UTF-8（含过长编码与编码成 UTF-8 的代理项）、原始控制字符、非法转义与孤立的代理项都不合法。
    fn string(&mut self) -> Option<String> {
        self.at += 1;
        let mut text = String::new();
        loop {
            // 引号、反斜杠与控制字符都是 ASCII，不会落在多字节序列中间：其间的一段整体按 UTF-8 校验
            let start = self.at;
            while self.peek().is_some_and(|byte| byte != b'"' && byte != b'\\' && byte >= 0x20) {
                self.at += 1;
            }
            text.push_str(std::str::from_utf8(&self.data[start..self.at]).ok()?);
            match self.peek()? {
                b'"' => {
                    self.at += 1;
                    return Some(text);
                }
                b'\\' => self.escape(&mut text)?,
                _ => return None,
            }
        }
    }

    /// 读一个转义序列；高代理项后必须紧跟低代理项的转义。
    fn escape(&mut self, text: &mut String) -> Option<()> {
        let kind = *self.data.get(self.at + 1)?;
        self.at += 2;
        let char = match kind {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{08}',
            b'f' => '\u{0C}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => match self.hex4()? {
                high @ 0xD800..=0xDBFF => {
                    if self.data.get(self.at..self.at + 2)? != b"\\u" {
                        return None;
                    }
                    self.at += 2;
                    let low = self.hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&low) {
                        return None;
                    }
                    char::from_u32(0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(low) - 0xDC00))?
                }
                0xDC00..=0xDFFF => return None,
                unit => char::from_u32(u32::from(unit))?,
            },
            _ => return None,
        };
        text.push(char);
        Some(())
    }

    /// 读四位十六进制数。
    fn hex4(&mut self) -> Option<u16> {
        let digits = self.data.get(self.at..self.at + 4)?;
        // from_str_radix 接受前导的 '+'，先逐位核对
        if !digits.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        let unit = u16::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()?;
        self.at += 4;
        Some(unit)
    }

    /// 读一个数 `-? (0 | [1-9][0-9]*) (.[0-9]+)? ([eE][+-]?[0-9]+)?`，按值模型分类。
    fn number(&mut self) -> Option<Number> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek()? {
            b'0' => self.at += 1,
            b'1'..=b'9' => {
                self.digits();
            }
            _ => return None,
        }
        let mut integer = true;
        if self.peek() == Some(b'.') {
            self.at += 1;
            if !self.digits() {
                return None;
            }
            integer = false;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !self.digits() {
                return None;
            }
            integer = false;
        }
        let text = std::str::from_utf8(&self.data[start..self.at]).ok()?;
        if integer {
            // -0 是整数 0；超出 i64 的整数写法按模型是浮点，u64 范围内的另记一类
            if let Ok(number) = text.parse::<i64>() {
                return Some(Number::Integer(number));
            }
            if let Ok(number) = text.parse::<u64>() {
                return Some(Number::Unsigned(number));
            }
        }
        // 上溢为无穷、下溢为 0（Rust 的解析就是这样，不报错）
        text.parse::<f64>().ok().map(Number::Float)
    }

    /// 读至少一位十进制数字。
    fn digits(&mut self) -> bool {
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
        self.at > start
    }

    fn literal(&mut self, word: &[u8], node: Node) -> Option<Node> {
        if self.data[self.at..].starts_with(word) {
            self.at += word.len();
            return Some(node);
        }
        None
    }
}
