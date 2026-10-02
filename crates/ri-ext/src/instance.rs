//! One `ri-js` instance and the thread that owns it.
//!
//! The actor thread owns the wasmtime store and runs one guest export at a
//! time. Calls and operation results arrive as commands; operations the guest
//! starts run on the tokio runtime and come back as commands, so host code
//! never calls into the guest while the guest is running.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::sync::oneshot;
use wasmtime::component::{Component, ResourceTable};
use wasmtime::{Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};

use crate::Error;
use crate::engine::{Engine, Extension, ri as wit};
use crate::loader::Loader;
use crate::requests::Host;

/// How long one guest export may compute, not counting time spent in host
/// requests, before the instance is stopped.
const CALL_LIMIT: Duration = Duration::from_secs(60);

/// What an instance may reach. The default grants everything, as pi does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grants {
    /// Read and write files.
    pub filesystem: bool,
    /// Run processes.
    pub process: bool,
    /// Send HTTP requests.
    pub network: bool,
    /// Read environment variables.
    pub environment: bool,
}

impl Default for Grants {
    fn default() -> Grants {
        Grants {
            filesystem: true,
            process: true,
            network: true,
            environment: true,
        }
    }
}

impl Grants {
    /// Nothing outside the instance.
    pub fn none() -> Grants {
        Grants {
            filesystem: false,
            process: false,
            network: false,
            environment: false,
        }
    }
}

/// The session side of an instance: requests and operations the runtime does
/// not answer itself, and console output.
pub trait Bridge: Send + Sync {
    /// Answers request `kind` at once. Runs on the instance's thread while the
    /// guest waits, so it must not block for long.
    fn request(&self, kind: &str, _payload: &Value) -> Result<Value, String> {
        Err(format!("{kind} is not available here"))
    }

    /// Starts operation `kind`; the guest gets its result when it completes.
    fn start(&self, kind: &str, _payload: Value) -> BoxFuture<'static, Result<Value, String>> {
        let message = format!("{kind} is not available here");
        Box::pin(async move { Err(message) })
    }

    /// Console output from extensions, at `level` `debug`, `info`, `warn` or
    /// `error`.
    fn log(&self, _level: &str, message: &str) {
        let _ = writeln!(std::io::stderr(), "{message}");
    }
}

/// A bridge to nothing: only the runtime's own requests work.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoBridge;

impl Bridge for NoBridge {}

/// How to start an instance.
#[derive(Clone, Debug)]
pub struct Options {
    /// The working directory extensions see.
    pub cwd: PathBuf,
    /// The agent directory extensions see.
    pub agent_dir: PathBuf,
    /// What the instance may reach.
    pub grants: Grants,
    /// The most linear memory the instance may use, in bytes.
    pub memory_limit: usize,
    /// Where transpiled modules are cached; `None` disables the cache.
    pub cache_dir: Option<PathBuf>,
}

impl Options {
    /// Full grants and a 1 GiB memory limit in `cwd`, with the configured
    /// agent directory.
    pub fn new(cwd: PathBuf) -> Options {
        Options {
            cwd,
            agent_dir: ri_core::config::agent_dir(),
            grants: Grants::default(),
            memory_limit: 1 << 30,
            cache_dir: None,
        }
    }
}

/// The store's data: WASI state, limits and the host side of the WIT world.
pub(crate) struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    host: Arc<Host>,
    started: Vec<(u64, String, String)>,
    call_started: Instant,
    host_time: Duration,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl wit::extension::types::Host for State {}

impl wit::extension::host::Host for State {
    fn start(&mut self, op: u64, kind: String, payload: String) {
        self.started.push((op, kind, payload));
    }

    fn request(&mut self, kind: String, payload: String) -> Result<String, String> {
        let begun = Instant::now();
        let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        let result = self
            .host
            .request(&kind, &payload)
            .map(|value| ri_types::json::to_string(&value).unwrap_or_else(|_| "null".into()));
        self.host_time += begun.elapsed();
        result
    }
}

enum Command {
    Call {
        kind: String,
        payload: String,
        reply: oneshot::Sender<Result<Value, Error>>,
    },
    Resolve {
        generation: u64,
        op: u64,
        value: Result<Value, String>,
    },
    Stop,
}

/// A running `ri-js` instance. Dropping it stops the instance.
pub struct Instance {
    commands: mpsc::Sender<Command>,
}

impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance").finish_non_exhaustive()
    }
}

impl Instance {
    /// Starts an instance of the JS runtime on its own thread. Operations run
    /// on the current tokio runtime.
    pub async fn start(
        engine: &Engine,
        options: Options,
        bridge: Arc<dyn Bridge>,
    ) -> Result<Instance, Error> {
        Instance::start_component(engine, engine.component.clone(), options, bridge).await
    }

    /// Starts an instance of a native extension `component`, compiled by
    /// [`Engine::native`].
    pub(crate) async fn start_component(
        engine: &Engine,
        component: Component,
        options: Options,
        bridge: Arc<dyn Bridge>,
    ) -> Result<Instance, Error> {
        let host = Arc::new(Host {
            loader: Loader::new(options.cwd.clone(), options.cache_dir.clone()),
            bridge,
            grants: options.grants,
            cwd: options.cwd.clone(),
            agent_dir: options.agent_dir.clone(),
        });
        let (commands, receiver) = mpsc::channel();
        let (ready, started) = oneshot::channel();
        let engine = engine.clone();
        let runtime = tokio::runtime::Handle::current();
        let sender = commands.clone();
        std::thread::Builder::new()
            .name("ri-ext-instance".into())
            .spawn(
                move || match Actor::new(engine, component, options, host, runtime, sender) {
                    Ok(actor) => {
                        let _ = ready.send(Ok(()));
                        actor.run(&receiver);
                    }
                    Err(err) => {
                        let _ = ready.send(Err(err));
                    }
                },
            )
            .map_err(|err| Error::Instantiate(err.to_string()))?;
        started.await.map_err(|_| Error::Stopped)??;
        Ok(Instance { commands })
    }

    /// Runs dispatch `kind` with `payload` and waits for its result.
    pub async fn call(&self, kind: &str, payload: &Value) -> Result<Value, Error> {
        let payload =
            ri_types::json::to_string(payload).map_err(|err| Error::Call(err.to_string()))?;
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Call {
                kind: kind.to_owned(),
                payload,
                reply,
            })
            .map_err(|_| Error::Stopped)?;
        result.await.map_err(|_| Error::Stopped)?
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
    }
}

struct Actor {
    engine: Engine,
    component: Component,
    options: Options,
    host: Arc<Host>,
    runtime: tokio::runtime::Handle,
    commands: mpsc::Sender<Command>,
    store: Store<State>,
    bindings: Extension,
    generation: u64,
    next_id: u64,
    pending: HashMap<u64, oneshot::Sender<Result<Value, Error>>>,
    /// Calls that shape the instance's state, replayed after a restart.
    replay: Vec<(String, String)>,
}

impl Actor {
    fn new(
        engine: Engine,
        component: Component,
        options: Options,
        host: Arc<Host>,
        runtime: tokio::runtime::Handle,
        commands: mpsc::Sender<Command>,
    ) -> Result<Actor, Error> {
        let (store, bindings) = instantiate(&engine, &component, &options, &host)?;
        Ok(Actor {
            engine,
            component,
            options,
            host,
            runtime,
            commands,
            store,
            bindings,
            generation: 0,
            next_id: 1,
            pending: HashMap::new(),
            replay: Vec::new(),
        })
    }

    fn run(mut self, commands: &mpsc::Receiver<Command>) {
        while let Ok(command) = commands.recv() {
            match command {
                Command::Call {
                    kind,
                    payload,
                    reply,
                } => {
                    let id = self.next_id;
                    self.next_id += 1;
                    self.pending.insert(id, reply);
                    if matches!(kind.as_str(), "load" | "bind" | "flags") {
                        self.replay.push((kind.clone(), payload.clone()));
                    }
                    self.guest(|bindings, store| {
                        bindings
                            .ri_extension_guest()
                            .call_dispatch(store, id, &kind, &payload)
                    });
                }
                Command::Resolve {
                    generation,
                    op,
                    value,
                } => {
                    if generation != self.generation {
                        continue;
                    }
                    let value = match value {
                        Ok(value) => {
                            Ok(ri_types::json::to_string(&value).unwrap_or_else(|_| "null".into()))
                        }
                        Err(message) => Err(message),
                    };
                    self.guest(|bindings, store| {
                        bindings.ri_extension_guest().call_resolve(
                            store,
                            op,
                            value.as_ref().map_err(String::as_str),
                        )
                    });
                }
                Command::Stop => break,
            }
        }
    }

