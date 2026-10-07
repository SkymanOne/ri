//! The executor the guest runs futures on. yapi's guest is single-threaded
//! and runs only inside an export, so each export polls the woken tasks
//! until none can progress, then returns the outcomes they produced. Host
//! operations wake their task through the `resolve` export.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Poll, Wake, Waker};

use serde_json::Value;

use crate::bindings::yapi::extension::host;
use crate::bindings::yapi::extension::types::Outcome;

pub(crate) type LocalFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// An operation's state: started, with the waker of the task awaiting it,
/// or finished.
enum Slot {
    Waiting(Option<Waker>),
    Done(Result<Value, String>),
}

thread_local! {
    static TASKS: RefCell<HashMap<u64, LocalFuture<()>>> = RefCell::default();
    static READY: RefCell<VecDeque<u64>> = RefCell::default();
    static OPS: RefCell<HashMap<u64, Slot>> = RefCell::default();
    static OUTCOMES: RefCell<Vec<Outcome>> = RefCell::default();
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
}

fn next_id() -> u64 {
    NEXT_ID.with(|next| next.replace(next.get() + 1))
}

/// Wakes task `.0` by queueing it.
struct TaskWaker(u64);

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        READY.with(|ready| ready.borrow_mut().push_back(self.0));
    }
}

/// Runs `future` in the background and returns its task id. It starts once
/// the current handler yields, and keeps running after the handler returns,
/// until it finishes or the extension stops.
pub fn spawn(future: impl Future<Output = ()> + 'static) -> u64 {
    let id = next_id();
    TASKS.with(|tasks| tasks.borrow_mut().insert(id, Box::pin(future)));
    READY.with(|ready| ready.borrow_mut().push_back(id));
    id
}

/// Stops task `id`, dropping its future; whether it was running.
pub(crate) fn cancel(id: u64) -> bool {
    TASKS.with(|tasks| tasks.borrow_mut().remove(&id)).is_some()
}

/// Reports `outcome` when the current export returns.
pub(crate) fn report(outcome: Outcome) {
    OUTCOMES.with(|outcomes| outcomes.borrow_mut().push(outcome));
}

/// Polls woken tasks until none can progress; the outcomes reported since
/// the last call.
pub(crate) fn run() -> Vec<Outcome> {
    while let Some(id) = READY.with(|ready| ready.borrow_mut().pop_front()) {
        // The task is out of the map while it runs, so it can spawn others.
        let Some(mut task) = TASKS.with(|tasks| tasks.borrow_mut().remove(&id)) else {
            continue;
        };
        let waker = Waker::from(Arc::new(TaskWaker(id)));
        if task
            .as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending()
        {
            TASKS.with(|tasks| tasks.borrow_mut().insert(id, task));
        }
    }
    OUTCOMES.with(|outcomes| outcomes.take())
}

/// Completes operation `op` with `value`, the host's JSON answer.
pub(crate) fn resolve(op: u64, value: Result<String, String>) {
    let value = value.and_then(|text| {
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|err| err.to_string())
    });
    let waker = OPS.with(|ops| match ops.borrow_mut().get_mut(&op) {
        Some(slot) => match std::mem::replace(slot, Slot::Done(value)) {
            Slot::Waiting(waker) => waker,
            Slot::Done(_) => None,
        },
        None => None,
    });
    if let Some(waker) = waker {
        waker.wake();
    }
}

/// Starts host operation `kind` with JSON `payload` and resolves with its
/// answer, `null` when the host answers nothing. The operation starts at
/// once; dropping the future ignores its answer.
pub fn op(kind: &str, payload: &Value) -> impl Future<Output = Result<Value, String>> + 'static {
    let id = next_id();
    OPS.with(|ops| ops.borrow_mut().insert(id, Slot::Waiting(None)));
    host::start(id, kind, &payload.to_string());
    Op(id)
}

struct Op(u64);

impl Future for Op {
    type Output = Result<Value, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        OPS.with(|ops| {
            let mut ops = ops.borrow_mut();
            match ops.remove(&self.0) {
                Some(Slot::Done(value)) => Poll::Ready(value),
                _ => {
                    ops.insert(self.0, Slot::Waiting(Some(cx.waker().clone())));
                    Poll::Pending
                }
            }
        })
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        OPS.with(|ops| ops.borrow_mut().remove(&self.0));
    }
}

/// Waits `ms` milliseconds.
pub async fn sleep(ms: u64) {
    let _ = op("timer", &serde_json::json!({ "ms": ms })).await;
}
