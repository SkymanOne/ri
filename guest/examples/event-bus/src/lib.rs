//! Extensions talking over `pi.events`, a port of Pi's `event-bus.ts`.
//! The listener shows each `my:notification` event, from this extension or
//! any other, native or Pi. `/emit [message]` emits one, and so does the
//! start of a session. `/mute` stops the listener, and runs it again.

use std::cell::RefCell;

use yapi_extension_api::events::{self, Subscription};
use yapi_extension_api::{Api, json, notify};

thread_local! {
    /// The listener, unless muted.
    static LISTENER: RefCell<Option<Subscription>> = const { RefCell::new(None) };
}

fn listen() -> Subscription {
    events::on("my:notification", |data| {
        let message = data["message"].as_str().unwrap_or_default();
        let from = data["from"].as_str().unwrap_or_default();
        notify(&format!("Event from {from}: {message}"), "info");
    })
}

fn init(api: &mut Api) {
    LISTENER.set(Some(listen()));
    api.register_command(
        "emit",
        "Emit my:notification event (usage: /emit message)",
        |args, _ctx| async move {
            let message = match args.trim() {
                "" => "hello",
                message => message,
            };
            let data = json!({"message": message, "from": "/emit command"});
            events::emit("my:notification", &data);
            Ok(())
        },
    );
    api.register_command(
        "mute",
        "Stop or resume showing my:notification events",
        |_args, _ctx| async move {
            match LISTENER.take() {
                Some(listener) => listener.unsubscribe(),
                None => LISTENER.set(Some(listen())),
            }
            Ok(())
        },
    );
    api.on("session_start", |_event, _ctx| async move {
        let data = json!({"message": "Session started", "from": "event-bus-example"});
        events::emit("my:notification", &data);
        Ok(None)
    });
}

yapi_extension_api::extension!(init);
