//! 测试共用：读规范仓库的一致性向量、运行 future 的最小执行器、取 panic 消息。

#![allow(dead_code)]

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

use serde::de::DeserializeOwned;

/// 读 `spec/conformance/vectors/<relative>`；没有检出 submodule 时给出明确的提示。
pub fn vector<T: DeserializeOwned>(relative: &str) -> T {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "spec", "conformance", "vectors", relative].iter().collect();
    let data = std::fs::read(&path).unwrap_or_else(|error| panic!("cannot read {}: {error}; run `git submodule update --init`", path.display()));
    serde_json::from_slice(&data).unwrap_or_else(|error| panic!("cannot parse {}: {error}", path.display()))
}

/// 唤醒时解除当前线程的 park。
struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// 在当前线程上运行 future 直到完成。
pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

/// 运行闭包并取出 panic 的消息；没有 panic 时测试失败。SDK 的编程错误以 FrameworkError 为内容，取它的文本。
pub fn panic_message(action: impl FnOnce() + std::panic::UnwindSafe) -> String {
    let payload = std::panic::catch_unwind(action).expect_err("the action must panic");
    if let Some(error) = payload.downcast_ref::<xihan_framework::primitives::FrameworkError>() {
        return error.to_string();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    payload.downcast_ref::<&str>().map(|text| (*text).to_owned()).unwrap_or_default()
}

/// 核对一个按名字编码的枚举：`name`、`as_str`、`Display`、`FromStr` 与 serde 给出、接受的都是同一个名字，未知名字读不进来。
pub fn assert_named<T>(all: &[T], names: &[&str], name: fn(T) -> &'static str, as_str: fn(T) -> &'static str)
where
    T: Copy + PartialEq + std::fmt::Debug + std::fmt::Display + std::str::FromStr + serde::Serialize + DeserializeOwned,
{
    assert_eq!(all.len(), names.len(), "{all:?}");
    for (value, expected) in all.iter().zip(names) {
        assert_eq!((name(*value), as_str(*value), value.to_string().as_str()), (*expected, *expected, *expected));
        assert!(expected.parse::<T>().ok() == Some(*value), "{expected} must parse");
        assert_eq!(serde_json::to_value(value).unwrap(), serde_json::json!(expected));
        assert_eq!(serde_json::from_value::<T>(serde_json::json!(expected)).unwrap(), *value);
    }
    assert!("no such name".parse::<T>().is_err() && serde_json::from_value::<T>(serde_json::json!("no such name")).is_err());
}

/// 向量 `kernel-messages.json` 里的一条内核错误。
#[derive(serde::Deserialize)]
pub struct KernelMessage {
    pub code: String,
    pub key: String,
    pub arguments: Vec<String>,
    pub template: String,
}

#[derive(serde::Deserialize)]
struct KernelMessages {
    messages: Vec<KernelMessage>,
}

/// 全部内核错误。
pub fn kernel_messages() -> Vec<KernelMessage> {
    vector::<KernelMessages>("primitives/kernel-messages.json").messages
}

/// 核对一个错误与向量里同键的条目一致：错误码、消息键、参数名，以及没有进程默认解析器时的 en-US 文案。
pub fn assert_kernel_error(error: &xihan_framework::primitives::Error, key: &str) {
    let messages = kernel_messages();
    let expected = messages.iter().find(|message| message.key == key).unwrap_or_else(|| panic!("{key} is not in kernel-messages.json"));
    assert_eq!((error.code(), error.message_key()), (expected.code.as_str(), Some(key)), "{error}");
    let names: Vec<&str> = error.details().map(|details| details.keys().map(String::as_str).collect()).unwrap_or_default();
    let mut arguments: Vec<&str> = expected.arguments.iter().map(String::as_str).collect();
    arguments.sort_unstable();
    assert_eq!(names, arguments, "the arguments of {key}");
    let empty = xihan_framework::primitives::Details::new();
    let text = xihan_framework::primitives::format_message(&expected.template, error.details().unwrap_or(&empty));
    assert_eq!(error.message(), text, "the en-US message of {key}");
}

/// 向量 `baggage-keys.json` 的 `values`：已登记键的值能否进入调用上下文。
#[derive(serde::Deserialize)]
pub struct BaggageValues {
    pub acceptable: Vec<String>,
    pub unacceptable: Vec<String>,
}

#[derive(serde::Deserialize)]
struct BaggageVectors {
    values: BaggageValues,
}

/// Baggage 值的向量。
pub fn baggage_values() -> BaggageValues {
    vector::<BaggageVectors>("primitives/baggage-keys.json").values
}
