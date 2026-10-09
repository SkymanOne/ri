#![doc = env!("CARGO_PKG_DESCRIPTION")]
//!
//! Native WebAssembly extensions are unstable until yapi 1.0. The WIT world,
//! the Rust SDK and the host requests they use may change in any release
//! before then, and extensions may need to be rebuilt. Pi extensions from npm
//! use Pi's extension API and are not affected.
//!
//! An extension is a `cdylib` built for `wasm32-wasip2`. Its init function
//! registers what it offers; [`extension!`] exports it:
//!
//! ```ignore
//! use yapi_extension_api::{Api, Tool, ToolResult, json};
//!
//! fn init(api: &mut Api) {
//!     api.register_tool(Tool::new(
//!         "shout",
//!         "Repeats the text in capitals",
//!         json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
//!         |params, _ctx| async move {
//!             Ok(ToolResult::text(params["text"].as_str().unwrap_or_default().to_uppercase()))
//!         },
//!     ));
//! }
//!
//! yapi_extension_api::extension!(init);
//! ```
//!
//! yapi loads a `.wasm` file listed where pi extensions are: `-e`, the
//! `extensions` directories, or a package's `yapi.extensions` manifest.
//! Events and results are pi's JSON shapes.
//!
//! Handlers are `async`. Requests such as [`notify`] and [`exec`] answer at
//! once. [`sleep`], [`op`] and [`Process`] wait for host work, during which
//! other handlers run. [`spawn`] runs work in the background, after the
//! handler that started it has returned, as a Pi extension does with a
//! promise it does not await.
//!
//! A [`Component`] draws interface as a pi-tui component does.
//! [`Context::custom`] shows one with keyboard focus until it finishes, and
//! [`Context::set_widget`], [`Context::set_footer`] and
//! [`Context::set_header`] keep one on screen.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;

pub use serde_json::{Value, json};

mod process;
mod task;
mod ui;

pub use process::{Process, ProcessEvent};
use task::LocalFuture;
pub use task::{op, sleep, spawn};
pub use ui::{
    Component, CustomOptions, Done, EditorComponent, OverlayHandle, Placement, Theme, Widget,
    editor_action, editor_changed, editor_shortcut, editor_submit, parse_key, request_render,
    terminal_size, theme,
};
#[cfg(feature = "widgets")]
pub mod widgets;

// The generated bindings name this crate by its external path.
extern crate self as yapi_extension_api;

#[doc(hidden)]
pub use wit_bindgen as __wit_bindgen;

#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "../../wit/since_v0.1.0",
        world: "extension",
        pub_export_macro: true,
        export_macro_name: "__export_world",
        default_bindings_module: "yapi_extension_api::bindings",
        runtime_path: "yapi_extension_api::__wit_bindgen::rt",
    });
}

use bindings::yapi::extension::host;
use bindings::yapi::extension::types::Outcome;

/// What a handler, tool or command runs in: pi's `ctx` as JSON.
#[derive(Clone, Debug)]
pub struct Context {
    data: Value,
    tool_call_id: Option<String>,
}

impl Context {
    /// Whether a person can answer dialogs.
    pub fn has_ui(&self) -> bool {
        self.data["hasUI"] == true
    }

    /// The mode: `tui`, `print`, `json` or `rpc`.
    pub fn mode(&self) -> &str {
        self.data["mode"].as_str().unwrap_or("print")
    }

    /// The session's working directory.
    pub fn cwd(&self) -> Option<&str> {
        self.data["cwd"].as_str()
    }

    /// Everything the host sent.
    pub fn data(&self) -> &Value {
        &self.data
    }

    /// Shows `partial` as the running tool's progress, as pi's `onUpdate`
    /// does. Does nothing outside a tool.
    pub fn update(&self, partial: &ToolResult) {
        if let Some(id) = &self.tool_call_id {
            let _ = request(
                "tool.update",
                &json!({"toolCallId": id, "partial": partial.to_json()}),
            );
        }
    }
}

/// What a tool returns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolResult {
    /// Content blocks for the model, such as `{"type": "text", "text": ...}`.
    pub content: Vec<Value>,
    /// Data kept with the result but not sent to the model.
    pub details: Option<Value>,
}

impl ToolResult {
    /// A result with one text block.
    pub fn text(text: impl Into<String>) -> ToolResult {
        ToolResult {
            content: vec![json!({"type": "text", "text": text.into()})],
            details: None,
        }
    }

