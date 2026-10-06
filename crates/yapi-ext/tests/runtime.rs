//! The runtime loads pi extensions written in TypeScript and runs their
//! tools, commands and handlers.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use yapi_ext::{Engine, Instance, NoBridge, Options};

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("runtime-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn engine() -> Engine {
    Engine::new(Some(
        &Path::new(env!("CARGO_TARGET_TMPDIR")).join("wasm-cache"),
    ))
    .unwrap()
}

async fn load(dir: &Path, files: &[(&str, &str)]) -> (Instance, Value) {
    for (name, text) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let instance = Instance::start(
        &engine(),
        Options::new(dir.to_path_buf()),
        Arc::new(NoBridge),
    )
    .await
    .unwrap();
    let path = dir.join(files[0].0);
    let loaded = instance
        .call(
            "load",
            &json!({"cwd": dir, "extensions": [{"id": 1, "path": path}]}),
        )
        .await
        .unwrap();
    (instance, loaded["extensions"][0].clone())
}

const HELLO: &str = r#"
import { Type } from "typebox";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { StringEnum } from "@earendil-works/pi-ai";
import { greeting } from "./greeting.js";

export default function (pi: ExtensionAPI) {
	pi.registerTool({
		name: "hello",
		label: "Hello",
		description: "Says hello",
		parameters: Type.Object({ name: Type.String(), tone: StringEnum(["warm", "dry"] as const) }),
		async execute(_id, params: { name: string; tone: string }) {
			return { content: [{ type: "text", text: greeting(params.name, params.tone) }], details: {} };
		},
	});
	pi.registerCommand("greet", { description: "Greets", handler: async () => {} });
	pi.registerFlag("loud", { type: "boolean", default: false, description: "Shout" });
	pi.on("session_start", async () => {});
}
"#;

