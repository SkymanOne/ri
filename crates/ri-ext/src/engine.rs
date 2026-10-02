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
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").finish_non_exhaustive()
    }
}

impl Engine {
    /// Compiles the runtime component, or loads it from `cache_dir` when an
    /// earlier compile by the same engine configuration is there.
    pub fn new(cache_dir: Option<&Path>) -> Result<Engine, Error> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.epoch_interruption(true);
        let engine = wasmtime::Engine::new(&config).map_err(Error::compile)?;
        let component = load_component(&engine, cache_dir)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(Error::compile)?;
        Extension::add_to_linker::<State, HasSelf<State>>(&mut linker, |state| state)
            .map_err(Error::compile)?;
        start_ticker(engine.weak());
        Ok(Engine {
            engine,
            component,
            linker: Arc::new(linker),
        })
    }
}

fn cache_path(engine: &wasmtime::Engine, dir: &Path) -> PathBuf {
    let mut compatibility = std::collections::hash_map::DefaultHasher::new();
    engine
        .precompile_compatibility_hash()
        .hash(&mut compatibility);
    let mut digest = Sha256::new();
    digest.update(RI_JS);
    digest.update(compatibility.finish().to_le_bytes());
    let hex: String = digest
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    dir.join(format!("ri-js-{hex}.cwasm"))
}

fn load_component(engine: &wasmtime::Engine, cache_dir: Option<&Path>) -> Result<Component, Error> {
    let Some(dir) = cache_dir else {
        return Component::new(engine, RI_JS).map_err(Error::compile);
    };
    let path = cache_path(engine, dir);
    if path.is_file() {
        // SAFETY: the file holds a component precompiled from `RI_JS` by an
        // engine with this configuration; its name carries both hashes.
        // Writing it takes write access to the agent directory, which already
        // grants everything an extension could do.
        if let Ok(component) = unsafe { Component::deserialize_file(engine, &path) } {
            return Ok(component);
        }
    }
    let compiled = engine.precompile_component(RI_JS).map_err(Error::compile)?;
    // A missing cache only costs the next start a compile.
    if std::fs::create_dir_all(dir).is_ok() {
        let partial = path.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&partial, &compiled).is_ok() && std::fs::rename(&partial, &path).is_ok() {
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
