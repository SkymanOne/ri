//! Codemode scripts, as pi's sandbox worker (`runtime/worker.ts` in
//! `pi-codemode` 1.0.0) runs them: a fresh QuickJS runtime and context holding
//! only pi's prelude. The context has no Node shims, no host natives and no
//! module loader, so the script reaches the host only through the prelude's
//! bridge: tool and global calls become operations the host resolves, output
//! items become requests, and the script's end becomes the outcome of the
//! dispatch that started it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use rquickjs::function::{Rest, This};
use rquickjs::{
    CatchResultExt, CaughtError, Context, Function, Object, Persistent, Runtime, Value,
};

use crate::eval;
use crate::yapi::extension::host;
use crate::yapi::extension::types::Outcome;

/// pi's prelude, unchanged: globals, the tool proxy, store, output and errors.
const PRELUDE: &str = include_str!("../js/vendor/codemode-prelude.js");
/// pi's `memoryLimit`.
const MEMORY_LIMIT: usize = 256 * 1024 * 1024;
/// quickjs-wasi's `MAX_STACK_SIZE`: deep recursion throws a catchable error.
const STACK_SIZE: usize = 512 * 1024;

struct Script {
    runtime: Runtime,
    context: Context,
    api: Persistent<Object<'static>>,
}

thread_local! {
    static SCRIPT: RefCell<Option<Script>> = const { RefCell::new(None) };
    /// The dispatch whose outcome is the script's end.
    static CALL: Cell<u64> = const { Cell::new(0) };
    /// Prelude call ids by host operation.
    static OPS: RefCell<HashMap<u64, u64>> = RefCell::new(HashMap::new());
}

fn string(value: Option<&Value<'_>>) -> Option<String> {
    value?.as_string()?.to_string().ok()
}

fn done(json: serde_json::Value) {
    crate::push_outcome(Outcome::Done((CALL.get(), json.to_string())));
}

/// The prelude's `bridge(kind, a, b, c)`; called with primitives only.
fn bridge(kind: String, args: Rest<Value<'_>>) {
    let arg = |index: usize| args.0.get(index);
    match kind.as_str() {
        "call" | "global" => {
            let id = arg(0).and_then(Value::as_number).unwrap_or_default() as u64;
            let payload = serde_json::json!({
                "name": string(arg(1)).unwrap_or_default(),
                "args": string(arg(2)),
            });
            let op = crate::next_op();
            OPS.with(|ops| ops.borrow_mut().insert(op, id));
            host::start(op, &format!("codemode.{kind}"), &payload.to_string());
        }
        "output" => {
            let item = if string(arg(0)).as_deref() == Some("image") {
                serde_json::json!({
                    "type": "image",
                    "data": string(arg(1)).unwrap_or_default(),
                    "mimeType": string(arg(2)).unwrap_or_default(),
                })
            } else {
                serde_json::json!({"type": "text", "text": string(arg(1)).unwrap_or_default()})
            };
            let _ = host::request("codemode.output", &item.to_string());
        }
        "done" => {
            if arg(0).and_then(Value::as_bool) == Some(true) {
                done(serde_json::json!({
                    "ok": true,
                    "value": string(arg(1)),
                    "writes": string(arg(2)),
                }));
            } else {
                done(serde_json::json!({"ok": false, "error": string(arg(1)).unwrap_or_default()}));
            }
        }
        _ => {}
    }
}

/// pi's `describeException`: the error as the prelude describes errors.
fn describe(error: CaughtError<'_>) -> String {
    let (name, message, stack) = match &error {
        CaughtError::Exception(exception) => {
            let object = exception.as_object();
            let name: Option<String> = object.get("name").ok();
            (
                name.unwrap_or_else(|| "Error".into()),
                exception.message().unwrap_or_default(),
                exception.stack().unwrap_or_default(),
            )
        }
        other => ("Error".into(), other.to_string(), String::new()),
    };
    let head = if message.is_empty() {
        name.clone()
    } else {
        format!("{name}: {message}")
    };
    let stack = stack.trim_end();
    let stack = if stack.is_empty() {
        head
    } else {
        format!("{head}\n{stack}")
    };
    serde_json::json!({"name": name, "message": message, "stack": stack}).to_string()
}

/// The script's runtime, context and prelude API.
fn script() -> Option<(Runtime, Context, Persistent<Object<'static>>)> {
    SCRIPT.with(|script| {
        let script = script.borrow();
        let script = script.as_ref()?;
        Some((
            script.runtime.clone(),
            script.context.clone(),
            script.api.clone(),
        ))
    })
}

