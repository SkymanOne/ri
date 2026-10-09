//! The Rust SDK's example extensions (`guest/examples`) in sessions driven by
//! the faux provider.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{
    cli_source, custom_entries, engine, options, scratch, session, session_with_tools, text_of,
};
use futures_util::future::BoxFuture;
use serde_json::json;
use yapi_ai::faux::{Faux, Response};
use yapi_core::agent_session::{AgentSession, TreeNavigation};
use yapi_core::extensions::{
    ComponentHost, CustomOptions, DialogOptions, ExtensionUi, Mode, NoUi, NotifyKind,
    RemoteComponent,
};
use yapi_ext::ExtensionHost;
use yapi_types::message::Message;
use yapi_types::session::FileEntry;

fn example() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello.wasm")
}

#[tokio::test(flavor = "multi_thread")]
async fn native_extensions_register_and_run() {
    let dir = scratch("native");
    let host = ExtensionHost::load_native(&engine(), options(&dir), &cli_source(&example()))
        .await
        .unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let flags = host.flags();
    assert_eq!(flags.len(), 1);
    assert_eq!(flags[0].name, "shout-suffix");
    assert!(flags[0].takes_value);
    let mut values = serde_json::Map::new();
    values.insert("shout-suffix".into(), json!("!!"));
    host.set_flags(values).await.unwrap();

    let faux = Faux::new([
        Response::tool_call("call-1", "shout", json!({"text": "hello"})),
        Response::tool_call("call-2", "shout", json!({"text": ""})),
        Response::text("done"),
    ]);
    let session = session(&faux, &dir, host.for_session());
    assert!(session.active_tool_names().contains(&"shout".to_owned()));
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None, None)
        .await;
    assert_eq!(
        custom_entries(&session, "hello-started"),
        [json!({"mode": "print"})]
    );

    session.prompt("shout hello", Vec::new()).await.unwrap();
    let results: Vec<String> = session
        .messages()
        .iter()
        .filter(|message| matches!(message, Message::ToolResult(_)))
        .map(text_of)
        .collect();
    assert_eq!(results, ["HELLO!!", "Nothing to shout"]);
    assert_eq!(session.extensions()[0].commands()[0].name, "hello");
}

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}.wasm"))
}

async fn load(dir: &Path, name: &str) -> Arc<ExtensionHost> {
    let host = ExtensionHost::load_native(&engine(), options(dir), &cli_source(&fixture(name)))
        .await
        .unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    host
}

/// The text of every tool result, in order.
fn tool_results(session: &AgentSession) -> Vec<String> {
    session
        .messages()
        .iter()
        .filter(|message| matches!(message, Message::ToolResult(_)))
        .map(text_of)
        .collect()
}

