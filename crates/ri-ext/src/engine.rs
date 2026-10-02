//! The wasmtime engine and the compiled `ri-js` component.

use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, EngineWeak};

use crate::Error;
use crate::instance::State;

/// The JS runtime component, built by `cargo xtask js-runtime`.
const RI_JS: &[u8] = include_bytes!("../ri-js.wasm");

/// How often the epoch advances; guest time limits are checked at this rate.
pub(crate) const TICK: Duration = Duration::from_millis(10);

wasmtime::component::bindgen!({ path: "../../wit/since_v0.1.0", world: "extension" });

/// A wasmtime engine with the `ri-js` component compiled and linked. Cloning
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
        let engine = wasmtime::Engine::new(&config).map_err(Error::compile)?;
        let component = compile(&engine, RI_JS, cache_dir, "ri-js")?;
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
        compile(&self.engine, &bytes, self.cache_dir.as_deref(), "native")
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
    let hex: String = digest
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    dir.join(format!("{name}-{hex}.cwasm"))
}

/// Compiles component `bytes`, through a cache in `cache_dir` named after
/// `name`. Compiling the runtime under a new hash removes its older entries.
fn compile(
    engine: &wasmtime::Engine,
    bytes: &[u8],
    cache_dir: Option<&Path>,
    name: &str,
) -> Result<Component, Error> {
    let Some(dir) = cache_dir else {
        return Component::new(engine, bytes).map_err(Error::compile);
    };
    let path = cache_path(engine, bytes, dir, name);
    if path.is_file() {
        // SAFETY: the file holds a component precompiled from `bytes` by an
        // engine with this configuration; its name carries both hashes.
        // Writing it takes write access to the agent directory, which already
        // grants everything an extension could do.
        if let Ok(component) = unsafe { Component::deserialize_file(engine, &path) } {
            return Ok(component);
        }
    }
    let compiled = engine.precompile_component(bytes).map_err(Error::compile)?;
    // A missing cache only costs the next start a compile.
    if std::fs::create_dir_all(dir).is_ok() {
        let partial = path.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&partial, &compiled).is_ok()
            && std::fs::rename(&partial, &path).is_ok()
            && name == "ri-js"
        {
            remove_stale(dir, &path);
        }
    }
    // SAFETY: `compiled` was produced by `precompile_component` on this engine
    // just above.
    unsafe { Component::deserialize(engine, &compiled) }.map_err(Error::compile)
}

/// Removes runtimes compiled from other versions of `RI_JS` or by other
/// engine configurations.
fn remove_stale(dir: &Path, current: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path != current && name.starts_with("ri-js-") && name.ends_with(".cwasm") {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Advances the engine's epoch until the engine is dropped.
fn start_ticker(engine: EngineWeak) {
    let _ = std::thread::Builder::new()
        .name("ri-ext-epoch".into())
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