/// Runs queued jobs, then fails a script that waits on nothing that can ever
/// resume it.
fn drain() -> Vec<Outcome> {
    if let Some((runtime, context, api)) = script() {
        while let Ok(true) | Err(_) = runtime.execute_pending_job() {}
        context.with(|ctx| {
            let result = (|| -> rquickjs::Result<()> {
                let api = api.restore(&ctx)?;
                let stalled: Function<'_> = api.get("stalled")?;
                stalled.call::<_, ()>((This(api),))
            })();
            if let Err(error) = result.catch(&ctx) {
                done(serde_json::json!({"ok": false, "error": describe(error)}));
            }
        });
    }
    crate::take_outcomes()
}

/// Starts the script of dispatch `call`. `payload` holds the `code` and the
/// prelude's `tools`, `globals` and `store` arguments as JSON texts.
pub fn start(call: u64, payload: &str) -> Vec<Outcome> {
    CALL.set(call);
    let payload: serde_json::Value = serde_json::from_str(payload).unwrap_or_default();
    let text = |key: &str| payload[key].as_str().unwrap_or_default().to_owned();
    let setup = (|| -> rquickjs::Result<(Runtime, Context)> {
        let runtime = Runtime::new()?;
        runtime.set_memory_limit(MEMORY_LIMIT);
        runtime.set_max_stack_size(STACK_SIZE);
        let context = Context::full(&runtime)?;
        Ok((runtime, context))
    })();
    let Ok((runtime, context)) = setup else {
        done(serde_json::json!({"ok": false, "error": "{\"message\":\"Failed to load QuickJS\"}"}));
        return crate::take_outcomes();
    };
    let started = context.with(|ctx| {
        let result = (|| -> rquickjs::Result<Persistent<Object<'static>>> {
            let bridge = Function::new(ctx.clone(), bridge)?.with_name("bridge")?;
            let prelude: Function<'_> = eval(&ctx, PRELUDE, "codemode-prelude.js")?;
            let api: Object<'_> =
                prelude.call((bridge, text("tools"), text("globals"), text("store")))?;
            Ok(Persistent::save(&ctx, api))
        })();
        result.catch(&ctx).map_err(|error| describe(error))
    });
    let api = match started {
        Ok(api) => api,
        Err(error) => {
            done(serde_json::json!({"ok": false, "error": error}));
            return crate::take_outcomes();
        }
    };
    SCRIPT.with(|script| {
        *script.borrow_mut() = Some(Script {
            runtime,
            context: context.clone(),
            api: api.clone(),
        });
    });
    // The prefix shares the first line with the script, so line numbers match
    // the script as written.
    let source = format!("(async (tools, console) => {{{}\n}})", text("code"));
    context.with(|ctx| {
        let compiled = eval::<Function<'_>>(&ctx, &source, "codemode.js").catch(&ctx);
        let result = match compiled {
            Ok(function) => (|| -> rquickjs::Result<()> {
                let api = api.restore(&ctx)?;
                let run: Function<'_> = api.get("run")?;
                run.call::<_, ()>((This(api), function))
            })()
            .catch(&ctx),
            Err(error) => {
                done(serde_json::json!({"ok": false, "error": describe(error)}));
                Ok(())
            }
        };
        if let Err(error) = result {
            done(serde_json::json!({"ok": false, "error": describe(error)}));
        }
    });
    drain()
}

/// Completes operation `op` if the script started it. Successful results are
/// `{"json": <JSON text, or null for undefined>}`.
pub fn resolve(op: u64, value: &Result<String, String>) -> Option<Vec<Outcome>> {
    let id = OPS.with(|ops| ops.borrow_mut().remove(&op))?;
    let (_, context, api) = script()?;
    context.with(|ctx| {
        let result = (|| -> rquickjs::Result<()> {
            let api = api.restore(&ctx)?;
            let settle: Function<'_> = api.get("settle")?;
            match value {
                Ok(json) => {
                    let wrapper: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
                    let payload = wrapper["json"].as_str().map(str::to_owned);
                    settle.call::<_, ()>((This(api), id as f64, true, payload))
                }
                Err(message) => {
                    settle.call::<_, ()>((This(api), id as f64, false, message.clone()))
                }
            }
        })();
        if let Err(error) = result.catch(&ctx) {
            done(serde_json::json!({"ok": false, "error": describe(error)}));
        }
    });
    Some(drain())
}