/// A person who answers each selection with the next of `answers`.
struct Person(std::sync::Mutex<Vec<&'static str>>);

impl ExtensionUi for Person {
    fn has_ui(&self) -> bool {
        true
    }

    fn notify(&self, _message: &str, _kind: NotifyKind) {}

    fn select(
        &self,
        _title: &str,
        _options: Vec<String>,
        _dialog: DialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        let answer = self.0.lock().unwrap().remove(0).to_owned();
        Box::pin(async move { Some(answer) })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_gate_asks_before_dangerous_commands() {
    // The commands stay harmless if the gate lets them through.
    let dir = scratch("permission-gate");
    let host = load(&dir, "permission-gate").await;
    let faux = Faux::new([
        Response::tool_call(
            "call-1",
            "bash",
            json!({"command": "rm -rf ./nothing-here"}),
        ),
        Response::tool_call("call-2", "bash", json!({"command": "echo safe"})),
        Response::text("done"),
        Response::tool_call("call-3", "bash", json!({"command": "sudo -n true"})),
        Response::tool_call("call-4", "bash", json!({"command": "chmod 777 ./missing"})),
        Response::text("done"),
    ]);
    let session = session_with_tools(&faux, &dir, host.for_session(), &["bash"]);
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None, None)
        .await;
    session.prompt("run them", Vec::new()).await.unwrap();
    let results = tool_results(&session);
    assert_eq!(
        results[0],
        "Dangerous command blocked (no UI for confirmation)"
    );
    assert!(results[1].contains("safe"), "{results:?}");

    // With a person to ask, the answer decides.
    let person = Person(std::sync::Mutex::new(vec!["No", "Yes"]));
    session
        .bind_extensions(Arc::new(person), Mode::Tui, None, None)
        .await;
    session.prompt("again", Vec::new()).await.unwrap();
    let results = tool_results(&session);
    assert_eq!(results[2], "Blocked by user");
    assert_ne!(results[3], "Blocked by user", "the person allowed it");
}

#[tokio::test(flavor = "multi_thread")]
async fn protected_paths_block_writes() {
    let dir = scratch("protected-paths");
    let host = load(&dir, "protected-paths").await;
    let faux = Faux::new([
        Response::tool_call(
            "call-1",
            "write",
            json!({"path": ".env", "content": "KEY=1"}),
        ),
        Response::tool_call(
            "call-2",
            "write",
            json!({"path": "notes.txt", "content": "hi"}),
        ),
        Response::text("done"),
    ]);
    let session = session_with_tools(&faux, &dir, host.for_session(), &["write"]);
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None, None)
        .await;
    session.prompt("write", Vec::new()).await.unwrap();
    assert_eq!(tool_results(&session)[0], "Path \".env\" is protected");
    assert!(!dir.join(".env").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "hi"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn todo_keeps_a_list_per_branch() {
    let dir = scratch("todo");
    let host = load(&dir, "todo").await;
    let todo = |id: &str, params: serde_json::Value| Response::tool_call(id, "todo", params);
    let faux = Faux::new([
        todo("call-1", json!({"action": "add", "text": "buy milk"})),
        todo("call-2", json!({"action": "add", "text": "write docs"})),
        todo("call-3", json!({"action": "toggle", "id": 1})),
        todo("call-4", json!({"action": "toggle", "id": 9})),
        todo("call-5", json!({"action": "list"})),
        Response::text("done"),
        todo("call-6", json!({"action": "list"})),
        Response::text("done"),
    ]);
    let session = session(&faux, &dir, host.for_session());
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None, None)
        .await;
    session.prompt("plan", Vec::new()).await.unwrap();
    assert_eq!(
        tool_results(&session),
        [
            "Added todo #1: buy milk",
            "Added todo #2: write docs",
            "Todo #1 completed",
            "Todo #9 not found",
            "[x] #1: buy milk\n[ ] #2: write docs",
        ]
    );

    // Back on the branch where only the first item existed, the list is
    // rebuilt from that point.
    let first = session.with_session(|file| {
        file.entries()
            .find_map(|entry| match entry {
                FileEntry::Message(entry) => match &entry.message {
                    Message::ToolResult(result) if result.tool_call_id == "call-1" => {
                        Some(entry.meta.id.clone())
                    }
                    _ => None,
                },
                _ => None,
            })
            .unwrap()
    });
    session
        .navigate_tree(&first, TreeNavigation::default())
        .await
        .unwrap();
    session.prompt("list", Vec::new()).await.unwrap();
    assert_eq!(tool_results(&session).last().unwrap(), "[ ] #1: buy milk");
}

/// A terminal that shows the custom components extensions open.
#[derive(Default)]
struct Terminal {
    custom: std::sync::Mutex<Option<(RemoteComponent, CustomOptions)>>,
    closed: std::sync::Mutex<Vec<(u64, u32)>>,
    editor: std::sync::Mutex<Option<RemoteComponent>>,
    footer: std::sync::Mutex<Option<RemoteComponent>>,
    /// Notifications and what the extension's editor reported, in order.
    events: std::sync::Mutex<Vec<String>>,
    listeners: std::sync::Mutex<Option<Arc<dyn ComponentHost>>>,
    completions: std::sync::Mutex<Option<Arc<dyn ComponentHost>>>,
}

impl Terminal {
    fn event(&self, event: String) {
        self.events.lock().unwrap().push(event);
    }

    /// The custom component the extension shows, once it shows one.
    async fn shown(&self) -> (RemoteComponent, CustomOptions) {
        loop {
            if let Some(shown) = self.custom.lock().unwrap().clone() {
                return shown;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

impl ExtensionUi for Terminal {
    fn has_ui(&self) -> bool {
        true
    }

    fn notify(&self, message: &str, _kind: NotifyKind) {
        self.event(format!("notify {message}"));
    }

    fn editor_text(&self) -> String {
        "draft".into()
    }

    fn set_footer(&self, footer: Option<RemoteComponent>) {
        *self.footer.lock().unwrap() = footer;
    }

    fn set_editor(&self, editor: Option<RemoteComponent>, _embeds_status: bool) {
        *self.editor.lock().unwrap() = editor;
    }

    fn editor_changed(&self, text: &str) {
        self.event(format!("changed {text}"));
    }

    fn editor_submit(&self, text: &str) {
        self.event(format!("submit {text}"));
    }

    fn editor_action(&self, action: &str) {
        self.event(format!("action {action}"));
    }

    fn keybindings(&self) -> serde_json::Value {
        json!({"bindings": {"app.interrupt": ["escape"], "app.exit": ["ctrl+d"]}, "actions": []})
    }

    fn shows_components(&self) -> bool {
        true
    }

    fn theme(&self) -> serde_json::Value {
        json!({"fg": {"accent": "\x1b[36m", "borderMuted": "\x1b[37m", "dim": "\x1b[90m"}, "bg": {}, "dim": []})
    }

    fn custom(&self, component: RemoteComponent, options: CustomOptions) {
        *self.custom.lock().unwrap() = Some((component, options));
    }

    fn close(&self, component: RemoteComponent) {
        self.closed.lock().unwrap().push(component.key());
    }

    fn set_terminal_input(&self, _runtime: u64, listeners: Option<Arc<dyn ComponentHost>>) {
        self.event(format!("listening {}", listeners.is_some()));
        *self.listeners.lock().unwrap() = listeners;
    }

    fn set_autocomplete(&self, providers: Arc<dyn ComponentHost>, triggers: Vec<String>) {
        self.event(format!("autocomplete {}", triggers.join(" ")));
        *self.completions.lock().unwrap() = Some(providers);
    }

    /// The built-in provider completes slash commands.
    fn suggestions(&self, request: serde_json::Value) -> BoxFuture<'static, serde_json::Value> {
        let answer = (request["lines"][0] == "/he")
            .then(|| json!({"items": [{"value": "hello", "label": "hello"}], "prefix": "/he"}));
        Box::pin(async move { answer.unwrap_or_default() })
    }

    /// Puts the item's value on a line of its own, the cursor after it.
    fn apply_completion(&self, request: &serde_json::Value) -> serde_json::Value {
        let value = request["item"]["value"].as_str().unwrap();
        let line = format!("é {value}");
        json!({"lines": [line], "cursorLine": 0, "cursorCol": line.encode_utf16().count()})
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn todo_shows_the_list_in_a_component_until_escape() {
    let dir = scratch("todo-component");
    let host = load(&dir, "todo").await;
    let faux = Faux::new([
        Response::tool_call(
            "call-1",
            "todo",
            json!({"action": "add", "text": "buy milk"}),
        ),
        Response::text("done"),
    ]);
    let session = session(&faux, &dir, host.for_session());
    let terminal = Arc::new(Terminal::default());
    session
        .bind_extensions(terminal.clone(), Mode::Tui, None, None)
        .await;
    session.prompt("plan", Vec::new()).await.unwrap();

    let command = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("/todos", Vec::new()).await }
    });
    let (component, options) = terminal.shown().await;
    assert!(!options.overlay);
    // Rendered in the session's theme, for each width asked.
    let lines = component.render(20).await;
    assert_eq!(
        lines[1],
        "\x1b[37m───\x1b[39m\x1b[36m Todos \x1b[39m\x1b[37m──────────\x1b[39m"
    );
    assert_eq!(lines[3], "  0/1 completed");
    assert_eq!(lines[5], "  \x1b[90m○\x1b[39m \x1b[36m#1\x1b[39m buy milk");
    assert_eq!(component.render(12).await[1].matches('─').count(), 5);

    // Other keys leave it open. Escape, as the Kitty protocol sends it,
    // closes it and ends the command.
    component.input("x");
    assert_eq!(component.render(20).await, lines);
    assert!(terminal.closed.lock().unwrap().is_empty());
    component.input("\x1b[27u");
    command.await.unwrap().unwrap();
    assert_eq!(*terminal.closed.lock().unwrap(), [component.key()]);
    assert!(component.render(20).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn repo_status_reports_uncommitted_files() {
    let dir = scratch("repo-status");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(status.status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    let host = load(&dir, "repo-status").await;
    let faux = Faux::new([
        Response::tool_call("call-1", "repo_status", json!({})),
        Response::text("done"),
    ]);
    let session = session(&faux, &dir, host.for_session());
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None, None)
        .await;
    session.prompt("status", Vec::new()).await.unwrap();
    let report = &tool_results(&session)[0];
    assert!(report.starts_with("On main, "), "{report}");
    assert!(report.contains("a.txt"), "{report}");
}

/// A tool call whose run is aborted before it reaches the extension is
/// aborted as soon as it starts, instead of running to the end.
#[tokio::test(flavor = "multi_thread")]
async fn native_tools_aborted_before_they_start_stop() {
    let dir = scratch("subagent-aborted");
    let host = load(&dir, "subagent").await;
    let faux = Faux::new([]);
    let session = session(&faux, &dir, host.for_session());
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None, None)
        .await;
    let tool = session
        .callable_tools()
        .into_iter()
        .find(|tool| tool.name() == "subagent")
        .unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let result = tool
        .tool
        .execute(
            "call-1".into(),
            json!({"task": "x"}),
            cancel,
            Arc::new(|_| {}),
        )
        .await;
    assert_eq!(result.unwrap_err(), "This operation was aborted");
}

/// The rows of `component` at `width`, without CSI and APC sequences.
async fn plain(component: &RemoteComponent, width: u16) -> Vec<String> {
    let strip = |row: &str| {
        let mut out = String::new();
        let mut chars = row.chars();
        while let Some(char) = chars.next() {
            match (char, chars.clone().next()) {
                ('\x1b', Some('[')) => {
                    while !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {}
                }
                ('\x1b', Some('_')) => while chars.next().is_some_and(|c| c != '\x07') {},
                _ => out.push(char),
            }
        }
        out
    };
    component
        .render(width)
        .await
        .iter()
        .map(|row| strip(row))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn modal_editor_replaces_the_editor() {
    let dir = scratch("modal-editor");
    let host = load(&dir, "modal-editor").await;
    let session = session(&Faux::new([]), &dir, host.for_session());
    let terminal = Arc::new(Terminal::default());
    session
        .bind_extensions(terminal.clone(), Mode::Tui, None, None)
        .await;
    let editor = terminal.editor.lock().unwrap().clone().unwrap();

    // It takes the built-in editor's text, and shows its cursor and mode.
    let rows = editor.render(20).await;
    assert!(rows[1].contains("draft\x1b_pi:c\x07"), "{rows:?}");
    assert_eq!(plain(&editor, 20).await[2], "──────────── INSERT ");

    // Normal mode moves and deletes, then Escape interrupts as the built-in
    // editor's does, and Enter submits back in insert mode.
    for key in ["!", "\x1b", "0", "x", "\x1b", "i", "\r"] {
        editor.input(key);
    }
    editor.editor_op(&json!({"op": "setText", "text": "next"}));
    assert_eq!(plain(&editor, 20).await[1], "next                ");
    assert_eq!(
        *terminal.events.lock().unwrap(),
        [
            "changed draft",
            "changed draft!",
            "changed raft!",
            "action app.interrupt",
            "submit raft!",
            "changed ",
            "changed next",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn question_draws_its_call_and_result() {
    let dir = scratch("question-renderers");
    let host = load(&dir, "question").await;
    let session = session(&Faux::new([]), &dir, host.for_session());
    session
        .bind_extensions(Arc::new(Terminal::default()), Mode::Tui, None, None)
        .await;
    let extension = session.extensions()[0].clone();
    let renderers = &extension.renderers().tools["question"];
    assert!(renderers.call && renderers.result && !renderers.own_shell);

    let args = json!({"question": "Color?", "options": [{"label": "Red"}, {"label": "Blue"}]});
    let call = json!({"kind": "toolCall", "name": "question", "toolCallId": "c1", "args": args, "context": {}});
    let drawn = extension.component(&call).await.unwrap();
    assert_eq!(
        plain(&drawn, 40).await,
        [
            "question Color?                         ",
            "  Options: 1. Red, 2. Blue, 3. Type     ",
            "something.                              ",
        ]
    );
    // Drawing again replaces the component.
    let again = extension.component(&call).await.unwrap();
    assert_ne!(again.key(), drawn.key());
    assert!(drawn.render(40).await.is_empty());

    let details = json!({"question": "Color?", "options": ["Red", "Blue"], "answer": "Blue", "wasCustom": false});
    let result = json!({
        "kind": "toolResult", "name": "question", "toolCallId": "c1", "args": args,
        "result": {"content": [], "details": details}, "options": {}, "context": {},
    });
    let drawn = extension.component(&result).await.unwrap();
    assert_eq!(plain(&drawn, 12).await, ["✓ 2. Blue   "]);
}

#[tokio::test(flavor = "multi_thread")]
async fn message_renderer_draws_status_messages() {
    let dir = scratch("message-renderer");
    let host = load(&dir, "message-renderer").await;
    let session = session(&Faux::new([]), &dir, host.for_session());
    session
        .bind_extensions(Arc::new(Terminal::default()), Mode::Tui, None, None)
        .await;
    let extension = session.extensions()[0].clone();
    assert_eq!(extension.renderers().messages, ["status-update"]);
    let message = json!({"customType": "status-update", "content": "Disk full", "details": {"level": "warn"}});
    let request =
        json!({"kind": "message", "key": "1", "message": message, "options": {"outputPad": 2}});
    let drawn = extension.component(&request).await.unwrap();
    assert_eq!(
        plain(&drawn, 20).await,
        [
            "                    ",
            "  [WARN] Disk full  ",
            "                    "
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn select_menu_picks_from_a_select_list() {
    let dir = scratch("select-menu");
    let host = load(&dir, "select-menu").await;
    let session = session(&Faux::new([]), &dir, host.for_session());
    let terminal = Arc::new(Terminal::default());
    session
        .bind_extensions(terminal.clone(), Mode::Tui, None, None)
        .await;
    let command = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("/menu", Vec::new()).await }
    });
    let (menu, _) = terminal.shown().await;
    assert_eq!(
        plain(&menu, 20).await,
        [
            "────────────────────",
            " Drinks             ",
            "→ Tea",
            "  Juice",
            "  Water",
            "────────────────────"
        ]
    );
    menu.input("\x1b[B");
    assert_eq!(plain(&menu, 20).await[3], "→ Juice");
    menu.input("\r");
    command.await.unwrap().unwrap();
    assert_eq!(*terminal.events.lock().unwrap(), ["notify Chose juice"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn overlay_test_takes_its_width_and_typing() {
    let dir = scratch("overlay-test");
    let host = load(&dir, "overlay-test").await;
    let session = session(&Faux::new([]), &dir, host.for_session());
    let terminal = Arc::new(Terminal::default());
    session
        .bind_extensions(terminal.clone(), Mode::Tui, None, None)
        .await;
    let command = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("/overlay-test", Vec::new()).await }
    });
    let (overlay, options) = terminal.shown().await;
    // Without overlay options, the overlay is as wide as the component.
    assert!(options.overlay);
    assert_eq!(options.overlay_options, json!({"width": 70}));
    overlay.input("h");
    overlay.input("i");
    let rows = overlay.render(70).await;
    assert!(rows[9].contains("hi\x1b_pi:c\x07"), "{rows:?}");
    overlay.input("\r");
    command.await.unwrap().unwrap();
    assert_eq!(*terminal.events.lock().unwrap(), ["notify Search: \"hi\""]);
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_footer_toggles_a_footer_component() {
    let dir = scratch("custom-footer");
    let host = load(&dir, "custom-footer").await;
    let session = session(&Faux::new([]), &dir, host.for_session());
    let terminal = Arc::new(Terminal::default());
    session
        .bind_extensions(terminal.clone(), Mode::Tui, None, None)
        .await;
    session.prompt("/footer", Vec::new()).await.unwrap();
    let footer = terminal.footer.lock().unwrap().clone().unwrap();
    let model = session.model().unwrap().id;
    let row = format!("↑0 ↓0 $0.000{}{model}", " ".repeat(28 - model.len()));
    assert_eq!(plain(&footer, 40).await, [row]);
    session.prompt("/footer", Vec::new()).await.unwrap();
    assert!(terminal.footer.lock().unwrap().is_none());
    assert_eq!(
        *terminal.events.lock().unwrap(),
        [
            "notify Custom footer enabled",
            "notify Default footer restored"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn input_hooks_listen_complete_and_take_a_shortcut() {
    let dir = scratch("input-hooks");
    let host = load(&dir, "input-hooks").await;
    let session = session(&Faux::new([]), &dir, host.for_session());
    let terminal = Arc::new(Terminal::default());
    session
        .bind_extensions(terminal.clone(), Mode::Tui, None, None)
        .await;

    // The listener replaces `a`, lets `?` through past a full editor, and
    // consumes Ctrl+G, which it reports on with the editor's text.
    let listeners = terminal.listeners.lock().unwrap().clone().unwrap();
    let keys = ["b", "a", "?", "\x07"].map(str::to_owned).to_vec();
    assert_eq!(listeners.terminal_input(keys).await, ["b", "A", "?"]);

    // Variables complete after `$`, in byte columns past the `é`, and
    // apply as the built-in provider does.
    let providers = terminal.completions.lock().unwrap().clone().unwrap();
    let request = json!({"lines": ["é $P"], "cursorLine": 0, "cursorCol": 4, "force": false});
    let item = |value: &str| json!({"value": value, "label": value, "description": "environment variable"});
    let applied = |value: &str| json!({"lines": [format!("é {value}")], "cursorLine": 0, "cursorCol": 2 + value.len()});
    assert_eq!(
        providers.suggestions(request).await,
        json!({"prefix": "$P", "items": [item("$PATH"), item("$PWD")], "applied": [applied("$PATH"), applied("$PWD")]})
    );
    // Other text goes to the built-in provider.
    let request = json!({"lines": ["/he"], "cursorLine": 0, "cursorCol": 3, "force": true});
    assert_eq!(
        providers.suggestions(request).await,
        json!({"prefix": "/he", "items": [{"value": "hello", "label": "hello"}], "applied": [applied("hello")]})
    );

    let (shortcuts, _) = session.extension_shortcuts(&[]);
    assert_eq!(shortcuts[0].key, "alt+k");
    assert_eq!(shortcuts[0].description.as_deref(), Some("Count keys"));
    session.run_shortcut(&shortcuts[0]).await.unwrap();

    // Once `/quiet` drops the subscription, keys pass.
    session.prompt("/quiet", Vec::new()).await.unwrap();
    assert!(terminal.listeners.lock().unwrap().is_none());
    assert_eq!(listeners.terminal_input(vec!["a".into()]).await, ["a"]);
    assert_eq!(
        *terminal.events.lock().unwrap(),
        [
            "listening true",
            "autocomplete $",
            "notify Saw 4 keys, editor: draft",
            "notify Saw 4 keys",
            "listening false",
            "notify Stopped listening",
        ]
    );
}
