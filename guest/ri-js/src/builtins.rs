//! Modules embedded in the runtime: the runtime script, Node's built-in
//! modules, pi's API facade and the vendored pi packages.
//!
//! Embedded modules have `node:` and `ri:` names, so they never collide with
//! the file paths the host resolves.

use rquickjs::{Ctx, Function, Object};

/// The script that sets up globals, the Node shims and the dispatcher.
pub const RUNTIME: &str = concat!(
    include_str!("../js/runtime/00-core.js"),
    "\n",
    include_str!("../js/runtime/10-node.js"),
    "\n",
    include_str!("../js/runtime/15-node-exports.js"),
    "\n",
    include_str!("../js/runtime/20-cjs.js"),
    "\n",
    include_str!("../js/runtime/30-host.js"),
);

include!(concat!(env!("OUT_DIR"), "/modules.rs"));

/// pi's loader aliases (`getAliases` in `core/extensions/loader.ts`).
const ALIASES: &[(&str, &str)] = &[
    ("@earendil-works/pi-coding-agent", "ri:pi/coding-agent"),
    ("@earendil-works/pi-agent-core", "ri:pi/agent-core"),
    ("@earendil-works/pi-tui", "ri:vendor/pi-tui"),
    ("@earendil-works/pi-ai", "ri:pi/ai"),
    ("@mariozechner/pi-coding-agent", "ri:pi/coding-agent"),
    ("@mariozechner/pi-agent-core", "ri:pi/agent-core"),
    ("@mariozechner/pi-tui", "ri:vendor/pi-tui"),
    ("@mariozechner/pi-ai", "ri:pi/ai"),
    ("typebox", "ri:vendor/typebox"),
    ("typebox/value", "ri:vendor/typebox-value"),
    ("typebox/compile", "ri:vendor/typebox-compile"),
    ("@sinclair/typebox", "ri:vendor/typebox"),
    ("@sinclair/typebox/value", "ri:vendor/typebox-value"),
    ("@sinclair/typebox/compile", "ri:vendor/typebox-compile"),
];

/// Node's built-in modules the runtime provides, by their names without the
/// `node:` prefix. A name in `__ri_builtins` that is missing here can still be
/// imported with the prefix.
const NODE: &[&str] = &[
    "assert",
    "assert/strict",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "events",
    "fs",
    "fs/promises",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "path/posix",
    "perf_hooks",
    "process",
    "readline",
    "readline/promises",
    "repl",
    "stream",
    "stream/promises",
    "string_decoder",
    "timers",
    "timers/promises",
    "tls",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "worker_threads",
    "zlib",
];

/// The embedded module `name` imported from `base` resolves to, if any.
pub fn resolve(base: &str, name: &str) -> Option<String> {
    // Vendored bundles import their shared chunks by relative path.
    if let (Some(_), Some(chunk)) = (base.strip_prefix("ri:vendor/"), name.strip_prefix("./")) {
        return Some(format!("ri:vendor/{}", chunk.trim_end_matches(".mjs")));
    }
    if let Some(builtin) = name.strip_prefix("node:") {
        return Some(format!("node:{builtin}"));
    }
    if NODE.contains(&name) {
        return Some(format!("node:{name}"));
    }
    if let Some((_, module)) = ALIASES.iter().find(|(alias, _)| *alias == name) {
        return Some((*module).to_owned());
    }
    // Every pi-ai entry point maps to the facade, a superset of the root.
    ["@earendil-works/pi-ai/", "@mariozechner/pi-ai/"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        .then(|| "ri:pi/ai".to_owned())
}

/// The source of embedded module `name`. A Node module is an ES module view
/// of its object in `__ri_builtins`, which CommonJS `require` returns as is.
pub fn source(ctx: &Ctx<'_>, name: &str) -> Option<rquickjs::Result<String>> {
    if let Some(builtin) = name.strip_prefix("node:") {
        return Some(node_module(ctx, builtin));
    }
    MODULES
        .iter()
        .find(|(module, _)| *module == name)
        .map(|(_, source)| Ok((*source).to_owned()))
}

fn node_module(ctx: &Ctx<'_>, name: &str) -> rquickjs::Result<String> {
    let builtins: Object<'_> = ctx.globals().get("__ri_builtins")?;
    if !builtins.contains_key(name)? {
        return Err(rquickjs::Exception::throw_message(
            ctx,
            &format!("Cannot find module 'node:{name}'"),
        ));
    }
    let ri: Object<'_> = ctx.globals().get("__ri")?;
    let exports: Function<'_> = ri.get("builtinExports")?;
    let names: Vec<String> = exports.call((name,))?;
    let mut source = format!(
        "const m = globalThis.__ri_builtins[{}];\nexport default m;\n",
        crate::json_string(name)
    );
    for (index, export) in names.iter().enumerate() {
        source.push_str(&format!("const e{index} = m.{export};\n"));
    }
    let list: Vec<String> = names
        .iter()
        .enumerate()
        .map(|(index, export)| format!("e{index} as {export}"))
        .collect();
    source.push_str(&format!("export {{ {} }};\n", list.join(", ")));
    Ok(source)
}
