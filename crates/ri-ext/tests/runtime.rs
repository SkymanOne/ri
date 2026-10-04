//! The runtime loads pi extensions written in TypeScript and runs their
//! tools, commands and handlers.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ri_ext::{Engine, Instance, NoBridge, Options};
use serde_json::{Value, json};

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
                    "params": {"name": "ri", "tone": "warm"}, "ctx": {}}),
        )
        .await
        .unwrap();
    assert_eq!(result["content"][0]["text"], "Hello, ri!");
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
import { createRequire } from \"node:module\";
const Stream = createRequire(import.meta.url)(\"stream\");
function Legacy() {}
util.inherits(Legacy, Stream);
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
            "file://{dir}/node_modules/cjs-lib/index.js,file://{dir}/data.txt,node:fs,object,3,0,yes\0,function"
        )
    );
}

/// pi-ai's subpath modules link, and their catalog reads ri's.
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
			getProviderEnvValue("RI_TEST_VALUE", { RI_TEST_VALUE: "set" }),
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
        "true,true,true,0,set,16384,anthropicProvider from @earendil-works/pi-ai is not available in ri extensions"
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
