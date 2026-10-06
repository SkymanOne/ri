//! Modules embedded in the runtime: the runtime script, Node's built-in
//! modules, pi's API facade and the vendored pi packages.
//!
//! Embedded modules have `node:` and `yapi:` names, so they never collide with
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
    ("@earendil-works/pi-coding-agent", "yapi:pi/coding-agent"),
    ("@earendil-works/pi-agent-core", "yapi:pi/agent-core"),
    ("@earendil-works/pi-tui", "yapi:pi/tui"),
    ("@earendil-works/pi-ai", "yapi:pi/ai"),
    ("@mariozechner/pi-coding-agent", "yapi:pi/coding-agent"),
    ("@mariozechner/pi-agent-core", "yapi:pi/agent-core"),
    ("@mariozechner/pi-tui", "yapi:pi/tui"),
    ("@mariozechner/pi-ai", "yapi:pi/ai"),
    ("typebox", "yapi:vendor/typebox"),
    ("typebox/value", "yapi:vendor/typebox-value"),
    ("typebox/compile", "yapi:vendor/typebox-compile"),
    ("@sinclair/typebox", "yapi:vendor/typebox"),
    ("@sinclair/typebox/value", "yapi:vendor/typebox-value"),
    ("@sinclair/typebox/compile", "yapi:vendor/typebox-compile"),
];

/// The embedded module `name` imported from `base` resolves to, if any.
pub fn resolve(ctx: &Ctx<'_>, base: &str, name: &str) -> rquickjs::Result<Option<String>> {
    // Vendored bundles import their shared chunks by relative path.
    if let (Some(_), Some(chunk)) = (base.strip_prefix("yapi:vendor/"), name.strip_prefix("./")) {
        return Ok(Some(format!(
            "yapi:vendor/{}",
            chunk.trim_end_matches(".mjs")
        )));
    }
    // Embedded modules import each other by name.
    if name.starts_with("node:") || name.starts_with("yapi:") {
        return Ok(Some(name.to_owned()));
    }
    if let Some((_, module)) = ALIASES.iter().find(|(alias, _)| *alias == name) {
        return Ok(Some((*module).to_owned()));
    }
    // A Node built-in by its bare name, as `require` resolves it.
    let builtin: Option<String> = runtime_function(ctx, "builtinName")?.call((name,))?;
    if let Some(builtin) = builtin {
        return Ok(Some(format!("node:{builtin}")));
    }
    // Every pi-ai entry point maps to the facade, a superset of the root.
    Ok(["@earendil-works/pi-ai/", "@mariozechner/pi-ai/"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        .then(|| "yapi:pi/ai".to_owned()))
}

/// The source of embedded module `name`. A Node module is an ES module view
/// of its object in `__yapi_builtins`, which CommonJS `require` returns as is.
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
    let builtins: Object<'_> = ctx.globals().get("__yapi_builtins")?;
    if !builtins.contains_key(name)? {
        return Err(rquickjs::Exception::throw_message(
            ctx,
            &format!("Cannot find module 'node:{name}'"),
        ));
    }
    let names: Vec<String> = runtime_function(ctx, "builtinExports")?.call((name,))?;
    Ok(format!(
        "const m = globalThis.__yapi_builtins[{}];\nexport default m;\n{}",
        serde_json::Value::from(name),
        crate::reexports(&names)
    ))
}

/// The runtime script's function `__yapi.<name>`.
pub fn runtime_function<'js>(ctx: &Ctx<'js>, name: &str) -> rquickjs::Result<Function<'js>> {
    let yapi: Object<'js> = ctx.globals().get("__yapi")?;
    yapi.get(name)
}
