//! 插件日志（`invocation.md` §6）：写 stderr，一行一个 JSON 对象，`level`、`message`，其余成员是作用域属性；
//! 宿主逐行转成自己的日志（来源 `plugins.<清单 ID>`）。

use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;

use crate::primitives::{json, named_enum};

named_enum! {
    /// 日志级别，名字是宿主认的、写进日志行的小写名（`invocation.md` §6），按严重程度从低到高排序。
    ///
    /// 协议以后可以增加级别，所以是 `#[non_exhaustive]`：本 crate 之外的 `match` 要带通配分支。
    #[non_exhaustive]
    pub enum Level ("log level") {
        /// `trace`。
        Trace = 0 => "trace",
        /// `debug`。
        Debug = 1 => "debug",
        /// `information`。
        Information = 2 => "information",
        /// `warning`。
        Warning = 3 => "warning",
        /// `error`。
        Error = 4 => "error",
        /// `critical`。
        Critical = 5 => "critical",
    }
}

/// 写结构化日志行的记录器；克隆共享同一个输出。
///
/// 每行先写 `level` 与 `message`，再写 [`Logger::with_field`] 加的作用域属性与这一条的属性；同名的属性只写一次、后加的为准，
/// 不写名为 `level` 或 `message` 的属性：宿主读 JSON 时重复的成员不合法。消息里的换行按 JSON 转义，不会伪造出新的一行。
///
/// ```
/// use std::sync::{Arc, Mutex};
/// use xihan_framework::plugin::{Level, Logger};
///
/// #[derive(Clone, Default)]
/// struct Buffer(Arc<Mutex<Vec<u8>>>);
/// impl std::io::Write for Buffer {
///     fn write(&mut self, data: &[u8]) -> std::io::Result<usize> { self.0.lock().unwrap().write(data) }
///     fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
/// }
///
/// let buffer = Buffer::default();
/// let logger = Logger::new(buffer.clone()).with_field("plugin", "xihan.samples.echo");
/// logger.log(Level::Warning, "slow\ncall", &[("elapsedMs", 1200.into())]);
/// logger.debug("dropped: below the minimum level");
/// assert_eq!(
///     String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap(),
///     "{\"level\":\"warning\",\"message\":\"slow\\ncall\",\"plugin\":\"xihan.samples.echo\",\"elapsedMs\":1200}\n"
/// );
/// ```
#[derive(Clone)]
pub struct Logger {
    sink: Arc<Mutex<Box<dyn Write + Send>>>,
    minimum: Level,
    fields: Arc<Vec<(String, Value)>>,
}

impl Logger {
    /// 写 stderr、最低级别 [`Level::Information`] 的记录器。
    pub fn stderr() -> Self {
        Self::new(io::stderr())
    }

    /// 写到 `writer`、最低级别 [`Level::Information`] 的记录器；每行写完即 flush。
    pub fn new(writer: impl Write + Send + 'static) -> Self {
        Self { sink: Arc::new(Mutex::new(Box::new(writer))), minimum: Level::Information, fields: Arc::new(Vec::new()) }
    }

    /// 换最低级别：低于它的日志不写。
    pub fn with_minimum(mut self, level: Level) -> Self {
        self.minimum = level;
        self
    }

    /// 派生一个每行都带这个作用域属性的记录器，共享同一个输出。
    pub fn with_field(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        let (name, value) = (name.into(), value.into());
        let mut fields = Vec::clone(&self.fields);
        fields.retain(|(existing, _)| *existing != name);
        fields.push((name, value));
        self.fields = Arc::new(fields);
        self
    }

    /// 判断这个级别的日志会不会写出。
    pub fn is_enabled(&self, level: Level) -> bool {
        level >= self.minimum
    }

    /// 写一行日志；写不出（输出已关闭）时丢弃，不影响调用方。
    pub fn log(&self, level: Level, message: &str, fields: &[(&str, Value)]) {
        if !self.is_enabled(level) {
            return;
        }
        let mut line = String::with_capacity(64 + message.len());
        line.push_str("{\"level\":\"");
        line.push_str(level.as_str());
        line.push_str("\",\"message\":");
        json::write_string(&mut line, message);
        // 作用域属性被这一条的同名属性盖过；这一条里重复的名字只留最后一个
        let scope = self.fields.iter().map(|(name, value)| (name.as_str(), value)).filter(|(name, _)| !fields.iter().any(|(own, _)| own == name));
        let own = fields
            .iter()
            .enumerate()
            .filter(|(index, (name, _))| !fields[index + 1..].iter().any(|(later, _)| later == name))
            .map(|(_, (name, value))| (*name, value));
        for (name, value) in scope.chain(own) {
            if name == "level" || name == "message" {
                continue;
            }
            line.push(',');
            json::write_string(&mut line, name);
            line.push(':');
            line.push_str(&serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned()));
        }
        line.push_str("}\n");
        // 锁里只有一次整行写入，panic 不会留下半行，所以中毒时照常使用
        let mut sink = self.sink.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = sink.write_all(line.as_bytes()).and_then(|()| sink.flush());
    }

    /// 写一条 [`Level::Trace`]。
    pub fn trace(&self, message: &str) {
        self.log(Level::Trace, message, &[]);
    }

    /// 写一条 [`Level::Debug`]。
    pub fn debug(&self, message: &str) {
        self.log(Level::Debug, message, &[]);
    }

    /// 写一条 [`Level::Information`]。
    pub fn information(&self, message: &str) {
        self.log(Level::Information, message, &[]);
    }

    /// 写一条 [`Level::Warning`]。
    pub fn warning(&self, message: &str) {
        self.log(Level::Warning, message, &[]);
    }

    /// 写一条 [`Level::Error`]。
    pub fn error(&self, message: &str) {
        self.log(Level::Error, message, &[]);
    }

    /// 写一条 [`Level::Critical`]。
    pub fn critical(&self, message: &str) {
        self.log(Level::Critical, message, &[]);
    }
}

impl fmt::Debug for Logger {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Logger").field("minimum", &self.minimum).field("fields", &self.fields).finish_non_exhaustive()
    }
}