const GREETING: &str = r#"
export function greeting(name: string, tone: string): string {
	return tone === "warm" ? `Hello, ${name}!` : `Hello ${name}.`;
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn registers_and_runs_a_typescript_extension() {
    let dir = scratch("hello");
    let (instance, extension) = load(&dir, &[("hello.ts", HELLO), ("greeting.ts", GREETING)]).await;
    assert_eq!(extension.get("error"), None, "{extension}");
    assert_eq!(extension["tools"][0]["name"], "hello");
    assert_eq!(
        extension["tools"][0]["parameters"]["properties"]["tone"],
        json!({"type": "string", "enum": ["warm", "dry"]})
    );
    assert_eq!(extension["commands"][0]["name"], "greet");
    assert_eq!(extension["flags"][0]["name"], "loud");
    assert_eq!(extension["events"], json!(["session_start"]));

    instance.call("bind", &Value::Null).await.unwrap();
    let result = instance
        .call(
            "tool",
            &json!({"extension": 1, "name": "hello", "toolCallId": "t1",
                    "params": {"name": "yapi", "tone": "warm"}, "ctx": {}}),
        )
        .await
        .unwrap();
    assert_eq!(result["content"][0]["text"], "Hello, yapi!");
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_load_errors_as_pi_does() {
    let dir = scratch("errors");
    let (_instance, extension) = load(&dir, &[("bad.ts", "export const notAFactory = 1;\n")]).await;
    let error = extension["error"].as_str().unwrap();
    assert!(
        error.starts_with("Extension does not export a valid factory function"),
        "{error}"
    );

    let dir = scratch("missing");
    let (_instance, extension) = load(
        &dir,
        &[(
            "missing.ts",
            "import x from \"not-installed\";\nexport default () => x;\n",
        )],
    )
    .await;
    let error = extension["error"].as_str().unwrap();
    assert!(
        error.starts_with("Failed to load extension: Cannot find module 'not-installed'"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_node_apis_and_common_js_packages() {
    let dir = scratch("node");
    let main = r#"
import fs from "node:fs";
import path from "node:path";
import { execSync } from "child_process";
import lib from "cjs-lib";
import { twice } from "cjs-lib";
export default function (pi) {
	pi.registerCommand("probe", {
		description: [
			fs.readFileSync(path.join(__dirname, "data.txt"), "utf8").trim(),
			String(lib.twice(2)),
			String(twice(3)),
			execSync("echo hi", { encoding: "utf8" }).trim(),
			typeof setTimeout,
		].join(","),
		handler: async () => {},
	});
}
"#;
    let (_instance, extension) = load(
        &dir,
        &[
            ("main.ts", main),
            ("data.txt", "from disk\n"),
            (
                "node_modules/cjs-lib/package.json",
                r#"{"name":"cjs-lib","main":"index.js"}"#,
            ),
            (
                "node_modules/cjs-lib/index.js",
                "exports.twice = (n) => n * 2;\n",
            ),
        ],
    )
    .await;
    assert_eq!(extension.get("error"), None, "{extension}");
    assert_eq!(
        extension["commands"][0]["description"],
        "from disk,4,6,hi,function"
    );
}

/// Gaps the npm package comparison found: `import.meta.resolve`, Node's
/// diagnostic report, a NUL character in a source file, and `stream` as the
/// constructor old packages extend.
#[tokio::test(flavor = "multi_thread")]
async fn resolves_import_meta_reads_reports_and_keeps_nul_characters() {
    let dir = scratch("meta");
    let main = "
import lib from \"cjs-lib\";
import util from \"node:util\";
import EventEmitter from \"node:events\";
import { createRequire } from \"node:module\";
const Stream = createRequire(import.meta.url)(\"stream\");
function Legacy() {
	Stream.call(this);
}
util.inherits(Legacy, Stream);
function Emitter() {
	EventEmitter.call(this);
}
util.inherits(Emitter, EventEmitter);
const nul = \"a\0b\";
export default function (pi) {
	pi.registerCommand(\"probe\", {
		description: [
			import.meta.resolve(\"cjs-lib\"),
			import.meta.resolve(\"./data.txt\"),
			import.meta.resolve(\"node:fs\"),
			typeof process.report.getReport().header,
			nul.length,
			nul.charCodeAt(1),
			lib.ok,
			new Legacy() instanceof Stream && typeof Stream.Readable,
			new Emitter().on(\"x\", () => {}).listenerCount(\"x\"),
		].join(\",\"),
		handler: async () => {},
	});
}
";
    let (_instance, extension) = load(
        &dir,
        &[
            ("main.ts", main),
            ("data.txt", "data\n"),
            (
                "node_modules/cjs-lib/package.json",
                r#"{"name":"cjs-lib","main":"index.js"}"#,
            ),
            ("node_modules/cjs-lib/index.js", "exports.ok = 'yes\0';\n"),
        ],
    )
    .await;
    assert_eq!(extension.get("error"), None, "{extension}");
    let dir = dir.display();
    assert_eq!(
        extension["commands"][0]["description"],
        format!(
            "file://{dir}/node_modules/cjs-lib/index.js,file://{dir}/data.txt,node:fs,object,3,0,yes\0,function,1"
        )
    );
}

/// pi-ai's subpath modules link, and their catalog reads yapi's.
#[tokio::test(flavor = "multi_thread")]
async fn links_pi_ai_subpaths() {
    let dir = scratch("ai-subpaths");
    let main = r#"
import { getBuiltinModels, getBuiltinProviders, ANTHROPIC_MODELS, ANTHROPIC_IMAGE_MODELS } from "@earendil-works/pi-ai/providers/all";
import { getProviderEnvValue } from "@earendil-works/pi-ai/utils/provider-env";
import { DEFAULT_THINKING_BUDGETS } from "@earendil-works/pi-ai/api/simple-options";
import { anthropicProvider } from "@mariozechner/pi-ai/providers/anthropic";
export default function (pi) {
	const ids = getBuiltinModels("anthropic").map((model) => model.id);
	let stub;
	try {
		anthropicProvider();
	} catch (error) {
		stub = error.message;
	}
	pi.registerCommand("probe", {
		description: [
			getBuiltinProviders().includes("anthropic"),
			ids.length > 0 && ids.every((id) => ANTHROPIC_MODELS[id].id === id),
			Object.keys(ANTHROPIC_MODELS).length === ids.length,
			Object.keys(ANTHROPIC_IMAGE_MODELS).length,
			getProviderEnvValue("YAPI_TEST_VALUE", { YAPI_TEST_VALUE: "set" }),
			DEFAULT_THINKING_BUDGETS.high,
			stub,
		].join(","),
		handler: async () => {},
	});
}
"#;
    let (_instance, extension) = load(&dir, &[("main.ts", main)]).await;
    assert_eq!(extension.get("error"), None, "{extension}");
    assert_eq!(
        extension["commands"][0]["description"],
        "true,true,true,0,set,16384,anthropicProvider from @earendil-works/pi-ai is not available in yapi extensions"
    );
}

/// File descriptors, file streams and `fs.promises.open`, which loggers use.
#[tokio::test(flavor = "multi_thread")]
async fn reads_and_writes_through_file_descriptors() {
    let dir = scratch("descriptors");
    let main = r#"
import fs from "node:fs";
import path from "node:path";
export default async function (pi) {
	const file = path.join(import.meta.dirname, "log.txt");
	const fd = fs.openSync(file, "w");
	fs.writeSync(fd, "hello ");
	fs.writeSync(fd, Buffer.from("world"));
	fs.closeSync(fd);
	const appended = fs.openSync(file, "a");
	fs.writeSync(appended, "!");
	fs.closeSync(appended);
	const read = fs.openSync(file, "r");
	const buffer = Buffer.alloc(5);
	fs.readSync(read, buffer, 0, 5, 6);
	const at = buffer.toString();
	fs.readSync(read, buffer, 0, 5, null);
	const current = buffer.toString();
	fs.closeSync(read);
	let closed;
	try {
		fs.closeSync(read);
	} catch (error) {
		closed = error.code;
	}
	let missing;
	try {
		fs.openSync(path.join(import.meta.dirname, "missing.txt"), "r");
	} catch (error) {
		missing = error.code;
	}
	const target = path.join(import.meta.dirname, "stream.txt");
	const stream = fs.createWriteStream(target);
	stream.write("a");
	stream.end("b");
	await new Promise((resolve) => stream.on("finish", resolve));
	const chunks = [];
	for await (const chunk of fs.createReadStream(target, "utf8")) chunks.push(chunk);
	const handle = await fs.promises.open(file, "r");
	const { bytesRead } = await handle.read(Buffer.alloc(3), 0, 3, 0);
	await handle.close();
	pi.registerCommand("probe", {
		description: [fs.readFileSync(file, "utf8"), at, current, closed, missing, chunks.join(""), bytesRead].join(","),
		handler: async () => {},
	});
}
"#;
    let (_instance, extension) = load(&dir, &[("main.ts", main)]).await;
    assert_eq!(extension.get("error"), None, "{extension}");
    assert_eq!(
        extension["commands"][0]["description"],
        "hello world!,world,hello,EBADF,ENOENT,ab,3"
    );
}

/// TypeScript enums, whose transform needs their members evaluated.
#[tokio::test(flavor = "multi_thread")]
async fn transpiles_typescript_enums() {
    let dir = scratch("enums");
    let main = r#"
enum Status { Idle, Busy = 5, Done }
enum Label { Ok = "ok", Fail = "fail" }
const enum Flag { A = 1 << 2, B = A | 1 }
export default function (pi) {
	pi.registerCommand("probe", {
		description: [Status.Idle, Status.Busy, Status.Done, Status[5], Label.Fail, Flag.B].join(","),
		handler: async () => {},
	});
}
"#;
    let (_instance, extension) = load(&dir, &[("main.ts", main)]).await;
    assert_eq!(extension.get("error"), None, "{extension}");
    assert_eq!(extension["commands"][0]["description"], "0,5,6,Busy,fail,5");
}

/// `realpath` inside an instance whose filesystem is limited to one folder,
/// as for a package with restricted grants.
#[tokio::test(flavor = "multi_thread")]
async fn resolves_real_paths_under_restricted_roots() {
    let dir = scratch("realpath-roots");
    std::fs::write(
        dir.join("main.ts"),
        r#"
import fs from "node:fs";
import path from "node:path";
export default function (pi) {
	const out = [import.meta.filename, path.join(import.meta.dirname, "link.ts"), process.cwd()].map((p) => fs.realpathSync(p));
	pi.registerCommand("probe", { description: out.join(","), handler: async () => {} });
}
"#,
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("main.ts", dir.join("link.ts")).unwrap();
    let mut options = Options::new(dir.clone());
    options.filesystem_roots = vec![dir.clone()];
    let instance = Instance::start(&engine(), options, Arc::new(NoBridge))
        .await
        .unwrap();
    let loaded = instance
        .call(
            "load",
            &json!({"cwd": dir, "extensions": [{"id": 1, "path": dir.join("main.ts")}]}),
        )
        .await
        .unwrap();
    let extension = &loaded["extensions"][0];
    assert_eq!(extension.get("error"), None, "{extension}");
    let main = dir.join("main.ts").display().to_string();
    assert_eq!(
        extension["commands"][0]["description"],
        format!("{main},{main},{}", dir.display())
    );
}

/// Node modules yapi has no sockets for still load, with the parts that need
/// none: address checks, agents, `node:sea` and the names of `node:sqlite`.
#[tokio::test(flavor = "multi_thread")]
async fn loads_socket_free_parts_of_network_modules() {
    let dir = scratch("network");
    let main = r#"
import net, { BlockList } from "node:net";
import http from "node:http";
import { isSea } from "node:sea";
import { DatabaseSync } from "node:sqlite";
export default function (pi) {
	const blocked = new BlockList();
	blocked.addAddress("1.2.3.4");
	blocked.addRange("10.0.0.1", "10.0.0.9");
	blocked.addSubnet("fd00::", 8, "ipv6");
	let sqlite;
	try {
		new DatabaseSync(":memory:");
	} catch (error) {
		sqlite = error.code;
	}
	pi.registerCommand("probe", {
		description: [
			net.isIP("10.1.2.3"),
			net.isIP("::ffff:1.2.3.4"),
			net.isIP("nope"),
			blocked.check("10.0.0.5"),
			blocked.check("10.0.0.10"),
			blocked.check("fd12::1", "ipv6"),
			blocked.rules.join(";"),
			typeof new http.Agent({ keepAlive: true }).destroy,
			isSea(),
			sqlite,
		].join(","),
		handler: async () => {},
	});
}
"#;
    let (_instance, extension) = load(&dir, &[("main.ts", main)]).await;
    assert_eq!(extension.get("error"), None, "{extension}");
    assert_eq!(
        extension["commands"][0]["description"],
        "4,6,0,true,false,true,Subnet: IPv6 fd00::/8;Range: IPv4 10.0.0.1-10.0.0.9;Address: IPv4 1.2.3.4,function,false,ERR_NOT_SUPPORTED"
    );
}

/// `dns.lookup` resolves through the host, as SSRF guards such as
/// pi-web-access's expect before they fetch, and needs the network grant.
/// Only `localhost` and address literals resolve here, so the test needs no
/// network.
#[tokio::test(flavor = "multi_thread")]
async fn looks_up_host_names_through_the_host() {
    let dir = scratch("dns");
    let main = r#"
import dns, { lookup } from "node:dns";
import { lookup as lookupAsync, getDefaultResultOrder } from "node:dns/promises";
export default async function (pi) {
	const all = await lookupAsync("localhost", { all: true, verbatim: true });
	const one = await new Promise((resolve, reject) =>
		lookup("127.0.0.1", (error, address, family) => (error ? reject(error) : resolve(`${address}/${family}`))),
	);
	const literal = await dns.promises.lookup("::1", { family: 4 });
	let invalid;
	try {
		lookup("localhost", { family: 5 }, () => {});
	} catch (error) {
		invalid = error.code;
	}
	let unsupported;
	try {
		dns.resolve4("localhost", () => {});
	} catch (error) {
		unsupported = error.code;
	}
	pi.registerCommand("probe", {
		description: [
			all.length > 0 && all.every(({ address, family }) => (address === "127.0.0.1" && family === 4) || (address === "::1" && family === 6)),
			one,
			`${literal.address}/${literal.family}`,
			invalid,
			unsupported,
			getDefaultResultOrder(),
		].join(","),
		handler: async () => {},
	});
}
"#;
    let (_instance, extension) = load(&dir, &[("main.ts", main)]).await;
    assert_eq!(extension.get("error"), None, "{extension}");
    assert_eq!(
        extension["commands"][0]["description"],
        "true,127.0.0.1/4,::1/6,ERR_INVALID_ARG_VALUE,ERR_NOT_SUPPORTED,verbatim"
    );

    let denied = r#"
import { lookup } from "node:dns/promises";
export default async function (pi) {
	const message = await lookup("localhost").then(() => "resolved", (error) => error.message);
	pi.registerCommand("probe", { description: message, handler: async () => {} });
}
"#;
    let dir = scratch("dns-denied");
    let path = dir.join("main.ts");
    std::fs::write(&path, denied).unwrap();
    let mut options = Options::new(dir.clone());
    options.grants.network = false;
    let instance = Instance::start(&engine(), options, Arc::new(NoBridge))
        .await
        .unwrap();
    let loaded = instance
        .call(
            "load",
            &json!({"cwd": dir, "extensions": [{"id": 1, "path": path}]}),
        )
        .await
        .unwrap();
    let description = loaded["extensions"][0]["commands"][0]["description"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(description.contains("Network access"), "{loaded}");
}

/// Scripts without module syntax load as CommonJS, imports may carry a
/// query, and TypeScript grammar checks do not stop a module, as with pi's
/// loader.
#[tokio::test(flavor = "multi_thread")]
async fn loads_scripts_queries_and_unchecked_typescript() {
    let dir = scratch("loading");
    let main = r#"
import { createRequire } from "node:module";
import { value } from "./dep.js?v=1";
import { join } from "./unchecked.ts";
createRequire(import.meta.url)("./polyfill.js");
export default function (pi) {
	pi.registerCommand("probe", { description: [globalThis.polyfilled, value, join("a", "b")].join(","), handler: async () => {} });
}
"#;
    let (_instance, extension) = load(
        &dir,
        &[
            ("main.ts", main),
            (
                "polyfill.js",
                "(function (global) {\n\tglobal.polyfilled = \"yes\";\n})(globalThis);\n",
            ),
            ("dep.js", "export const value = 2;\n"),
            (
                "unchecked.ts",
                "export function join(a?: string, b: string): string {\n\treturn `${a}+${b}`;\n}\n",
            ),
        ],
    )
    .await;
    assert_eq!(extension.get("error"), None, "{extension}");
    assert_eq!(extension["commands"][0]["description"], "yes,2,a+b");
}

/// pi-ai's API provider registry: an extension's own stream serves its calls
/// for that API, and built-in APIs run on yapi's providers.
#[tokio::test(flavor = "multi_thread")]
async fn registers_api_providers() {
    let dir = scratch("api-providers");
    let main = r#"
import { registerApiProvider, getApiProvider, getApiProviders, unregisterApiProviders, streamSimple, createAssistantMessageEventStream } from "@earendil-works/pi-ai";
export default async function (pi) {
	registerApiProvider({
		api: "echo",
		stream: () => { throw new Error("unused"); },
		streamSimple: (model, context) => {
			const stream = createAssistantMessageEventStream();
			const message = { role: "assistant", content: [{ type: "text", text: `echo ${context.messages.length}` }], api: model.api, provider: model.provider, model: model.id, stopReason: "stop", timestamp: 0 };
			queueMicrotask(() => stream.push({ type: "done", reason: "stop", message }));
			return stream;
		},
	}, "mine");
	const reply = await streamSimple({ api: "echo", provider: "local", id: "e" }, { messages: [1, 2] }).result();
	const builtin = typeof getApiProvider("anthropic-messages")?.streamSimple;
	const count = getApiProviders().length;
	unregisterApiProviders("mine");
	pi.registerCommand("probe", {
		description: [reply.content[0].text, builtin, count, getApiProvider("echo"), getApiProvider("nope")].join(","),
		handler: async () => {},
	});
}
"#;
    let (_instance, extension) = load(&dir, &[("main.ts", main)]).await;
    assert_eq!(extension.get("error"), None, "{extension}");
    assert_eq!(
        extension["commands"][0]["description"],
        "echo 2,function,1,,"
    );
}

/// An instance given its own environment sees those variables only.
#[tokio::test(flavor = "multi_thread")]
async fn sees_only_the_given_environment() {
    let dir = scratch("environment");
    std::fs::write(
        dir.join("main.ts"),
        r#"
export default function (pi) {
	pi.registerCommand("probe", { description: JSON.stringify(process.env), handler: async () => {} });
}
"#,
    )
    .unwrap();
    let mut options = Options::new(dir.clone());
    options.environment = Some([("ONLY".to_owned(), "this".to_owned())].into());
    let instance = Instance::start(&engine(), options, Arc::new(NoBridge))
        .await
        .unwrap();
    let loaded = instance
        .call(
            "load",
            &json!({"cwd": dir, "extensions": [{"id": 1, "path": dir.join("main.ts")}]}),
        )
        .await
        .unwrap();
    assert_eq!(
        loaded["extensions"][0]["commands"][0]["description"],
        r#"{"ONLY":"this"}"#
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pins_pi_tui_to_text_fallbacks() {
    let dir = scratch("capabilities");
    std::fs::write(
        dir.join("main.ts"),
        r#"
import { getCapabilities } from "@earendil-works/pi-tui";
import { getCapabilities as legacy } from "@mariozechner/pi-tui";

export default function (pi) {
	const { images, hyperlinks } = getCapabilities();
	const same = legacy() === getCapabilities();
	pi.registerCommand("probe", { description: JSON.stringify({ images, hyperlinks, same }), handler: async () => {} });
}
"#,
    )
    .unwrap();
    // A terminal pi-tui would otherwise detect as supporting both.
    let mut options = Options::new(dir.clone());
    options.environment = Some(
        [
            ("TERM_PROGRAM".to_owned(), "kitty".to_owned()),
            ("KITTY_WINDOW_ID".to_owned(), "1".to_owned()),
        ]
        .into(),
    );
    let instance = Instance::start(&engine(), options, Arc::new(NoBridge))
        .await
        .unwrap();
    let loaded = instance
        .call(
            "load",
            &json!({"cwd": dir, "extensions": [{"id": 1, "path": dir.join("main.ts")}]}),
        )
        .await
        .unwrap();
    assert_eq!(
        loaded["extensions"][0]["commands"][0]["description"],
        r#"{"images":null,"hyperlinks":false,"same":true}"#,
        "{loaded}"
    );
}

/// A bridge that sets its flag when the instance's thread drops it.
struct DropFlag(Arc<std::sync::atomic::AtomicBool>);

impl yapi_ext::Bridge for DropFlag {}

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn join_stopped_waits_for_dropped_instances() {
    let dir = scratch("join-stopped");
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let instance = Instance::start(
        &engine(),
        Options::new(dir),
        Arc::new(DropFlag(dropped.clone())),
    )
    .await
    .unwrap();
    drop(instance);
    yapi_ext::join_stopped();
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
}
