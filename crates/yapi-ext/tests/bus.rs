//! `pi.events` across runtimes: Pi extensions and native extensions, each
//! runtime in an instance of its own, hear each other's events.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{Notes, cli_source, custom_entries, engine, eventually, options, scratch, session};
use serde_json::{Value, json};
use yapi_ai::faux::Faux;
use yapi_core::extensions::Mode;
use yapi_ext::{Engine, ExtensionHost};

/// Records what it hears on `my:notification`: `heard` until `/stop`,
/// `all` always.
const LISTENER: &str = r#"
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	let heard = 0;
	const stop = pi.events.on("my:notification", (data) => {
		heard++;
		pi.appendEntry("heard", data);
	});
	pi.events.on("my:notification", (data) => pi.appendEntry("all", data));
	pi.registerCommand("tell", {
		description: "Emits my:notification",
		handler: async (args) => {
			pi.events.emit("my:notification", { message: args, from: "js" });
			pi.appendEntry("heard-during-emit", heard);
		},
	});
	pi.registerCommand("stop", { description: "Stops heard", handler: async () => stop() });
}
"#;

async fn js(engine: &Engine, dir: &Path, name: &str, source: &str) -> Arc<ExtensionHost> {
    let path = dir.join(format!("{name}.ts"));
    std::fs::write(&path, source).unwrap();
    let host = ExtensionHost::load(engine, options(dir), &[cli_source(&path)])
        .await
        .unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    host
}

fn messages(entries: Vec<Value>) -> Vec<String> {
    entries
        .iter()
        .map(|data| {
            format!(
                "{}: {}",
                data["from"].as_str().unwrap(),
                data["message"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn pi_and_native_extensions_hear_each_other() {
    let dir = scratch("bus");
    let engine = engine();
    let listener = js(&engine, &dir, "listener", LISTENER).await;
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/event-bus.wasm");
    let native = ExtensionHost::load_native(&engine, options(&dir), &cli_source(&example))
        .await
        .unwrap();
    let extensions = [listener.for_session(), native.for_session()].concat();
    let session = session(&Faux::new([]), &dir, extensions);
    let notes = Arc::new(Notes::default());
    session
        .bind_extensions(notes.clone(), Mode::Print, None, None)
        .await;
    let heard = || messages(custom_entries(&session, "heard"));

    // The emitter's own listener runs during `emit`, the other runtime's after.
    assert_eq!(
        notes.all(),
        ["Event from event-bus-example: Session started"]
    );
    eventually("the start", || {
        heard() == ["event-bus-example: Session started"]
    })
    .await;
    session.prompt("/emit hi", Vec::new()).await.unwrap();
    assert_eq!(notes.all()[1], "Event from /emit command: hi");
    eventually("hi", || heard().len() == 2).await;
    assert_eq!(heard()[1], "/emit command: hi");

    session.prompt("/tell yo", Vec::new()).await.unwrap();
    assert_eq!(custom_entries(&session, "heard-during-emit"), [json!(3)]);
    eventually("yo", || notes.all().len() == 3).await;
    assert_eq!(notes.all()[2], "Event from js: yo");

    // A Pi listener that unsubscribed hears nothing more.
    session.prompt("/stop", Vec::new()).await.unwrap();
    session.prompt("/emit later", Vec::new()).await.unwrap();
    eventually("later", || {
        messages(custom_entries(&session, "all"))
            .last()
            .map(String::as_str)
            == Some("/emit command: later")
    })
    .await;
    assert_eq!(heard().len(), 3);

    // Nor does a native one, until it subscribes again.
    session.prompt("/mute", Vec::new()).await.unwrap();
    let extensions = session.extensions();
    let native = &extensions[1];
    let calls = native.settle().await;
    session.prompt("/tell quiet", Vec::new()).await.unwrap();
    // The event's call has run once the native runtime has started one more.
    for _ in 0..1000 {
        if native.settle().await > calls {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    session.prompt("/mute", Vec::new()).await.unwrap();
    session.prompt("/tell loud", Vec::new()).await.unwrap();
    eventually("loud", || notes.all().len() == 5).await;
    assert_eq!(notes.all()[4], "Event from js: loud");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_runtime_holds_a_bounded_queue_of_events() {
    const BUSY: &str = r#"
export default function (pi) {
	let ticks = 0;
	pi.events.on("tick", () => ticks++);
	pi.registerCommand("busy", {
		description: "Computes for a while",
		handler: async () => {
			const end = Date.now() + 1500;
			while (Date.now() < end) {}
		},
	});
	pi.registerCommand("count", { description: "Records the ticks", handler: async () => pi.appendEntry("ticks", ticks) });
}
"#;
    const BURST: &str = r#"
export default function (pi) {
	pi.registerCommand("burst", {
		description: "Emits many ticks",
		handler: async () => {
			for (let i = 0; i < 1000; i++) pi.events.emit("tick", i);
		},
	});
}
"#;
    let dir = scratch("bus-bounded");
    let engine = engine();
    // Two instances of the JS runtime, as trust domains will get.
    let busy = js(&engine, &dir, "busy", BUSY).await;
    let burst = js(&engine, &dir, "burst", BURST).await;
    let busy = session(&Faux::new([]), &dir, busy.for_session());
    let burst = session(&Faux::new([]), &dir, burst.for_session());
    for session in [&busy, &burst] {
        session
            .bind_extensions(Arc::new(Notes::default()), Mode::Print, None, None)
            .await;
    }
    let computing = tokio::spawn({
        let busy = busy.clone();
        async move { busy.prompt("/busy", Vec::new()).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    burst.prompt("/burst", Vec::new()).await.unwrap();
    computing.await.unwrap().unwrap();

    let ticks = || custom_entries(&busy, "ticks").last().cloned();
    for _ in 0..100 {
        busy.prompt("/count", Vec::new()).await.unwrap();
        if ticks() == Some(json!(64)) || ticks() == Some(json!(65)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    busy.prompt("/count", Vec::new()).await.unwrap();
    // The queue's 64, and the one on its way to the runtime.
    let count = ticks().unwrap().as_u64().unwrap();
    assert!((64..=65).contains(&count), "{count} ticks");
}
