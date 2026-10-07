//! One `yapi-js` instance and the thread that owns it.
//!
//! The actor thread owns the wasmtime store and runs one guest export at a
//! time. Calls and operation results arrive as commands; operations the guest
//! starts run on the tokio runtime and come back as commands, so host code
//! never calls into the guest while the guest is running.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::sync::oneshot;
use wasmtime::component::{Component, ResourceTable};
use wasmtime::{Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};

use crate::Error;
use crate::engine::{Engine, Extension, yapi as wit};
use crate::loader::Loader;
use crate::requests::{AiStreams, Host};

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
    /// Send HTTP requests and resolve host names.
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

    /// Ends a guest step: called on the instance's thread after each guest
    /// call, once the operations it began have started and before its results
    /// are delivered.
    fn step_ended(&self) {}

    /// The request hooks of running extension stream `id`, for the requests
    /// it makes through yapi's wire APIs.
    fn stream_hooks(&self, _id: u64) -> yapi_ai::stream::RequestHooks {
        yapi_ai::stream::RequestHooks::default()
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
    /// The directory `os.homedir()` names.
    pub home_dir: PathBuf,
    /// The directory `os.tmpdir()` names.
    pub temp_dir: PathBuf,
    /// What the instance may reach.
    pub grants: Grants,
    /// The directories file access reaches, when granted.
    pub filesystem_roots: Vec<PathBuf>,
    /// The most linear memory the instance may use, in bytes.
    pub memory_limit: usize,
    /// Where transpiled modules are cached; `None` disables the cache.
    pub cache_dir: Option<PathBuf>,
    /// The environment variables extensions see when the environment is
    /// granted: these alone, or with `None`, the process's own.
    pub environment: Option<std::collections::BTreeMap<String, String>>,
}

impl Options {
    /// Full grants and a 1 GiB memory limit in `cwd`, with the configured
    /// agent directory.
    pub fn new(cwd: PathBuf) -> Options {
        Options {
            cwd,
            agent_dir: yapi_core::config::agent_dir(),
            home_dir: yapi_core::tools::path::home_dir(),
            temp_dir: std::env::temp_dir(),
            grants: Grants::default(),
            filesystem_roots: vec![PathBuf::from("/")],
            memory_limit: 1 << 30,
            cache_dir: None,
            environment: None,
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
    /// Set when the instance is dropped: the running export traps at the next
    /// epoch tick.
    interrupt: Arc<AtomicBool>,
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
            .map(|value| yapi_types::json::stringify(&value));
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
    Render {
        handle: u32,
        width: u32,
        reply: oneshot::Sender<Vec<String>>,
    },
    Input {
        handle: u32,
        data: String,
    },
    Settle(oneshot::Sender<()>),
    Stop,
}

/// A running `yapi-js` instance. Dropping it stops the instance, interrupting
/// the guest if it is computing.
pub struct Instance {
    commands: mpsc::Sender<Command>,
    interrupt: Arc<AtomicBool>,
    /// The actor thread; taken when the instance is dropped.
    thread: Option<JoinHandle<()>>,
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
        let runtime = tokio::runtime::Handle::current();
        let host = Arc::new(Host {
            loader: Loader::new(options.cwd.clone(), options.cache_dir.clone()),
            bridge,
            options,
            ai_streams: AiStreams::default(),
            streams: crate::ops::Streams::new(runtime.clone()),
        });
        let (commands, receiver) = mpsc::channel();
        let (ready, started) = oneshot::channel();
        let engine = engine.clone();
        let sender = commands.clone();
        let interrupt = Arc::new(AtomicBool::new(false));
        let flag = interrupt.clone();
        let thread = std::thread::Builder::new()
            .name("yapi-ext-instance".into())
            // Guest code runs on this thread's stack.
            .stack_size(crate::engine::WASM_STACK + (2 << 20))
            .spawn(
                move || match Actor::new(engine, component, host, runtime, sender, flag) {
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
        Ok(Instance {
            commands,
            interrupt,
            thread: Some(thread),
        })
    }

    /// Runs dispatch `kind` with `payload`, queued at once after the calls
    /// and input sent before it, and returns its result.
    pub fn call(
        &self,
        kind: &str,
        payload: &Value,
    ) -> impl Future<Output = Result<Value, Error>> + use<> {
        let result = self.send_call(kind, payload);
        async move {
            result
                .ok_or(Error::Stopped)?
                .await
                .map_err(|_| Error::Stopped)?
        }
    }

    /// Runs dispatch `kind` with `payload` without waiting for its result,
    /// after the calls and input sent before it.
    pub fn post(&self, kind: &str, payload: &Value) {
        self.send_call(kind, payload);
    }

    /// Queues dispatch `kind`; the receiver of its result, or `None` once the
    /// instance stopped.
    fn send_call(
        &self,
        kind: &str,
        payload: &Value,
    ) -> Option<oneshot::Receiver<Result<Value, Error>>> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Call {
                kind: kind.to_owned(),
                payload: yapi_types::json::stringify(payload),
                reply,
            })
            .ok()?;
        Some(result)
    }

    /// The lines of component `handle` at `width` columns; empty when the
    /// component or the instance is gone.
    pub async fn render(&self, handle: u32, width: u32) -> Vec<String> {
        let (reply, result) = oneshot::channel();
        if self
            .commands
            .send(Command::Render {
                handle,
                width,
                reply,
            })
            .is_err()
        {
            return Vec::new();
        }
        result.await.unwrap_or_default()
    }

    /// Resolves once the instance has handled what was sent to it before,
    /// such as a call it is running: a call that finished has delivered its
    /// result by then.
    pub async fn settle(&self) {
        let (done, settled) = oneshot::channel();
        if self.commands.send(Command::Settle(done)).is_ok() {
            let _ = settled.await;
        }
    }

    /// Delivers raw terminal input to component `handle`.
    pub fn input(&self, handle: u32, data: &str) {
        let _ = self.commands.send(Command::Input {
            handle,
            data: data.to_owned(),
        });
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.interrupt.store(true, Ordering::Relaxed);
        let _ = self.commands.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let mut stopping = yapi_types::sync::lock(&STOPPING);
            stopping.retain(|thread| !thread.is_finished());
            stopping.push(thread);
        }
    }
}