    /// The result with `details`.
    pub fn with_details(mut self, details: Value) -> ToolResult {
        self.details = Some(details);
        self
    }

    fn to_json(&self) -> Value {
        let mut out = json!({"content": self.content});
        if let Some(details) = &self.details {
            out["details"] = details.clone();
        }
        out
    }
}

/// A registered handler: an async function of its input and the context.
type Handler<A, T> = Rc<dyn Fn(A, Context) -> LocalFuture<Result<T, String>>>;

fn handler<A, T, R>(f: impl Fn(A, Context) -> R + 'static) -> Handler<A, T>
where
    R: Future<Output = Result<T, String>> + 'static,
{
    Rc::new(move |input, ctx| Box::pin(f(input, ctx)))
}

/// A tool for the model.
pub struct Tool {
    name: String,
    label: Option<String>,
    description: String,
    parameters: Value,
    prompt_snippet: Option<String>,
    prompt_guidelines: Vec<String>,
    execute: Handler<Value, ToolResult>,
    render_call: Option<Rc<CallRenderer>>,
    render_result: Option<Rc<ResultRenderer>>,
    render_shell: Option<String>,
    execution_mode: Option<String>,
}

type CallRenderer = dyn Fn(&Value, &mut RenderContext) -> Option<Box<dyn Component>>;
type ResultRenderer = dyn Fn(&Value, &Value, &mut RenderContext) -> Option<Box<dyn Component>>;
type MessageRenderer = dyn Fn(&Value, &Value) -> Option<Box<dyn Component>>;

/// What a tool's renderer draws for; pi's `ToolRenderContext`.
pub struct RenderContext<'a> {
    /// pi's fields: `args`, `toolCallId`, `cwd`, `executionStarted`,
    /// `argsComplete`, `isPartial`, `expanded`, `showImages` and `isError`.
    pub data: &'a Value,
    /// State the renderers of one tool call share across renders, `{}` at
    /// first.
    pub state: &'a mut Value,
}

impl Tool {
    /// A tool named `name` taking arguments that match the JSON Schema
    /// `parameters`. When the run is aborted, yapi drops the future
    /// `execute` returned, which kills the processes it holds.
    pub fn new<R>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        execute: impl Fn(Value, Context) -> R + 'static,
    ) -> Tool
    where
        R: Future<Output = Result<ToolResult, String>> + 'static,
    {
        Tool {
            name: name.into(),
            label: None,
            description: description.into(),
            parameters,
            prompt_snippet: None,
            prompt_guidelines: Vec::new(),
            execute: handler(execute),
            render_call: None,
            render_result: None,
            render_shell: None,
            execution_mode: None,
        }
    }

    /// The name shown in the UI.
    pub fn label(mut self, label: impl Into<String>) -> Tool {
        self.label = Some(label.into());
        self
    }

    /// The line the system prompt lists the tool with.
    pub fn prompt_snippet(mut self, snippet: impl Into<String>) -> Tool {
        self.prompt_snippet = Some(snippet.into());
        self
    }

    /// A rule added to the system prompt while the tool is active.
    pub fn prompt_guideline(mut self, guideline: impl Into<String>) -> Tool {
        self.prompt_guidelines.push(guideline.into());
        self
    }

    /// pi's `renderCall`: the component the transcript shows for a call
    /// with arguments `args`, or `None` for yapi's own rendering. Call
    /// [`request_render`] to draw it again.
    pub fn render_call(
        mut self,
        render: impl Fn(&Value, &mut RenderContext) -> Option<Box<dyn Component>> + 'static,
    ) -> Tool {
        self.render_call = Some(Rc::new(render));
        draws();
        self
    }

    /// pi's `renderResult`: the component the transcript shows for a result
    /// `{"content", "details"}` with options `{"expanded", "isPartial"}`, or
    /// `None` for yapi's own rendering.
    pub fn render_result(
        mut self,
        render: impl Fn(&Value, &Value, &mut RenderContext) -> Option<Box<dyn Component>> + 'static,
    ) -> Tool {
        self.render_result = Some(Rc::new(render));
        draws();
        self
    }

    /// pi's `executionMode`: `sequential` to run alone, after the calls
    /// before it, or `parallel`.
    pub fn execution_mode(mut self, mode: impl Into<String>) -> Tool {
        self.execution_mode = Some(mode.into());
        self
    }

    /// pi's `renderShell`: `self` when the renderers draw the tool's whole
    /// box, or `default`.
    pub fn render_shell(mut self, shell: impl Into<String>) -> Tool {
        self.render_shell = Some(shell.into());
        self
    }
}

