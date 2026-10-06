#![doc = env!("CARGO_PKG_DESCRIPTION")]
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
//!         |params, _ctx| Ok(ToolResult::text(params["text"].as_str().unwrap_or_default().to_uppercase())),
//!     ));
//! }
//!
//! yapi_extension_api::extension!(init);
//! ```
//!
//! yapi loads a `.wasm` file listed where pi extensions are: `-e`, the
//! `extensions` directories, or a package's `yapi.extensions` manifest. Handlers
//! run synchronously; host actions such as [`notify`] and [`exec`] answer at
//! once. Events and results are pi's JSON shapes.

use std::cell::RefCell;
use std::collections::HashMap;

pub use serde_json::{Value, json};

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
}

impl Context {
    /// Whether a person can answer dialogs.
    pub fn has_ui(&self) -> bool {
        self.data["hasUI"] == true
    }

    /// The mode: `interactive`, `print`, `json` or `rpc`.
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

type ToolFn = Box<dyn Fn(&Value, &Context) -> Result<ToolResult, String>>;
type CommandFn = Box<dyn Fn(&str, &Context) -> Result<(), String>>;
type HandlerFn = Box<dyn Fn(&Value, &Context) -> Result<Option<Value>, String>>;

/// A tool for the model.
pub struct Tool {
    name: String,
    label: Option<String>,
    description: String,
    parameters: Value,
    prompt_snippet: Option<String>,
    prompt_guidelines: Vec<String>,
    execute: ToolFn,
}

impl Tool {
    /// A tool named `name` taking arguments that match the JSON Schema
    /// `parameters`.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        execute: impl Fn(&Value, &Context) -> Result<ToolResult, String> + 'static,
    ) -> Tool {
        Tool {
            name: name.into(),
            label: None,
            description: description.into(),
            parameters,
            prompt_snippet: None,
            prompt_guidelines: Vec::new(),
            execute: Box::new(execute),
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
    handler: CommandFn,
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
    handlers: Vec<(String, HandlerFn)>,
}

impl Api {
    /// Offers `tool` to the model.
    pub fn register_tool(&mut self, tool: Tool) {
        self.tools.retain(|existing| existing.name != tool.name);
        self.tools.push(tool);
    }

    /// Handles `/name args`.
    pub fn register_command(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        handler: impl Fn(&str, &Context) -> Result<(), String> + 'static,
    ) {
        self.commands.push(Command {
            name: name.into(),
            description: description.into(),
            handler: Box::new(handler),
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

    /// Runs `handler` for pi events of type `event`. Its result is the
    /// handler's return value in pi, such as `{"block": true, "reason": ...}`
    /// for `tool_call`.
    pub fn on(
        &mut self,
        event: impl Into<String>,
        handler: impl Fn(&Value, &Context) -> Result<Option<Value>, String> + 'static,
    ) {
        self.handlers.push((event.into(), Box::new(handler)));
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
            "messageRenderers": [],
            "entryRenderers": [],
            "markdownTransformer": false,
            "providers": [],
            "mcpServers": [],
        })
    }
}

/// Sends request `kind` to the host and returns its answer.
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
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn context(payload: &Value) -> Context {
    Context {
        data: payload["ctx"].clone(),
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

/// Runs a closure with the registered API, outside of the state borrow's
/// lifetime problems: handlers may call [`get_flag`].
fn with_api<T>(f: impl FnOnce(&Api) -> T) -> Result<T, String> {
    let api = STATE
        .with(|state| state.borrow_mut().api.take())
        .ok_or("extension not loaded")?;
    let result = f(&api);
    STATE.with(|state| state.borrow_mut().api = Some(api));
    Ok(result)
}

fn run(init: fn(&mut Api), kind: &str, payload: &Value) -> Result<Value, String> {
    match kind {
        "load" => {
            let entry = &payload["extensions"][0];
            STATE.with(|state| {
                let mut state = state.borrow_mut();
                state.id = entry["id"].as_u64().unwrap_or_default();
                state.path = entry["path"].as_str().unwrap_or_default().to_owned();
            });
            Ok(instantiate(init))
        }
        "reload" => Ok(instantiate(init)),
        "bind" | "shortcut" | "complete" => Ok(Value::Null),
        "flags" => {
            STATE.with(|state| {
                let mut state = state.borrow_mut();
                for (name, value) in payload["values"].as_object().into_iter().flatten() {
                    state.flag_values.insert(name.clone(), value.clone());
                }
            });
            Ok(Value::Null)
        }
        "tool" => {
            let ctx = context(payload);
            let name = payload["name"].as_str().unwrap_or_default();
            with_api(|api| {
                let tool = api
                    .tools
                    .iter()
                    .find(|tool| tool.name == name)
                    .ok_or_else(|| format!("Tool {name} is not registered"))?;
                (tool.execute)(&payload["params"], &ctx).map(|result| result.to_json())
            })?
        }
        "command" => {
            let ctx = context(payload);
            let name = payload["name"].as_str().unwrap_or_default();
            let args = payload["args"].as_str().unwrap_or_default();
            with_api(|api| {
                let command = api
                    .commands
                    .iter()
                    .find(|command| command.name == name)
                    .ok_or_else(|| format!("Command /{name} is not registered"))?;
                (command.handler)(args, &ctx).map(|()| Value::Null)
            })?
        }
        "emit" => {
            let ctx = context(payload);
            let event = &payload["event"];
            let kind = event["type"].as_str().unwrap_or_default();
            with_api(|api| {
                let mut result = Value::Null;
                let mut errors = Vec::new();
                for (_, handler) in api.handlers.iter().filter(|(name, _)| name == kind) {
                    match handler(event, &ctx) {
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
            })?
        }
        other => Err(format!("Unknown dispatch kind: {other}")),
    }
}

#[doc(hidden)]
pub fn dispatch(init: fn(&mut Api), id: u64, kind: &str, payload: &str) -> Vec<Outcome> {
    let payload: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let outcome = match run(init, kind, &payload) {
        Ok(value) => Outcome::Done((id, value.to_string())),
        Err(message) => Outcome::Failed((id, message)),
    };
    vec![outcome]
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
                _op: u64,
                _value: ::std::result::Result<::std::string::String, ::std::string::String>,
            ) -> ::std::vec::Vec<$crate::bindings::yapi::extension::types::Outcome> {
                ::std::vec::Vec::new()
            }

            fn render(_handle: u32, _width: u32) -> ::std::vec::Vec<::std::string::String> {
                ::std::vec::Vec::new()
            }

            fn input(
                _handle: u32,
                _data: ::std::string::String,
            ) -> ::std::vec::Vec<$crate::bindings::yapi::extension::types::Outcome> {
                ::std::vec::Vec::new()
            }
        }

        $crate::bindings::__export_world!(__YapiExtension with_types_in $crate::bindings);
    };
}
