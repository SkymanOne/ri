//! A todo list the model manages through a `todo` tool, and a `/todos`
//! command that shows it.
//!
//! A port of pi's `todo.ts` example. Each tool result carries the whole list
//! in its details, and the list is rebuilt from the session's current branch
//! when a session starts or `/tree` moves to another entry, so every branch
//! keeps its own list.

use std::cell::RefCell;

use yapi_extension_api::{Api, Tool, ToolResult, Value, json, notify, request};

#[derive(Clone)]
struct State {
    /// Items as `{"id", "text", "done"}`, in the order they were added.
    todos: Vec<Value>,
    next_id: u64,
}

thread_local! {
    static STATE: RefCell<State> = const { RefCell::new(State { todos: Vec::new(), next_id: 1 }) };
}

/// The list as the last `todo` result on the current branch left it.
fn rebuild() -> Result<(), String> {
    let branch = request("session.read", &json!({"method": "getBranch", "args": []}))?;
    let mut state = State {
        todos: Vec::new(),
        next_id: 1,
    };
    for entry in branch.as_array().into_iter().flatten() {
        let message = &entry["message"];
        if entry["type"] == "message"
            && message["role"] == "toolResult"
            && message["toolName"] == "todo"
        {
            let details = &message["details"];
            state.todos = details["todos"].as_array().cloned().unwrap_or_default();
            state.next_id = details["nextId"].as_u64().unwrap_or(1);
        }
    }
    STATE.with(|cell| *cell.borrow_mut() = state);
    Ok(())
}

fn listing(todos: &[Value]) -> String {
    if todos.is_empty() {
        return "No todos".into();
    }
    let lines: Vec<String> = todos
        .iter()
        .map(|todo| {
            let mark = if todo["done"] == true { "x" } else { " " };
            format!(
                "[{mark}] #{}: {}",
                todo["id"],
                todo["text"].as_str().unwrap_or_default()
            )
        })
        .collect();
    lines.join("\n")
}

/// Runs `action` on the list: the result text and an error, if any.
fn apply(state: &mut State, params: &Value) -> (String, Option<String>) {
    match params["action"].as_str().unwrap_or_default() {
        "list" => (listing(&state.todos), None),
        "add" => match params["text"].as_str().filter(|text| !text.is_empty()) {
            None => (
                "Error: text required for add".into(),
                Some("text required".into()),
            ),
            Some(text) => {
                let id = state.next_id;
                state.next_id += 1;
                state
                    .todos
                    .push(json!({"id": id, "text": text, "done": false}));
                (format!("Added todo #{id}: {text}"), None)
            }
        },
        "toggle" => {
            let Some(id) = params["id"].as_u64() else {
                return (
                    "Error: id required for toggle".into(),
                    Some("id required".into()),
                );
            };
            match state.todos.iter_mut().find(|todo| todo["id"] == id) {
                None => (
                    format!("Todo #{id} not found"),
                    Some(format!("#{id} not found")),
                ),
                Some(todo) => {
                    let done = todo["done"] != true;
                    todo["done"] = json!(done);
                    let word = if done { "completed" } else { "uncompleted" };
                    (format!("Todo #{id} {word}"), None)
                }
            }
        }
        "clear" => {
            let count = state.todos.len();
            *state = State {
                todos: Vec::new(),
                next_id: 1,
            };
            (format!("Cleared {count} todos"), None)
        }
        other => (
            format!("Unknown action: {other}"),
            Some(format!("unknown action: {other}")),
        ),
    }
}

fn init(api: &mut Api) {
    api.on("session_start", |_event, _ctx| rebuild().map(|()| None));
    api.on("session_tree", |_event, _ctx| rebuild().map(|()| None));
    api.register_tool(
        Tool::new(
            "todo",
            "Manage a todo list. Actions: list, add (text), toggle (id), clear",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "add", "toggle", "clear"]},
                    "text": {"type": "string", "description": "Todo text (for add)"},
                    "id": {"type": "number", "description": "Todo ID (for toggle)"},
                },
                "required": ["action"],
            }),
            |params, _ctx| {
                STATE.with(|cell| {
                    let mut state = cell.borrow_mut();
                    let (text, error) = apply(&mut state, params);
                    let mut details = json!({
                        "action": params["action"],
                        "todos": state.todos,
                        "nextId": state.next_id,
                    });
                    if let Some(error) = error {
                        details["error"] = json!(error);
                    }
                    Ok(ToolResult::text(text).with_details(details))
                })
            },
        )
        .label("Todo"),
    );
    api.register_command(
        "todos",
        "Show all todos on the current branch",
        |_args, _ctx| {
            let todos = STATE.with(|cell| cell.borrow().todos.clone());
            let done = todos.iter().filter(|todo| todo["done"] == true).count();
            notify(
                &format!("{done}/{} completed\n{}", todos.len(), listing(&todos)),
                "info",
            );
            Ok(())
        },
    );
}

yapi_extension_api::extension!(init);