/// A flag's value type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagType {
    /// `--name`.
    Boolean,
    /// `--name value`.
    String,
}

struct Command {
    name: String,
    description: String,
    handler: Handler<String, ()>,
}

struct Flag {
    name: String,
    kind: FlagType,
    default: Value,
    description: String,
}

/// What an extension registers; pi's `ExtensionAPI`.
#[derive(Default)]
pub struct Api {
    tools: Vec<Tool>,
    commands: Vec<Command>,
    flags: Vec<Flag>,
    handlers: Vec<(String, Handler<Value, Option<Value>>)>,
    message_renderers: Vec<(String, Rc<MessageRenderer>)>,
}

impl Api {
    /// Offers `tool` to the model.
    pub fn register_tool(&mut self, tool: Tool) {
        self.tools.retain(|existing| existing.name != tool.name);
        self.tools.push(tool);
    }

    /// Handles `/name args`.
    pub fn register_command<R>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        run: impl Fn(String, Context) -> R + 'static,
    ) where
        R: Future<Output = Result<(), String>> + 'static,
    {
        self.commands.push(Command {
            name: name.into(),
            description: description.into(),
            handler: handler(run),
        });
    }

    /// Accepts `--name` on the command line; read it with [`get_flag`].
    pub fn register_flag(
        &mut self,
        name: impl Into<String>,
        kind: FlagType,
        default: Value,
        description: impl Into<String>,
    ) {
        self.flags.push(Flag {
            name: name.into(),
            kind,
            default,
            description: description.into(),
        });
    }

    /// Runs `run` for pi events of type `event`. Its result is the
    /// handler's return value in pi, such as `{"block": true, "reason": ...}`
    /// for `tool_call`. yapi waits for it before the event's next handler.
    pub fn on<R>(&mut self, event: impl Into<String>, run: impl Fn(Value, Context) -> R + 'static)
    where
        R: Future<Output = Result<Option<Value>, String>> + 'static,
    {
        self.handlers.push((event.into(), handler(run)));
    }

    /// pi's `registerMessageRenderer`: the component the transcript shows
    /// for custom messages of type `custom_type`, from the message and the
    /// options `{"expanded"}`, or `None` for yapi's own rendering.
    pub fn register_message_renderer(
        &mut self,
        custom_type: impl Into<String>,
        render: impl Fn(&Value, &Value) -> Option<Box<dyn Component>> + 'static,
    ) {
        self.message_renderers
            .push((custom_type.into(), Rc::new(render)));
        draws();
    }

    fn describe(&self, id: u64, path: &str) -> Value {
        let mut events: Vec<&str> = self
            .handlers
            .iter()
            .map(|(event, _)| event.as_str())
            .collect();
        events.sort_unstable();
        events.dedup();
        json!({
            "id": id,
            "path": path,
            "tools": self.tools.iter().map(|tool| json!({
                "name": tool.name,
                "label": tool.label,
                "description": tool.description,
                "parameters": tool.parameters,
                "promptSnippet": tool.prompt_snippet,
                "promptGuidelines": tool.prompt_guidelines,
                "renderShell": tool.render_shell,
                "executionMode": tool.execution_mode,
                "hasRenderCall": tool.render_call.is_some(),
                "hasRenderResult": tool.render_result.is_some(),
            })).collect::<Vec<_>>(),
            "commands": self.commands.iter().map(|command| json!({
                "name": command.name, "description": command.description, "hasCompletions": false,
            })).collect::<Vec<_>>(),
            "flags": self.flags.iter().map(|flag| json!({
                "name": flag.name,
                "type": if flag.kind == FlagType::Boolean { "boolean" } else { "string" },
                "default": flag.default,
                "description": flag.description,
            })).collect::<Vec<_>>(),
            "shortcuts": [],
            "events": events,
            "messageRenderers": self.message_renderers.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            "entryRenderers": [],
            "markdownTransformer": false,
            "providers": [],
            "mcpServers": [],
        })
    }
}

