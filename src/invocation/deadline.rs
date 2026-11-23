//! 截止时间的定时器：进程里一个后台线程，按到期时刻触发登记的令牌。

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use super::cancellation::CancellationToken;

/// 一次调用的截止时间：到期时由定时器先触发截止令牌、再触发上下文令牌。
///
/// 归因只看截止令牌（RFC-0004「Resilience 阶段」，ADR-0095）：它只由定时器触发，不在读的时候另看时钟。
pub(crate) struct Deadline {
    at: SystemTime,
    fired: CancellationToken,
    // 丢弃即从定时器里撤下条目，条目数以仍然存活的截止时间为上限
    _registration: Option<Registration>,
    // 派生上下文收紧截止时间时，原上下文的截止时间：它仍然有效，它的令牌触发同样是超时
    outer: Option<Arc<Deadline>>,
}

impl Deadline {
    /// 登记截止时间：已经过去的立即触发；远到 `Instant` 表示不了的不起定时器，等同于调用期间不会到期（ADR-0203）。
    pub(crate) fn start(at: SystemTime, context: &CancellationToken) -> Arc<Self> {
        Self::start_within(at, context, None)
    }

    /// 同 [`Deadline::start`]，`outer` 是被它收紧的外层截止时间（派生上下文用）：外层的截止令牌触发时，这一个同样算已触发。
    pub(crate) fn start_within(at: SystemTime, context: &CancellationToken, outer: Option<Arc<Deadline>>) -> Arc<Self> {
        let fired = CancellationToken::new();
        let registration = match at.duration_since(SystemTime::now()) {
            Ok(remaining) if remaining > Duration::ZERO => {
                Instant::now().checked_add(remaining).map(|instant| Timer::global().register(instant, fired.clone(), context.clone()))
            }
            _ => {
                fire(&fired, context);
                None
            }
        };
        Arc::new(Self { at, fired, _registration: registration, outer })
    }

    /// 绝对截止时间。
    pub(crate) fn at(&self) -> SystemTime {
        self.at
    }

    /// 截止令牌是否已触发：自己的，或外层任何一个的。
    pub(crate) fn has_fired(&self) -> bool {
        // 外层的截止时间通常更晚，但两个定时器各按登记时的单调时钟换算，外层的令牌可能先到；沿着链看，层数就是收紧的次数
        let mut current = Some(self);
        while let Some(deadline) = current {
            if deadline.fired.is_cancelled() {
                return true;
            }
            current = deadline.outer.as_deref();
        }
        false
    }

    /// 测试用：立即触发截止令牌与上下文令牌，截止时间不变。
    #[cfg(test)]
    pub(crate) fn fire_now(&self, context: &CancellationToken) {
        fire(&self.fired, context);
    }
}

/// 先触发截止令牌、再触发上下文令牌：被上下文令牌唤醒的等待方读归因时一定已经看到截止令牌触发
fn fire(deadline: &CancellationToken, context: &CancellationToken) {
    deadline.cancel();
    context.cancel();
}

/// 定时器里的一个条目：同一时刻按登记序号区分。
type Key = (Instant, u64);

struct Timer {
    state: Mutex<TimerState>,
    changed: Condvar,
}

#[derive(Default)]
struct TimerState {
    entries: BTreeMap<Key, (CancellationToken, CancellationToken)>,
    next: u64,
}

/// 登记的凭据：丢弃时撤下条目。
struct Registration {
    key: Key,
}

impl Drop for Registration {
    fn drop(&mut self) {
        Timer::global().lock().entries.remove(&self.key);
    }
}

impl Timer {
    /// 进程唯一的定时器；第一次登记时启动后台线程，线程随进程结束。
    fn global() -> &'static Self {
        static TIMER: OnceLock<Timer> = OnceLock::new();
        static STARTED: Once = Once::new();
        let timer = TIMER.get_or_init(|| Self { state: Mutex::default(), changed: Condvar::new() });
        STARTED.call_once(|| {
            thread::Builder::new()
                .name("xihan-deadline-timer".to_owned())
                .spawn(move || timer.run())
                .unwrap_or_else(|error| panic!("The deadline timer thread could not be started: {error}."));
        });
        timer
    }

    /// 锁里只有对条目表的整体增删，panic 不会留下半改的状态，所以中毒时照常使用
    fn lock(&self) -> MutexGuard<'_, TimerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(&self, at: Instant, deadline: CancellationToken, context: CancellationToken) -> Registration {
        let mut state = self.lock();
        let key = (at, state.next);
        state.next += 1;
        // 只有新条目成了最早的一个，后台线程才需要提前醒来重新计时
        let earliest = state.entries.first_key_value().is_none_or(|(first, _)| key < *first);
        state.entries.insert(key, (deadline, context));
        drop(state);
        if earliest {
            self.changed.notify_one();
        }
        Registration { key }
    }

    /// 后台线程：睡到最早的条目到期，取出并在锁外触发，再看下一个。
    fn run(&self) {
        let mut state = self.lock();
        loop {
            let now = Instant::now();
            state = match state.entries.first_key_value().map(|(key, _)| key.0) {
                None => self.changed.wait(state).unwrap_or_else(PoisonError::into_inner),
                Some(at) if at <= now => {
                    let entry = state.entries.pop_first();
                    // 在锁外触发：唤醒等待方时可能有令牌被丢弃，丢弃凭据要回来拿这把锁
                    drop(state);
                    if let Some((_, (deadline, context))) = entry {
                        fire(&deadline, &context);
                    }
                    self.lock()
                }
                Some(at) => self.changed.wait_timeout(state, at - now).unwrap_or_else(PoisonError::into_inner).0,
            };
        }
    }
}