/// How long [`join_stopped`] waits, in total, for the threads of dropped
/// instances.
const STOP_WAIT: Duration = Duration::from_secs(2);

/// Threads of dropped instances that may still be tearing down their stores.
static STOPPING: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

/// Waits for the threads of dropped instances to finish, up to two seconds in
/// total, and leaves behind those still running then. Call it before the
/// process exits: exiting while a thread frees an instance's compiled code
/// can abort the process.
pub fn join_stopped() {
    let threads = std::mem::take(&mut *yapi_types::sync::lock(&STOPPING));
    let deadline = Instant::now() + STOP_WAIT;
    while threads.iter().any(|thread| !thread.is_finished()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    for thread in threads {
        if thread.is_finished() {
            let _ = thread.join();
        }
    }
}

struct Actor {
    engine: Engine,
    component: Component,
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
    interrupt: Arc<AtomicBool>,
}

impl Actor {
    fn new(
        engine: Engine,
        component: Component,
        host: Arc<Host>,
        runtime: tokio::runtime::Handle,
        commands: mpsc::Sender<Command>,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Actor, Error> {
        let (store, bindings) = instantiate(&engine, &component, &host, &interrupt)?;
        Ok(Actor {
            engine,
            component,
            host,
            runtime,
            commands,
            store,
            bindings,
            generation: 0,
            next_id: 1,
            pending: HashMap::new(),
            replay: Vec::new(),
            interrupt,
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
                            .yapi_extension_guest()
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
                    let value = value.map(|value| yapi_types::json::stringify(&value));
                    self.guest(|bindings, store| {
                        bindings.yapi_extension_guest().call_resolve(
                            store,
                            op,
                            value.as_ref().map_err(String::as_str),
                        )
                    });
                }
                Command::Render {
                    handle,
                    width,
                    reply,
                } => {
                    let _ = reply.send(self.render(handle, width));
                }
                Command::Input { handle, data } => self.guest(|bindings, store| {
                    bindings
                        .yapi_extension_guest()
                        .call_input(store, handle, &data)
                }),
                Command::Settle(done) => {
                    let _ = done.send(());
                }
                Command::Stop => break,
            }
            if self.interrupt.load(Ordering::Relaxed) {
                break;
            }
        }
    }

    /// Starts the clock and the epoch deadline of a guest call.
    fn start_call(&mut self) {
        let state = self.store.data_mut();
        state.call_started = Instant::now();
        state.host_time = Duration::ZERO;
        self.store.set_epoch_deadline(1);
    }

    fn render(&mut self, handle: u32, width: u32) -> Vec<String> {
        self.start_call();
        match self
            .bindings
            .yapi_extension_guest()
            .call_render(&mut self.store, handle, width)
        {
            Ok(lines) => lines,
            Err(trap) => {
                self.restart(&format!("{trap:#}"));
                Vec::new()
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
        self.start_call();
        let result = call(&self.bindings, &mut self.store);
        let started = std::mem::take(&mut self.store.data_mut().started);
        match result {
            Ok(outcomes) => {
                for (op, kind, payload) in started {
                    self.start_op(op, &kind, &payload);
                }
                self.host.bridge.step_ended();
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
        if self.interrupt.load(Ordering::Relaxed) {
            for (_, reply) in self.pending.drain() {
                let _ = reply.send(Err(Error::Stopped));
            }
            return;
        }
        self.host
            .bridge
            .log("error", &format!("Extension runtime stopped: {reason}"));
        self.host.streams.clear();
        for (_, reply) in self.pending.drain() {
            let _ = reply.send(Err(Error::Crashed(reason.to_owned())));
        }
        self.generation += 1;
        match instantiate(&self.engine, &self.component, &self.host, &self.interrupt) {
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
                    .yapi_extension_guest()
                    .call_dispatch(store, id, &kind, &payload)
            });
        }
    }
}

fn instantiate(
    engine: &Engine,
    component: &Component,
    host: &Arc<Host>,
    interrupt: &Arc<AtomicBool>,
) -> Result<(Store<State>, Extension), Error> {
    let options = &host.options;
    let mut wasi = WasiCtx::builder();
    if options.grants.filesystem {
        for root in &options.filesystem_roots {
            wasi.preopened_dir(root, root.to_string_lossy(), FsPerms::ReadWrite)
                .map_err(|err| Error::Instantiate(format!("{}: {err}", root.display())))?;
        }
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
        interrupt: interrupt.clone(),
    };
    let mut store = Store::new(&engine.engine, state);
    store.limiter(|state| &mut state.limits);
    store.epoch_deadline_callback(|context| {
        let state = context.data();
        if state.interrupt.load(Ordering::Relaxed) {
            return Err(wasmtime::Error::msg("the instance was stopped"));
        }
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