/// Sends request `kind` with JSON `payload` to the host and returns its
/// answer, `null` when the host answers nothing. A request the host refuses
/// returns its message as the error.
///
/// yapi supports these kinds for native extensions:
///
/// | Kind | Payload | Answer |
/// |---|---|---|
/// | `log` | `{"level", "message"}`, with `level` one of `debug`, `info`, `warn` and `error` | `null`. yapi treats `message` as console output from a Pi extension. |
/// | `cwd` | `{}` | The working directory extensions see, as a string. |
/// | `exec.sync` | `{"command", "args", "cwd", "env", "input", "timeout"}`, all but `command` optional | `{"stdout", "stderr", "code", "signal", "killed"}` once the process exits. See below. |
/// | `ui.notify` | `{"message", "type"}`, with `type` one of `info`, `warning` and `error`, or absent | `null`. Pi's `ctx.ui.notify`. |
/// | `session.sendMessage` | `{"message": {"customType", "content", "display", "details"}, "options": {"triggerTurn", "deliverAs"}}` | `null`. Pi's `pi.sendMessage`, with the same message and options. |
/// | `session.appendEntry` | `{"customType", "data"}` | `null`. Pi's `pi.appendEntry`. |
/// | `session.read` | `{"method", "args"}` | The result of Pi's `ctx.sessionManager.<method>(...args)`. See below. |
///
/// `exec.sync` runs `command` with the strings in `args` and waits for it.
/// `cwd` defaults to yapi's working directory, `env` replaces the whole
/// environment, `input` is written to standard input, and `timeout` kills the
/// process after that many milliseconds. `code` is `null` when a signal ended
/// the process, and `killed` is `true` when the timeout did. A process that
/// cannot start answers `{"stdout": "", "stderr": "", "code": null, "error"}`,
/// where `error` is Node's spawn error, such as `spawn git ENOENT`. The
/// request fails when the extension may not run processes.
///
/// `session.read` takes the method's arguments as the array `args`, such as
/// `["<entry id>"]` for `getEntry`. The methods are `getCwd`,
/// `getSessionDir`, `getSessionId`, `getSessionFile`, `getSessionName`,
/// `isPersisted`, `getHeader`, `getEntries`, `getEntry`, `getChildren`,
/// `getLabel`, `getLeafId`, `getLeafEntry`, `getBranch` and
/// `buildSessionContext`. Entries have the shapes of Pi's session files.
///
/// `ui.notify` and the `session` requests fail while the init function runs,
/// before yapi binds the extension to a session. Every other kind is internal
/// to yapi and unstable: it may change or disappear in any release.
pub fn request(kind: &str, payload: &Value) -> Result<Value, String> {
    let text = host::request(kind, &payload.to_string())?;
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|err| err.to_string())
}

/// Shows `message`; `level` is `info`, `warning` or `error`.
pub fn notify(message: &str, level: &str) {
    let _ = request("ui.notify", &json!({"message": message, "type": level}));
}

/// pi's `sendMessage`: adds a custom message to the session.
pub fn send_message(custom_type: &str, content: &str, display: bool) -> Result<(), String> {
    request(
        "session.sendMessage",
        &json!({"message": {"customType": custom_type, "content": content, "display": display}, "options": {}}),
    )
    .map(|_| ())
}

/// pi's `appendEntry`: stores `data` in the session file.
pub fn append_entry(custom_type: &str, data: Value) -> Result<(), String> {
    request(
        "session.appendEntry",
        &json!({"customType": custom_type, "data": data}),
    )
    .map(|_| ())
}

/// A finished process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecOutput {
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Exit code; `None` when a signal ended it.
    pub code: Option<i32>,
}

/// Runs `command` with `args` in `cwd` and waits for it.
pub fn exec(command: &str, args: &[&str], cwd: Option<&str>) -> Result<ExecOutput, String> {
    let result = request(
        "exec.sync",
        &json!({"command": command, "args": args, "cwd": cwd}),
    )?;
    if let Some(error) = result["error"].as_str() {
        return Err(error.to_owned());
    }
    Ok(ExecOutput {
        stdout: result["stdout"].as_str().unwrap_or_default().to_owned(),
        stderr: result["stderr"].as_str().unwrap_or_default().to_owned(),
        code: result["code"].as_i64().map(|code| code as i32),
    })
}

/// The value of flag `name`: from the command line, else its default.
pub fn get_flag(name: &str) -> Option<Value> {
    STATE.with(|state| {
        let state = state.borrow();
        state.flag_values.get(name).cloned().or_else(|| {
            state
                .api
                .as_ref()?
                .flags
                .iter()
                .find(|flag| flag.name == name)
                .map(|flag| flag.default.clone())
        })
    })
}

