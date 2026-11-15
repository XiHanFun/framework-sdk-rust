use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::task::{Context, Poll, Waker};

/// 节点编号：子节点以它登记在父节点里，丢弃时按它注销。
static NEXT_NODE: AtomicU64 = AtomicU64::new(1);

/// 取消令牌：可以取消一次、可以等待，取消沿父子关系向下传播。
///
/// 克隆共享同一个令牌；[`CancellationToken::child_token`] 派生的子令牌在父令牌取消时随之取消，取消子令牌不影响父令牌。
/// 与运行时无关：[`CancellationToken::cancelled`] 返回的 future 由任何执行器驱动。
///
/// 父令牌只以弱引用记下子令牌，子令牌丢弃时注销自己；等待中的 future 丢弃时注销自己的唤醒器。
/// 所以登记的条目数以仍然存活的子令牌与 future 为上限，不随调用次数增长。
#[derive(Clone)]
pub struct CancellationToken {
    node: Arc<Node>,
}

struct Node {
    id: u64,
    // 子令牌持有父令牌：父令牌活得至少和子令牌一样久，丢弃子令牌时才能去父令牌里注销
    parent: Option<Arc<Node>>,
    cancelled: AtomicBool,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    children: HashMap<u64, Weak<Node>>,
    waiters: HashMap<u64, Waker>,
    next_waiter: u64,
}

impl Node {
    fn new(parent: Option<Arc<Node>>) -> Arc<Self> {
        Arc::new(Self { id: NEXT_NODE.fetch_add(1, Ordering::Relaxed), parent, cancelled: AtomicBool::new(false), state: Mutex::default() })
    }

    /// 锁里只有对两个表的整体增删，panic 不会留下半改的状态，所以中毒时照常使用
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(parent) = &self.parent {
            parent.lock().children.remove(&self.id);
        }
    }
}

impl CancellationToken {
    /// 创建未取消的令牌。
    pub fn new() -> Self {
        Self { node: Node::new(None) }
    }

    /// 派生子令牌：本令牌取消时子令牌随之取消；本令牌已取消时子令牌一出生就是取消的。
    pub fn child_token(&self) -> Self {
        let child = Node::new(Some(Arc::clone(&self.node)));
        let registered = {
            let mut state = self.node.lock();
            // 在父令牌的锁里判断：cancel 也在这把锁里置位并取走子令牌，二者不会交错，子令牌不会漏掉取消
            let registered = !self.node.is_cancelled();
            if registered {
                state.children.insert(child.id, Arc::downgrade(&child));
            }
            registered
        };
        if !registered {
            child.cancelled.store(true, Ordering::Release);
        }
        Self { node: child }
    }

    /// 取消本令牌与它的全部子令牌，唤醒全部等待方；重复取消什么都不做。
    pub fn cancel(&self) {
        let mut pending = vec![Arc::clone(&self.node)];
        while let Some(node) = pending.pop() {
            let taken = {
                let mut state = node.lock();
                // 置位与取走等待方、子令牌在同一把锁里：之后登记的等待方与子令牌都会先看到已取消
                if node.is_cancelled() {
                    None
                } else {
                    node.cancelled.store(true, Ordering::Release);
                    Some((std::mem::take(&mut state.waiters), std::mem::take(&mut state.children)))
                }
            };
            // 在锁外唤醒与递进：唤醒器可能做任意事，子令牌的最后一个引用在这里丢弃时会回来锁父令牌
            if let Some((waiters, children)) = taken {
                waiters.into_values().for_each(Waker::wake);
                pending.extend(children.into_values().filter_map(|child| child.upgrade()));
            }
        }
    }

    /// 判断是否已取消。
    pub fn is_cancelled(&self) -> bool {
        self.node.is_cancelled()
    }

    /// 等待取消：返回的 future 在令牌取消后完成，已取消时第一次轮询即完成。
    pub fn cancelled(&self) -> WaitForCancellation {
        WaitForCancellation { node: Arc::clone(&self.node), waiter: None }
    }

    /// 测试用：父令牌里登记的子令牌数。
    #[cfg(test)]
    fn registered_children(&self) -> usize {
        self.node.lock().children.len()
    }

    /// 测试用：登记的唤醒器数。
    #[cfg(test)]
    fn registered_waiters(&self) -> usize {
        self.node.lock().waiters.len()
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("CancellationToken").field("cancelled", &self.is_cancelled()).finish()
    }
}

/// 等待令牌取消的 future，由 [`CancellationToken::cancelled`] 返回；不借用令牌，可以 `'static` 地交给执行器。
#[must_use = "futures do nothing unless polled"]
pub struct WaitForCancellation {
    node: Arc<Node>,
    waiter: Option<u64>,
}

impl Future for WaitForCancellation {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.node.is_cancelled() {
            // cancel 在置位的同一把锁里取走了全部唤醒器，这里的条目已经不在表里
            this.waiter = None;
            return Poll::Ready(());
        }
        let mut state = this.node.lock();
        if this.node.is_cancelled() {
            this.waiter = None;
            return Poll::Ready(());
        }
        let waker = context.waker();
        match this.waiter {
            Some(id) => match state.waiters.get_mut(&id) {
                Some(existing) if existing.will_wake(waker) => {}
                Some(existing) => existing.clone_from(waker),
                None => {
                    state.waiters.insert(id, waker.clone());
                }
            },
            None => {
                let id = state.next_waiter;
                state.next_waiter += 1;
                state.waiters.insert(id, waker.clone());
                this.waiter = Some(id);
            }
        }
        Poll::Pending
    }
}

impl Drop for WaitForCancellation {
    fn drop(&mut self) {
        if let Some(id) = self.waiter {
            self.node.lock().waiters.remove(&id);
        }
    }
}

impl fmt::Debug for WaitForCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WaitForCancellation").field("cancelled", &self.node.is_cancelled()).finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::task::Wake;

    use super::*;

    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn children_and_waiters_unregister_when_dropped() {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        let grandchild = child.child_token();
        assert_eq!(parent.registered_children(), 1);
        drop(grandchild);
        assert_eq!(child.registered_children(), 0);
        drop(child);
        assert_eq!(parent.registered_children(), 0);

        let counter = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut context = Context::from_waker(&waker);
        let mut waiting = parent.cancelled();
        assert_eq!(Pin::new(&mut waiting).poll(&mut context), Poll::Pending);
        assert_eq!(Pin::new(&mut waiting).poll(&mut context), Poll::Pending);
        assert_eq!(parent.registered_waiters(), 1, "re-polling keeps one entry");
        drop(waiting);
        assert_eq!(parent.registered_waiters(), 0);

        let mut waiting = parent.cancelled();
        assert_eq!(Pin::new(&mut waiting).poll(&mut context), Poll::Pending);
        parent.cancel();
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(Pin::new(&mut waiting).poll(&mut context), Poll::Ready(()));
        assert_eq!(parent.registered_waiters(), 0);
    }

    #[test]
    fn cancellation_propagates_downwards_only() {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        let grandchild = child.child_token();
        let sibling = parent.child_token();
        child.cancel();
        assert!(child.is_cancelled() && grandchild.is_cancelled());
        assert!(!parent.is_cancelled() && !sibling.is_cancelled());
        parent.cancel();
        parent.cancel();
        assert!(sibling.is_cancelled());
        assert!(parent.child_token().is_cancelled(), "a child of a cancelled token is born cancelled");
        assert_eq!(parent.registered_children(), 0);
    }
}
