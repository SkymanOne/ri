//! The JS runtime component: QuickJS-NG with Node shims and pi's extension API.
//!
//! The host drives it through the `ri:extension` world. Each export runs the
//! JS job queue until it is empty and returns the outcomes JS produced. JS
//! reaches the host through four natives: `request` answers at once, `start`
//! begins an operation whose result a later `resolve` delivers, and `done` and
//! `fail` finish calls. Module sources come from the host, except the builtins
//! embedded here: the Node shims, pi's API facade and the vendored pi packages.

wit_bindgen::generate!({ path: "../../wit/since_v0.1.0", world: "extension" });

mod builtins;
mod fs;

use std::cell::{Cell, RefCell};

use exports::ri::extension::guest::Guest;
use ri::extension::host;
use ri::extension::types::Outcome;
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::{CatchResultExt, Context, Ctx, Function, Module, Object, Runtime, Value};

/// QuickJS's own limit; the host bounds the whole instance's memory.
const STACK_SIZE: usize = 1024 * 1024;

thread_local! {
    static JS: RefCell<Option<(Runtime, Context)>> = const { RefCell::new(None) };
    static OUTCOMES: RefCell<Vec<Outcome>> = const { RefCell::new(Vec::new()) };
    static NEXT_OP: Cell<u64> = const { Cell::new(1) };
}

/// Resolves specifiers: builtins by name, relative imports inside builtins
/// within their package, everything else through the host.
struct HostResolver;

impl Resolver for HostResolver {
    fn resolve<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        if let Some(builtin) = builtins::resolve(base, name) {
            return Ok(builtin);
        }
        let payload = format!(
            "{{\"specifier\":{},\"referrer\":{}}}",
            json_string(name),
            json_string(base)
        );
        let path = host::request("module.resolve", &payload)
            .map_err(|message| rquickjs::Exception::throw_message(ctx, &message))?;
        match serde_json::from_str::<serde_json::Value>(&path) {
            Ok(serde_json::Value::String(path)) => Ok(path),
            _ => Err(rquickjs::Exception::throw_message(
                ctx,
                &format!("Cannot find module '{name}'"),
            )),
        }
    }
}

/// Loads builtins from the embedded table and files through the host. The host
/// returns an ES module's source, or marks a file CommonJS: such a file runs
/// through `__ri_cjs` and its exports become the ES module's exports.
struct HostLoader;

impl Loader for HostLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        if let Some(source) = builtins::source(ctx, name) {
            return Module::declare(ctx.clone(), name, source?);
        }
        let payload = format!("{{\"path\":{}}}", json_string(name));
        let response = host::request("module.load", &payload)
            .map_err(|message| rquickjs::Exception::throw_message(ctx, &message))?;
        let response: serde_json::Value = serde_json::from_str(&response).unwrap_or_default();
        let source = match response["source"].as_str() {
            Some(source) => source.to_owned(),
            None => cjs_module(ctx, name)?,
        };
        Module::declare(ctx.clone(), name, source)
    }
}

/// An ES module that re-exports CommonJS module `path`, which runs now.
fn cjs_module(ctx: &Ctx<'_>, path: &str) -> rquickjs::Result<String> {
    let ri: Object<'_> = ctx.globals().get("__ri")?;
    let exports: Function<'_> = ri.get("cjsExports")?;
    let names: Vec<String> = exports.call((path,))?;
    let mut source = format!(
        "const m = globalThis.__ri_cjs({});\nexport default m !== null && typeof m === \"object\" && m.__esModule && \"default\" in m ? m.default : m;\n",
        json_string(path)
    );
    let list: Vec<String> = names
        .iter()
        .enumerate()
        .map(|(index, export)| {
            source.push_str(&format!("const e{index} = m.{export};\n"));
            format!("e{index} as {export}")
        })
        .collect();
    source.push_str(&format!("export {{ {} }};\n", list.join(", ")));
    Ok(source)
}

/// A JSON string literal.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn install_natives(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    let native = Object::new(ctx.clone())?;
    native.set(
        "request",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'_>, kind: String, payload: String| {
                host::request(&kind, &payload)
                    .map_err(|message| rquickjs::Exception::throw_message(&ctx, &message))
            },
        )?,
    )?;
    native.set(
        "start",
        Function::new(ctx.clone(), |kind: String, payload: String| {
            let op = NEXT_OP.with(|next| {
                let op = next.get();
                next.set(op + 1);
                op
            });
            host::start(op, &kind, &payload);
            op as f64
        })?,
    )?;
    native.set(
        "done",
        Function::new(ctx.clone(), |id: f64, json: String| {
            OUTCOMES.with(|outcomes| outcomes.borrow_mut().push(Outcome::Done((id as u64, json))));
        })?,
    )?;
    native.set(
        "fail",
        Function::new(ctx.clone(), |id: f64, message: String| {
            OUTCOMES.with(|outcomes| {
                outcomes
                    .borrow_mut()
                    .push(Outcome::Failed((id as u64, message)))
            });
        })?,
    )?;
    native.set(
        "fs",
        Function::new(ctx.clone(), |op: String, args: String| fs::call(&op, &args))?,
    )?;
    native.set(
        "segment",
        Function::new(
            ctx.clone(),
            |text: String, granularity: String| -> Vec<String> {
                use unicode_segmentation::UnicodeSegmentation as _;
                match granularity.as_str() {
                    "word" => text.split_word_bounds().map(str::to_owned).collect(),
                    "sentence" => text.split_sentence_bounds().map(str::to_owned).collect(),
                    _ => text.graphemes(true).map(str::to_owned).collect(),
                }
            },
        )?,
    )?;
    ctx.globals().set("__ri_native", native)?;
    Ok(())
}

