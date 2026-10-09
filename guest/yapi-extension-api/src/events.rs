//! Pi's `pi.events`: a bus that extensions use to talk to each other,
//! whichever runtime they run in.
//!
//! [`emit`] runs this extension's own handlers for the channel at once, in
//! the order they subscribed, as Pi's `emit` does. yapi then delivers the
//! event to the handlers of every other extension, native or Pi, after
//! `emit` has returned. Data travels between extensions as JSON.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use serde_json::{Value, json};

use crate::request;

type Handler = Rc<dyn Fn(&Value)>;

thread_local! {
    static HANDLERS: RefCell<Vec<(u64, String, Handler)>> = RefCell::default();
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
}

/// A handler [`on`] subscribed. Dropping it keeps the handler subscribed.
#[derive(Debug)]
pub struct Subscription(u64);

impl Subscription {
    /// Stops the handler, as calling the function Pi's `on` returns does.
    pub fn unsubscribe(self) {
        HANDLERS.with(|handlers| handlers.borrow_mut().retain(|(id, ..)| *id != self.0));
    }
}

/// Runs `handler` with the data of every event emitted on `channel`, by this
/// extension or any other, until it unsubscribes or the extension loads
/// again for a new session. Handlers run one after another, so a handler
/// that waits for host work starts it with [`spawn`](crate::spawn).
pub fn on(channel: impl Into<String>, handler: impl Fn(&Value) + 'static) -> Subscription {
    let id = NEXT_ID.with(|next| next.replace(next.get() + 1));
    HANDLERS.with(|handlers| {
        handlers
            .borrow_mut()
            .push((id, channel.into(), Rc::new(handler)));
    });
    Subscription(id)
}

/// Emits `data` on `channel`: runs this extension's handlers for it now,
/// and every other extension's after this returns.
pub fn emit(channel: &str, data: &Value) {
    deliver(channel, data);
    // A host with no other extensions has no one to tell.
    let _ = request("events.emit", &json!({"channel": channel, "data": data}));
}

/// Runs the handlers subscribed to `channel` when the delivery starts.
pub(crate) fn deliver(channel: &str, data: &Value) {
    let handlers: Vec<Handler> = HANDLERS.with(|handlers| {
        handlers
            .borrow()
            .iter()
            .filter(|(_, name, _)| name == channel)
            .map(|(_, _, handler)| handler.clone())
            .collect()
    });
    for handler in handlers {
        handler(data);
    }
}

/// Drops every handler, as Pi does when the session's extensions go stale.
pub(crate) fn reset() {
    HANDLERS.with(|handlers| handlers.borrow_mut().clear());
}