    /// Runs one guest export, then delivers its outcomes and starts the
    /// operations it began.
    fn guest(
        &mut self,
        call: impl FnOnce(
            &Extension,
            &mut Store<State>,
        ) -> wasmtime::Result<Vec<wit::extension::types::Outcome>>,
    ) {
        {
            let state = self.store.data_mut();
            state.call_started = Instant::now();
            state.host_time = Duration::ZERO;
        }
        self.store.set_epoch_deadline(1);
        let result = call(&self.bindings, &mut self.store);
        let started = std::mem::take(&mut self.store.data_mut().started);
        match result {
            Ok(outcomes) => {
                for (op, kind, payload) in started {
                    self.start_op(op, &kind, &payload);
                }
                for outcome in outcomes {
                    let (id, result) = match outcome {
                        wit::extension::types::Outcome::Done((id, json)) => (
                            id,
                            serde_json::from_str(&json).map_err(|err| Error::Call(err.to_string())),
                        ),
                        wit::extension::types::Outcome::Failed((id, message)) => {
                            (id, Err(Error::Call(message)))
                        }
                    };
                    if let Some(reply) = self.pending.remove(&id) {
                        let _ = reply.send(result);
                    }
                }
            }
            Err(trap) => self.restart(&format!("{trap:#}")),
        }
    }

    fn start_op(&self, op: u64, kind: &str, payload: &str) {
        let payload: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
        let future = self.host.start(kind, payload);
        let commands = self.commands.clone();
        let generation = self.generation;
        self.runtime.spawn(async move {
            let value = future.await;
            let _ = commands.send(Command::Resolve {
                generation,
                op,
                value,
            });
        });
    }

    /// Replaces a trapped instance with a fresh one and replays the calls that
    /// loaded and configured its extensions. Calls in flight fail.
    fn restart(&mut self, reason: &str) {
        self.host
            .bridge
            .log("error", &format!("Extension runtime stopped: {reason}"));
        for (_, reply) in self.pending.drain() {
            let _ = reply.send(Err(Error::Crashed(reason.to_owned())));
        }
        self.generation += 1;
        match instantiate(&self.engine, &self.component, &self.options, &self.host) {
            Ok((store, bindings)) => {
                self.store = store;
                self.bindings = bindings;
            }
            Err(err) => {
                self.host
                    .bridge
                    .log("error", &format!("Extension runtime cannot restart: {err}"));
                return;
            }
        }
        for (kind, payload) in self.replay.clone() {
            let id = self.next_id;
            self.next_id += 1;
            self.guest(|bindings, store| {
                bindings
                    .ri_extension_guest()
                    .call_dispatch(store, id, &kind, &payload)
            });
        }
    }
}

fn instantiate(
    engine: &Engine,
    component: &Component,
    options: &Options,
    host: &Arc<Host>,
) -> Result<(Store<State>, Extension), Error> {
    let mut wasi = WasiCtx::builder();
    if options.grants.filesystem {
        wasi.preopened_dir("/", "/", FsPerms::ReadWrite)
            .map_err(|err| Error::Instantiate(err.to_string()))?;
    }
    let state = State {
        wasi: wasi.build(),
        table: ResourceTable::new(),
        limits: StoreLimitsBuilder::new()
            .memory_size(options.memory_limit)
            .build(),
        host: host.clone(),
        started: Vec::new(),
        call_started: Instant::now(),
        host_time: Duration::ZERO,
    };
    let mut store = Store::new(&engine.engine, state);
    store.limiter(|state| &mut state.limits);
    store.epoch_deadline_callback(|context| {
        let state = context.data();
        let busy = state.call_started.elapsed().saturating_sub(state.host_time);
        if busy > CALL_LIMIT {
            Err(wasmtime::Error::msg(format!(
                "an extension computed for more than {} seconds without yielding",
                CALL_LIMIT.as_secs()
            )))
        } else {
            Ok(UpdateDeadline::Continue(1))
        }
    });
    let bindings = Extension::instantiate(&mut store, component, &engine.linker)
        .map_err(|err| Error::Instantiate(format!("{err:#}")))?;
    Ok((store, bindings))
}