#[derive(Default)]
struct State {
    api: Option<Api>,
    id: u64,
    path: String,
    flag_values: HashMap<String, Value>,
    /// Running calls the host may abort, by the host's id for them: the
    /// task and the call's id.
    abortable: HashMap<u64, (u64, u64)>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn context(payload: &Value) -> Context {
    Context {
        data: payload["ctx"].clone(),
        tool_call_id: payload["toolCallId"].as_str().map(str::to_owned),
    }
}

fn instantiate(init: fn(&mut Api)) -> Value {
    let mut api = Api::default();
    init(&mut api);
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let description = api.describe(state.id, &state.path);
        state.api = Some(api);
        json!({"extensions": [description]})
    })
}

/// What `find` picks from the registered API.
fn registered<T>(find: impl FnOnce(&Api) -> Option<T>) -> Option<T> {
    STATE.with(|state| state.borrow().api.as_ref().and_then(find))
}

/// Call `kind` with `payload`: what it answers once it completes.
fn call(init: fn(&mut Api), kind: &str, payload: Value) -> LocalFuture<Result<Value, String>> {
    let answer = |value: Result<Value, String>| -> LocalFuture<Result<Value, String>> {
        Box::pin(std::future::ready(value))
    };
    match kind {
        "load" => {
            let entry = &payload["extensions"][0];
            STATE.with(|state| {
                let mut state = state.borrow_mut();
                state.id = entry["id"].as_u64().unwrap_or_default();
                state.path = entry["path"].as_str().unwrap_or_default().to_owned();
            });
            answer(Ok(instantiate(init)))
        }
        "reload" => {
            ui::reset();
            answer(Ok(instantiate(init)))
        }
        "bind" => {
            ui::bind();
            #[cfg(feature = "widgets")]
            widgets::bind();
            answer(Ok(Value::Null))
        }
        "mouse" => answer(Ok(Value::Bool(ui::mouse(&payload)))),
        "editor" => {
            ui::editor_op(&payload);
            answer(Ok(Value::Null))
        }
        "component" => answer(Ok(DRAW.get().map_or(Value::Null, |draw| draw(&payload)))),
        "shortcut" | "complete" | "resize" => answer(Ok(Value::Null)),
        "flags" => {
            STATE.with(|state| {
                let mut state = state.borrow_mut();
                for (name, value) in payload["values"].as_object().into_iter().flatten() {
                    state.flag_values.insert(name.clone(), value.clone());
                }
            });
            answer(Ok(Value::Null))
        }
        "tool" => {
            let ctx = context(&payload);
            let name = payload["name"].as_str().unwrap_or_default().to_owned();
            let execute = registered(|api| {
                let tool = api.tools.iter().find(|tool| tool.name == name)?;
                Some(tool.execute.clone())
            });
            Box::pin(async move {
                let execute = execute.ok_or_else(|| format!("Tool {name} is not registered"))?;
                let result = execute(payload["params"].clone(), ctx).await?;
                Ok(result.to_json())
            })
        }
        "command" => {
            let ctx = context(&payload);
            let name = payload["name"].as_str().unwrap_or_default().to_owned();
            let run = registered(|api| {
                let command = api.commands.iter().find(|command| command.name == name)?;
                Some(command.handler.clone())
            });
            Box::pin(async move {
                let run = run.ok_or_else(|| format!("Command /{name} is not registered"))?;
                run(payload["args"].as_str().unwrap_or_default().to_owned(), ctx).await?;
                Ok(Value::Null)
            })
        }
        "emit" => {
            let ctx = context(&payload);
            let event = payload["event"].clone();
            let kind = event["type"].as_str().unwrap_or_default().to_owned();
            let handlers: Vec<_> = registered(|api| {
                Some(
                    api.handlers
                        .iter()
                        .filter(|(name, _)| *name == kind)
                        .map(|(_, handler)| handler.clone())
                        .collect(),
                )
            })
            .unwrap_or_default();
            Box::pin(async move {
                let mut result = Value::Null;
                let mut errors = Vec::new();
                for handler in handlers {
                    match handler(event.clone(), ctx.clone()).await {
                        Ok(Some(value)) => {
                            let blocks = kind == "tool_call" && value["block"] == true;
                            result = value;
                            if blocks {
                                break;
                            }
                        }
                        Ok(None) => {}
                        Err(error) if kind == "tool_call" => return Err(error),
                        Err(error) => errors.push(json!({"error": error})),
                    }
                }
                Ok(json!({"result": result, "errors": errors}))
            })
        }
        "abort" => {
            let aborted = payload["id"]
                .as_u64()
                .and_then(|id| STATE.with(|state| state.borrow_mut().abortable.remove(&id)));
            if let Some((task, call)) = aborted
                && task::cancel(task)
            {
                task::report(Outcome::Failed((call, "This operation was aborted".into())));
            }
            answer(Ok(Value::Null))
        }
        other => answer(Err(format!("Unknown dispatch kind: {other}"))),
    }
}

