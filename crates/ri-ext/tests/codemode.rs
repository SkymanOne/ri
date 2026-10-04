//! Codemode scripts in a session driven by the faux provider: nested tool
//! calls, output, errors, the store, limits, and the sandbox's boundaries.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{cli_source, custom_entries, engine, options, scratch, session};
use ri_ai::faux::{Faux, Response};
use ri_core::agent_session::AgentSession;
use ri_core::extensions::{Extension, Mode, NoUi};
use ri_ext::ExtensionHost;
use ri_ext::codemode::CodemodeExtension;
use ri_types::message::{ContentBlock, Message, ToolResultMessage};
use serde_json::{Value, json};

fn codemode() -> Arc<dyn Extension> {
    Arc::new(CodemodeExtension::new(
        Some(Path::new(env!("CARGO_TARGET_TMPDIR")).join("wasm-cache")),
        None,
    ))
}

/// Runs each script as one codemode call of the model, then ends the run.
async fn run(dir: &Path, scripts: &[&str]) -> (AgentSession, Faux, Vec<ToolResultMessage>) {
    let mut responses: Vec<Response> = scripts
        .iter()
        .enumerate()
        .map(|(index, code)| {
            Response::tool_call(
                &format!("toolu_{}", index + 1),
                "codemode",
                json!({"code": code}),
            )
        })
        .collect();
    responses.push(Response::text("done"));
    let faux = Faux::new(responses);
    let session = session(&faux, dir, vec![codemode()]);
    session.set_active_tools(vec!["read".into(), "bash".into(), "codemode".into()]);
    session.bind_extensions(Arc::new(NoUi), Mode::Print).await;
    session.prompt("go", Vec::new()).await.unwrap();
    let results = session
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    (session, faux, results)
}

/// The text blocks after the result header, which is checked and dropped.
fn output(result: &ToolResultMessage) -> Vec<String> {
    let mut texts = result.content.iter().filter_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    });
    let header = texts.next().unwrap();
    let status = if result.is_error {
        "failed"
    } else {
        "completed"
    };
    assert!(
        header.starts_with(&format!("Script {status}\nWall time "))
            && header.ends_with(" seconds\nOutput:\n"),
        "{header:?}"
    );
    texts.collect()
}

fn text(result: &ToolResultMessage) -> String {
    ri_types::message::blocks_text(&result.content, "")
}

#[tokio::test(flavor = "multi_thread")]
async fn scripts_call_tools_and_print_output() {
    let dir = scratch("codemode-calls");
    std::fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
    let (session, faux, results) = run(
        &dir,
        &["const t = await tools.read({path:'a.txt'}); text(t); console.log('n', 1, {a: 1}, [1,2], null, undefined); return {ok:true}",
          "const r = await tools.bash({command:'echo hi; exit 3'}); return r",
          "const settled = await Promise.allSettled([tools.read({path:'a.txt'}), tools.read({path:'missing.txt'})]); return settled.map((s) => s.status)"],
    )
    .await;

    assert_eq!(
        output(&results[0]),
        [
            "alpha\nbeta\n",
            "n 1 {\"a\":1} [1,2] null undefined",
            "{\"ok\":true}"
        ]
    );
    let calls = &results[0].details.as_ref().unwrap()["calls"];
    assert_eq!(calls[0]["id"], "toolu_1/1");
    assert_eq!(calls[0]["name"], "read");
    assert_eq!(calls[0]["args"], "{\"path\":\"a.txt\"}");
    assert_eq!(calls[0]["status"], "ok");
    assert!(calls[0]["durationMs"].is_number());
    let nested = results[0].nested_calls.as_ref().unwrap();
    assert!(nested.complete);
    assert_eq!(nested.calls[0].id, "toolu_1/1");
    assert_eq!(nested.calls[0].arguments.as_ref().unwrap()["path"], "a.txt");

    // bash resolves to its structured content, also for a non-zero exit.
    assert_eq!(output(&results[1]).len(), 1);
    let value: Value = serde_json::from_str(&output(&results[1])[0]).unwrap();
    assert_eq!(value["output"], "hi\n");
    assert_eq!(value["exit_code"], 3);
    let calls = &results[1].details.as_ref().unwrap()["calls"];
    assert_eq!(calls[0]["status"], "error");
    assert_eq!(calls[0]["error"], "hi\n\n\nCommand exited with code 3");

    assert_eq!(output(&results[2]), ["[\"fulfilled\",\"rejected\"]"]);

    // While codemode is active, declared tools say how scripts call them.
    let requests = faux.requests();
    let Some(Message::System(system)) = requests[0].first() else {
        panic!("no system message");
    };
    let tools = system.tools_added.as_ref().unwrap();
    let read = tools.iter().find(|tool| tool.name == "read").unwrap();
    assert!(
        read.description
            .ends_with("\n\nCodemode: `tools.read(args)` resolves to a string."),
        "{}",
        read.description
    );
    let bash = tools.iter().find(|tool| tool.name == "bash").unwrap();
    assert!(bash.description.ends_with(
        "resolves to `{ output, truncated, full_output_path?, exit_code, wall_time_seconds }`."
    ));
    let codemode = tools.iter().find(|tool| tool.name == "codemode").unwrap();
    assert!(
        codemode
            .description
            .starts_with("Run JavaScript that calls other tools.")
    );
    assert!(!codemode.description.contains("Nested tools:"));
    assert!(
        system
            .text()
            .contains("- codemode: Run JavaScript that calls other tools")
    );
    drop(session);
}

