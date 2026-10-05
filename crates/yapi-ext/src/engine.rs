//! The wasmtime engine and the compiled `yapi-js` component.

use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, EngineWeak};

use crate::Error;
use crate::instance::State;

/// The JS runtime component, built by `cargo xtask js-runtime` and deflated
/// by the build script.
const YAPI_JS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/yapi-js.wasm.deflate"));

/// The native stack guest code may use. QuickJS bounds its own stack well
/// below this, so deep recursion throws a catchable `RangeError` in JS
/// instead of trapping.
pub(crate) const WASM_STACK: usize = 8 * 1024 * 1024;

/// How often the epoch advances; guest time limits are checked at this rate.
pub(crate) const TICK: Duration = Duration::from_millis(10);

wasmtime::component::bindgen!({ path: "../../wit/since_v0.1.0", world: "extension" });

/// A wasmtime engine with the `yapi-js` component compiled and linked. Cloning
/// shares it; instances start from it in milliseconds.
#[derive(Clone)]
pub struct Engine {
    pub(crate) engine: wasmtime::Engine,
    pub(crate) component: Component,
    pub(crate) linker: Arc<Linker<State>>,
    cache_dir: Option<PathBuf>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").finish_non_exhaustive()
    }
}

impl Engine {
    /// Compiles the runtime component, or loads it from `cache_dir` when an
    /// earlier compile by the same engine configuration is there. Native
    /// extensions compile into the same cache.
    pub fn new(cache_dir: Option<&Path>) -> Result<Engine, Error> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.epoch_interruption(true);
        config.max_wasm_stack(WASM_STACK);
        config.async_stack_size(WASM_STACK + (1 << 20));
        let engine = wasmtime::Engine::new(&config).map_err(Error::compile)?;
        let component = compile(&engine, YAPI_JS, &inflate_runtime, cache_dir, "yapi-js")?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(Error::compile)?;
        Extension::add_to_linker::<State, HasSelf<State>>(&mut linker, |state| state)
            .map_err(Error::compile)?;
        start_ticker(engine.weak());
        Ok(Engine {
            engine,
            component,
            linker: Arc::new(linker),
            cache_dir: cache_dir.map(Path::to_path_buf),
        })
    }

    /// Compiles the native extension component in file `path`.
    pub(crate) fn native(&self, path: &Path) -> Result<Component, Error> {
        let bytes = std::fs::read(path)
            .map_err(|err| Error::Compile(format!("{}: {err}", path.display())))?;
        compile(
            &self.engine,
            &bytes,
            &|| Ok(bytes.clone()),
            self.cache_dir.as_deref(),
            "native",
        )
        .map_err(|err| Error::Compile(format!("{}: {err}", path.display())))
    }
}

fn cache_path(engine: &wasmtime::Engine, bytes: &[u8], dir: &Path, name: &str) -> PathBuf {
    let mut compatibility = std::collections::hash_map::DefaultHasher::new();
    engine
        .precompile_compatibility_hash()
        .hash(&mut compatibility);
    let mut digest = Sha256::new();
    digest.update(bytes);
    digest.update(compatibility.finish().to_le_bytes());
    let hex = yapi_types::time::hex(&digest.finalize()[..16]);
    dir.join(format!("{name}-{hex}.cwasm"))
}

/// The runtime component's wasm.
fn inflate_runtime() -> Result<Vec<u8>, Error> {
    let mut wasm = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::DeflateDecoder::new(YAPI_JS), &mut wasm)
        .map_err(|err| Error::Compile(format!("yapi-js: {err}")))?;
    Ok(wasm)
}

/// Compiles the component that `wasm` produces, through a cache in
/// `cache_dir` named after `name` and keyed by `key`, which identifies the
/// component. `wasm` runs only when the cache has no entry. Compiling the
/// runtime under a new key removes its older entries.
fn compile(
    engine: &wasmtime::Engine,
    key: &[u8],
    wasm: &dyn Fn() -> Result<Vec<u8>, Error>,
    cache_dir: Option<&Path>,
    name: &str,
) -> Result<Component, Error> {
    let Some(dir) = cache_dir else {
        return Component::new(engine, wasm()?).map_err(Error::compile);
    };
    let path = cache_path(engine, key, dir, name);
    if path.is_file() {
        // SAFETY: the file holds a component precompiled from the wasm `key`
        // identifies, by an engine with this configuration; its name carries
        // both hashes.
        // Writing it takes write access to the agent directory, which already
        // grants everything an extension could do.
        if let Ok(component) = unsafe { Component::deserialize_file(engine, &path) } {
            return Ok(component);
        }
    }
    let compiled = engine
        .precompile_component(&wasm()?)
        .map_err(Error::compile)?;
    // A missing cache only costs the next start a compile.
    if write_cache(&path, &compiled) && name == "yapi-js" {
        remove_stale(dir, &path);
    }
    // SAFETY: `compiled` was produced by `precompile_component` on this engine
    // just above.
    unsafe { Component::deserialize(engine, &compiled) }.map_err(Error::compile)
}

/// Writes a cache file whole: through a partial file that is renamed into
/// place, so a concurrent reader sees the old file or the new one. Whether the
/// file was written.
pub(crate) fn write_cache(path: &Path, contents: impl AsRef<[u8]>) -> bool {
    let partial = path.with_extension(format!("{}.tmp", std::process::id()));
    path.parent()
        .is_some_and(|dir| std::fs::create_dir_all(dir).is_ok())
        && std::fs::write(&partial, contents).is_ok()
        && std::fs::rename(&partial, path).is_ok()
}

/// Removes runtimes compiled from other versions of `YAPI_JS` or by other
/// engine configurations.
fn remove_stale(dir: &Path, current: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path != current && name.starts_with("yapi-js-") && name.ends_with(".cwasm") {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Advances the engine's epoch until the engine is dropped.
fn start_ticker(engine: EngineWeak) {
    let _ = std::thread::Builder::new()
        .name("yapi-ext-epoch".into())
        .spawn(move || {
            loop {
                std::thread::sleep(TICK);
                match engine.upgrade() {
                    Some(engine) => engine.increment_epoch(),
                    None => break,
                }
            }
        });
}