type Draw = fn(&Value) -> Value;

thread_local! {
    /// Answers the `component` call once a renderer is registered, so
    /// extensions without renderers leave out the code.
    static DRAW: std::cell::Cell<Option<Draw>> = const { std::cell::Cell::new(None) };
}

fn draws() {
    DRAW.set(Some(component));
}

/// The `component` call: what a registered renderer draws.
fn component(payload: &Value) -> Value {
    if payload["kind"] == "message" {
        let custom_type = &payload["message"]["customType"];
        let renderer = registered(|api| {
            let (_, renderer) = api
                .message_renderers
                .iter()
                .find(|(name, _)| custom_type == name)?;
            Some(renderer.clone())
        });
        return ui::transcript(payload, |_| {
            renderer?(&payload["message"], &payload["options"])
        });
    }
    let call = payload["kind"] == "toolCall";
    let name = &payload["name"];
    let renderers = registered(|api| {
        let tool = api.tools.iter().find(|tool| name == tool.name.as_str())?;
        Some((tool.render_call.clone(), tool.render_result.clone()))
    });
    ui::transcript(payload, |state| {
        let (render_call, render_result) = renderers?;
        let mut data = payload["context"].clone();
        data["args"] = payload["args"].clone();
        data["toolCallId"] = payload["toolCallId"].clone();
        let mut ctx = RenderContext { data: &data, state };
        if call {
            render_call?(&payload["args"], &mut ctx)
        } else {
            render_result?(&payload["result"], &payload["options"], &mut ctx)
        }
    })
}

#[doc(hidden)]
pub fn dispatch(init: fn(&mut Api), id: u64, kind: &str, payload: &str) -> Vec<Outcome> {
    let payload: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    // The host aborts a tool call by the id in its payload.
    let abortable = payload["id"].as_u64().filter(|_| kind == "tool");
    let answer = call(init, kind, payload);
    let task = spawn(async move {
        let outcome = match answer.await {
            Ok(value) => Outcome::Done((id, value.to_string())),
            Err(message) => Outcome::Failed((id, message)),
        };
        if let Some(op) = abortable {
            STATE.with(|state| state.borrow_mut().abortable.remove(&op));
        }
        task::report(outcome);
    });
    if let Some(op) = abortable {
        STATE.with(|state| state.borrow_mut().abortable.insert(op, (task, id)));
    }
    task::run()
}

#[doc(hidden)]
pub fn resolve(op: u64, value: Result<String, String>) -> Vec<Outcome> {
    task::resolve(op, value);
    task::run()
}

#[doc(hidden)]
pub fn render(handle: u32, width: u32) -> Vec<String> {
    ui::render(handle, width)
}

#[doc(hidden)]
pub fn input(handle: u32, data: &str) -> Vec<Outcome> {
    ui::input(handle, data);
    task::run()
}

/// Exports an extension whose init function is `$init: fn(&mut Api)`.
#[macro_export]
macro_rules! extension {
    ($init:path) => {
        struct __YapiExtension;

        impl $crate::bindings::exports::yapi::extension::guest::Guest for __YapiExtension {
            fn dispatch(
                id: u64,
                kind: ::std::string::String,
                payload: ::std::string::String,
            ) -> ::std::vec::Vec<$crate::bindings::yapi::extension::types::Outcome> {
                $crate::dispatch($init, id, &kind, &payload)
            }

            fn resolve(
                op: u64,
                value: ::std::result::Result<::std::string::String, ::std::string::String>,
            ) -> ::std::vec::Vec<$crate::bindings::yapi::extension::types::Outcome> {
                $crate::resolve(op, value)
            }

            fn render(handle: u32, width: u32) -> ::std::vec::Vec<::std::string::String> {
                $crate::render(handle, width)
            }

            fn input(
                handle: u32,
                data: ::std::string::String,
            ) -> ::std::vec::Vec<$crate::bindings::yapi::extension::types::Outcome> {
                $crate::input(handle, &data)
            }
        }

        $crate::bindings::__export_world!(__YapiExtension with_types_in $crate::bindings);
    };
}