/// The runtime and its context, created on first use.
fn js() -> (Runtime, Context) {
    JS.with(|js| {
        let mut js = js.borrow_mut();
        if js.is_none() {
            // A panic aborts the instance; report it first so the host can log why.
            std::panic::set_hook(Box::new(|info| {
                log_error(&format!("ri-js panicked: {info}"));
            }));
            let runtime = Runtime::new().expect("QuickJS runtime");
            runtime.set_max_stack_size(STACK_SIZE);
            runtime.set_loader(HostResolver, HostLoader);
            let context = Context::full(&runtime).expect("QuickJS context");
            context.with(|ctx| {
                install_natives(&ctx).expect("natives");
                // The runtime script sets up globals, shims and the dispatcher.
                let result: rquickjs::Result<Value<'_>> = ctx.eval(builtins::RUNTIME);
                if let Err(error) = result.catch(&ctx) {
                    log_error(&error.to_string());
                }
            });
            *js = Some((runtime, context));
        }
        js.clone().expect("initialized above")
    })
}

fn log_error(message: &str) {
    host::request(
        "log",
        &format!(
            "{{\"level\":\"error\",\"message\":{}}}",
            json_string(message)
        ),
    )
    .ok();
}

/// Runs `f` in the context.
fn with_ctx<T>(f: impl FnOnce(&Ctx<'_>) -> T) -> T {
    let (_, context) = js();
    context.with(|ctx| f(&ctx))
}

/// Runs jobs until none are left, then returns the outcomes.
fn drain() -> Vec<Outcome> {
    let (runtime, _) = js();
    loop {
        match runtime.execute_pending_job() {
            Ok(true) => {}
            Ok(false) => break,
            // A job's exception is reported by the promise it belongs to.
            Err(_) => {}
        }
    }
    OUTCOMES.with(|outcomes| std::mem::take(&mut *outcomes.borrow_mut()))
}

/// Calls `globalThis.__ri.<name>(args)`, reporting a synchronous exception as a
/// failure of call `id` when there is one.
fn call_runtime(
    name: &str,
    id: Option<u64>,
    args: impl for<'js> FnOnce(&Ctx<'js>) -> rquickjs::Result<rquickjs::function::Args<'js>>,
) -> Vec<Outcome> {
    with_ctx(|ctx| {
        let result = (|| -> rquickjs::Result<()> {
            let ri: Object<'_> = ctx.globals().get("__ri")?;
            let function: Function<'_> = ri.get(name)?;
            let args = args(ctx)?;
            function.call_arg::<()>(args)?;
            Ok(())
        })();
        if let Err(error) = result.catch(ctx) {
            let message = error.to_string();
            match id {
                Some(id) => OUTCOMES
                    .with(|outcomes| outcomes.borrow_mut().push(Outcome::Failed((id, message)))),
                None => log_error(&message),
            }
        }
    });
    drain()
}

struct Runtime_;

impl Guest for Runtime_ {
    fn dispatch(id: u64, kind: String, payload: String) -> Vec<Outcome> {
        call_runtime("dispatch", Some(id), |ctx| {
            let mut args = rquickjs::function::Args::new(ctx.clone(), 3);
            args.push_arg(id as f64)?;
            args.push_arg(kind)?;
            args.push_arg(payload)?;
            Ok(args)
        })
    }

    fn resolve(op: u64, value: Result<String, String>) -> Vec<Outcome> {
        call_runtime("resolve", None, |ctx| {
            let (ok, text) = match value {
                Ok(json) => (true, json),
                Err(message) => (false, message),
            };
            let mut args = rquickjs::function::Args::new(ctx.clone(), 3);
            args.push_arg(op as f64)?;
            args.push_arg(ok)?;
            args.push_arg(text)?;
            Ok(args)
        })
    }

    fn render(handle: u32, width: u32) -> Vec<String> {
        with_ctx(|ctx| {
            let result = (|| -> rquickjs::Result<Vec<String>> {
                let ri: Object<'_> = ctx.globals().get("__ri")?;
                let function: Function<'_> = ri.get("render")?;
                function.call((handle, width))
            })();
            result.catch(ctx).unwrap_or_default()
        })
    }

    fn input(handle: u32, data: String) -> Vec<Outcome> {
        call_runtime("input", None, |ctx| {
            let mut args = rquickjs::function::Args::new(ctx.clone(), 2);
            args.push_arg(handle)?;
            args.push_arg(data)?;
            Ok(args)
        })
    }
}

export!(Runtime_);