#[tokio::test(flavor = "multi_thread")]
async fn script_errors_read_as_in_pi() {
    let dir = scratch("codemode-errors");
    let (_session, _faux, results) = run(
        &dir,
        &[
            "await tools.nope({})",
            "text('before');\n\nthrow new Error('boom')",
            "await new Promise(() => {})",
            "// @options: {\"foo\": 1}\nreturn 1;",
            "const x = ;",
            "throw 'a string'",
            "return 'plain string'",
            "return undefined",
            "text('a'); exit(); text('b')",
            "await tools.read({path:'missing.txt'})",
        ],
    )
    .await;
    assert_eq!(
        output(&results[0]),
        [
            "Script error:\nTypeError: tools.nope does not exist. Available: read, bash. ALL_TOOLS lists every tool; searchTools(query) finds tools by topic. Check for a member with \"nope\" in tools.\n    at <anonymous> (codemode.js:1:35)\n\nNo tool calls were made."
        ]
    );
    assert_eq!(
        output(&results[1]),
        [
            "before",
            "Script error:\nError: boom\n    at <anonymous> (codemode.js:3:11)\n\nNo tool calls were made."
        ]
    );
    assert_eq!(
        output(&results[2]),
        [
            "Script error:\nError: The script is waiting on a promise that can never settle: no tool call is pending, and timers do not exist here.\n\nNo tool calls were made."
        ]
    );
    // Invalid options fail the call before anything runs, without the header.
    assert!(results[3].is_error);
    assert_eq!(
        text(&results[3]),
        "@options only supports `max_output_tokens` and `timeout_ms`; got `foo`"
    );
    assert_eq!(results[3].details, Some(json!({})));
    assert_eq!(
        output(&results[4]),
        [
            "Script error:\nSyntaxError: unexpected token in expression: ';'\n    at codemode.js:1:39\n\nNo tool calls were made."
        ]
    );
    assert_eq!(
        output(&results[5]),
        ["Script error:\nError: a string\n\nNo tool calls were made."]
    );
    assert_eq!(output(&results[6]), ["plain string"]);
    assert!(output(&results[7]).is_empty());
    assert_eq!(output(&results[8]), ["a"]);
    let missing = output(&results[9]);
    assert!(
        missing[0].starts_with("Script error:\nError: ENOENT: no such file or directory")
            && missing[0].ends_with(
                "\n\nTool calls made before the failure (they are not undone): read (error)"
            ),
        "{missing:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn store_values_last_across_calls() {
    let dir = scratch("codemode-store");
    let (session, _faux, results) = run(
        &dir,
        &[
            "store('k', {n: 1}); store('gone', 1); store('gone', undefined); return load('k')",
            "return [load('k'), load('gone')]",
            "store('k', 2); throw new Error('no')",
            "return load('k')",
        ],
    )
    .await;
    assert_eq!(output(&results[0]), ["{\"n\":1}"]);
    assert_eq!(output(&results[1]), ["[{\"n\":1},null]"]);
    // Writes of a failed script are dropped.
    assert_eq!(output(&results[3]), ["{\"n\":1}"]);
    assert_eq!(
        custom_entries(&session, "codemode-store"),
        [json!({"set": {"k": {"n": 1}}, "delete": ["gone"]})]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn limits_stop_scripts() {
    let dir = scratch("codemode-limits");
    let started = Instant::now();
    let (_session, _faux, results) = run(
        &dir,
        &[
            // Loads the engine, which pi's timeouts also count, so the
            // timeouts below measure the scripts on a busy machine too.
            "return 1",
            "// @options: {\"timeout_ms\": 200}\nwhile (true) {}",
            "// @options: {\"timeout_ms\": 3000}\nawait tools.bash({command: 'sleep 30'})",
            "function f() { return f() } f()",
            "// @options: {\"max_output_tokens\": 10}\ntext('x'.repeat(100)); text('tail-end')",
            "const kept = []; while (true) kept.push('x'.repeat(1 << 20) + kept.length)",
        ],
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(
        output(&results[1]),
        [
            "Script error:\nScript timed out: Execution timed out after 200 ms\n\nNo tool calls were made."
        ]
    );
    assert_eq!(
        output(&results[2]),
        [
            "Script error:\nScript timed out: Execution timed out after 3000 ms\n\nTool calls made before the failure (they are not undone): bash (cancelled)"
        ]
    );
    // QuickJS has no stack limit on WASI: runaway recursion stops the
    // sandbox instead of throwing a RangeError.
    assert!(
        output(&results[3])[0].starts_with("Script error:\nScript sandbox failed: wasm trap: "),
        "{:?}",
        output(&results[3])
    );
    let truncated = output(&results[4]);
    assert!(truncated[0].starts_with("Warning: truncated output (original token count: 28)"));
    let path = results[4].details.as_ref().unwrap()["fullOutputPath"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(path.contains("ri-codemode-"));
    let _ = std::fs::remove_file(path);
    assert!(
        output(&results[5])[0].starts_with("Script error:\nInternalError: out of memory"),
        "{:?}",
        output(&results[5])
    );
}

/// Scripts reach nothing but their tools: no files, processes, network,
/// environment, modules or host natives.
#[tokio::test(flavor = "multi_thread")]
async fn scripts_have_no_ambient_access() {
    let dir = scratch("codemode-sandbox");
    std::fs::write(dir.join("secret.txt"), "secret").unwrap();
    let secret = dir.join("secret.txt").display().to_string();
    let import =
        format!("return await import({secret:?}).then(() => 'loaded', (error) => String(error))");
    let (_session, _faux, results) = run(
        &dir,
        &[
            "return ['require', 'process', 'fetch', 'setTimeout', 'Buffer', 'XMLHttpRequest', 'WebSocket', '__ri', '__ri_native', '__ri_cjs', 'std', 'os', 'WebAssembly', 'Deno'].filter((name) => typeof globalThis[name] !== 'undefined')",
            &import,
            "return Object.getOwnPropertyNames(globalThis).join(',')",
            "try { return new Function('return this')().process } catch (error) { return String(error) }",
        ],
    )
    .await;
    assert_eq!(output(&results[0]), ["[]"]);
    assert!(
        !output(&results[1])[0].contains("loaded"),
        "{:?}",
        output(&results[1])
    );
    assert_eq!(
        output(&results[2]),
        [
            "Object,Function,Error,EvalError,RangeError,ReferenceError,SyntaxError,TypeError,URIError,InternalError,AggregateError,SuppressedError,Iterator,Array,parseInt,parseFloat,isNaN,isFinite,queueMicrotask,decodeURI,decodeURIComponent,encodeURI,encodeURIComponent,escape,unescape,Infinity,NaN,undefined,eval,Number,Boolean,String,Math,Reflect,Symbol,DisposableStack,globalThis,BigInt,Date,RegExp,JSON,Proxy,Map,Set,WeakMap,WeakSet,ArrayBuffer,SharedArrayBuffer,Uint8ClampedArray,Int8Array,Uint8Array,Int16Array,Uint16Array,Int32Array,Uint32Array,BigInt64Array,BigUint64Array,Float16Array,Float32Array,Float64Array,DataView,Promise,AsyncDisposableStack,WeakRef,FinalizationRegistry,DOMException,btoa,atob,performance,searchTools,describeTool,describeNamespace,store,load,tools,ALL_TOOLS,console,text,image,exit"
        ]
    );
    assert!(output(&results[3]).is_empty(), "{:?}", output(&results[3]));
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_globals_find_tools() {
    let dir = scratch("codemode-discovery");
    let (_session, _faux, results) = run(
        &dir,
        &[
            "return (await searchTools('run a shell command', { limit: 1 })).map((tool) => tool.name)",
            "return [await describeTool('nope'), await describeNamespace('nope'), (await describeTool('read')).split('\\n')[0]]",
            "return ALL_TOOLS.map((tool) => tool.name)",
            "await searchTools(1)",
        ],
    )
    .await;
    assert_eq!(output(&results[0]), ["[\"bash\"]"]);
    assert_eq!(
        output(&results[1]),
        [
            "[null,null,\"Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.\"]"
        ]
    );
    assert_eq!(output(&results[2]), ["[\"read\",\"bash\"]"]);
    assert_eq!(
        output(&results[3]),
        ["Script error:\nError: searchTools() expects a query string\n\nNo tool calls were made."]
    );
}

/// Packages that decorate codemode register the facade's definition; scripts
/// still run in ri's sandbox, and the loadout still applies.
#[tokio::test(flavor = "multi_thread")]
async fn facade_codemode_replaces_the_builtin() {
    let dir = scratch("codemode-facade");
    std::fs::write(dir.join("a.txt"), "alpha").unwrap();
    let path = dir.join("decorate.ts");
    std::fs::write(
        &path,
        r#"
import { createCodemodeExtension } from "@earendil-works/pi-coding-agent";
export default function (pi) {
	const proxy = new Proxy(pi, {
		get(target, property, receiver) {
			if (property === "registerTool") return (tool) => pi.registerTool({ ...tool, label: "Scripts" });
			return Reflect.get(target, property, receiver);
		},
	});
	createCodemodeExtension()(proxy);
}
"#,
    )
    .unwrap();
    let js = ExtensionHost::load(&engine(), options(&dir), &[cli_source(&path)])
        .await
        .unwrap();
    assert!(js.errors().is_empty(), "{:?}", js.errors());
    let faux = Faux::new([
        Response::tool_call(
            "toolu_1",
            "codemode",
            json!({"code": "return (await tools.read({path: 'a.txt'})) + '!'"}),
        ),
        Response::text("done"),
    ]);
    let mut extensions = js.for_session();
    extensions.push(codemode());
    let session = session(&faux, &dir, extensions);
    session.set_active_tools(vec!["read".into(), "codemode".into()]);
    session.bind_extensions(Arc::new(NoUi), Mode::Print).await;
    let codemode = session.tools().with(|registry| {
        registry
            .get("codemode")
            .map(|tool| tool.tool.label().to_owned())
    });
    assert_eq!(codemode.as_deref(), Some("Scripts"));
    session.prompt("go", Vec::new()).await.unwrap();
    let result = session
        .messages()
        .into_iter()
        .find_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .unwrap();
    assert_eq!(output(&result), ["alpha!"]);
    assert_eq!(result.nested_calls.unwrap().calls[0].id, "toolu_1/1");
    let requests = faux.requests();
    let Some(Message::System(system)) = requests[0].first() else {
        panic!("no system message");
    };
    let read = system
        .tools_added
        .as_ref()
        .unwrap()
        .iter()
        .find(|tool| tool.name == "read")
        .unwrap();
    assert!(read.description.ends_with("resolves to a string."));
}
