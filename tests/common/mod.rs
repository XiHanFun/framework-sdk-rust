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

/// 运行闭包并取出 panic 的消息；没有 panic 时测试失败。
pub fn panic_message(action: impl FnOnce() + std::panic::UnwindSafe) -> String {
    let payload = std::panic::catch_unwind(action).expect_err("the action must panic");
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    payload.downcast_ref::<&str>().map(|text| (*text).to_owned()).unwrap_or_default()
}
