//! QuickJS-ng execution runtime and async event-loop driver.
//!
//! This module owns the initialization of the QuickJS engine, registration
//! of host globals (`console`, `process`, timers, `queueMicrotask`), module loading,
//! and driving the event loop to completion using Tokio.

use crate::effect_record::{Outcome, PendingEffect};
use crate::error::{Result, VaernError};
use crate::event::{
    EventReceiver, EventSender, FetchCompletion, FsCompletion, LoopEvent, ModelCompletion,
    StreamChunkSent,
};
use crate::journal::Journal;
use crate::module_loader::{FsModuleLoader, ModuleLoader};
use crate::server::{HttpServer, ServerIncomingRequest, ServerResponse, ServerResponseBody};
use crate::transpile::{SwcTranspiler, Transpiler};
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::{
    Array, ArrayBuffer, Context, Ctx, Function, Module, Object, Runtime as QjsRuntime, Value,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

/// Configuration options for initializing a Vaern runtime instance.
#[derive(Debug, Clone, Default)]
pub struct RuntimeOptions {
    /// Command-line arguments forwarded to `process.argv`.
    pub argv: Vec<String>,
}

/// Trait defining the lifecycle and execution of a Vaern runtime instance.
pub trait Runtime {
    /// Executes a script file from disk, returning its process exit code.
    fn run_file(&mut self, path: &Path) -> Result<i32>;

    /// Executes raw JavaScript/TypeScript source code directly.
    fn eval(&mut self, filename: &str, source: &str) -> Result<i32>;
}

#[derive(Debug, Clone)]
struct TimerRecord {
    deadline: Instant,
    interval: Option<Duration>,
}

struct SharedState {
    timers: HashMap<u32, TimerRecord>,
    abort_senders: HashMap<u32, tokio::sync::broadcast::Sender<()>>,
    pending_fetches: usize,
    /// Next effect id, in dispatch order; see `effect_record`.
    next_eid: u64,
    /// Journalled effects in flight, keyed by kind and request id.
    pending_effects: HashMap<(&'static str, u32), PendingEffect>,
    /// `vaern:fs` operations running on the blocking pool.
    pending_fs: usize,
    /// `vaern:ai` model calls in flight.
    pending_models: usize,
    /// Production HTTP client shared by every fetch, built on the first one.
    /// Reusing it keeps connections alive and resumes TLS sessions.
    fetch_client: Option<crate::fetch::HttpClient>,
    events_tx: EventSender,
    active_servers: usize,
    server_handles: HashMap<u32, Arc<HttpServer>>,
    server_resp_senders: HashMap<u64, tokio::sync::oneshot::Sender<ServerResponse>>,
    server_stream_senders:
        HashMap<u64, tokio::sync::mpsc::Sender<std::result::Result<bytes::Bytes, std::io::Error>>>,
    /// Streamed responses whose last chunk is waiting for buffer space. Each
    /// keeps the event loop alive until its outcome is delivered to JS.
    stream_sends_waiting: HashSet<u64>,
    /// First unhandled promise rejection or uncaught timer exception. The event
    /// loop turns it into an error so a throwing script cannot exit with status 0.
    unhandled_error: Option<String>,
    /// Rejected promises with no handler yet, keyed by promise identity, in
    /// rejection order. A handler attached later removes only its own entry.
    pending_rejections: Vec<(u64, String)>,
    /// Event-loop turns so far. Outcomes handed over in one turn all reach
    /// the script before any promise callback runs, which decides the order
    /// their callbacks interleave; effect records keep it for replay.
    turn: u64,
}

/// Identity key for a promise. `Value` hashes its tag and raw bits, so equal
/// keys mean the same object while it is alive. No reference is retained,
/// which keeps the tracker from extending object lifetimes across the FFI.
fn promise_identity(promise: &Value<'_>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    promise.hash(&mut hasher);
    hasher.finish()
}

struct VaernModuleResolver {
    base_dir: PathBuf,
    /// Full error of the last failed resolution. QuickJS formats a thrown
    /// message into a 256-byte buffer, which would cut off the list of paths
    /// tried, so the script sees the short reason and the top-level report
    /// uses this.
    failure: Rc<RefCell<Option<VaernError>>>,
}

impl Resolver for VaernModuleResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        if name == "vaern:http" || name == "vaern:fs" || name == "vaern:ai" {
            return Ok(name.to_string());
        }
        let referrer_path = if base.is_empty() {
            self.base_dir.clone()
        } else {
            PathBuf::from(base)
        };
        let loader = FsModuleLoader;
        let resolved = loader.resolve(name, &referrer_path).map_err(|err| {
            let short = match &err {
                VaernError::ModuleResolution { reason, .. } => reason.clone(),
                other => other.to_string(),
            };
            *self.failure.borrow_mut() = Some(err);
            rquickjs::Error::new_loading_message(name, short)
        })?;
        Ok(resolved.to_string_lossy().replace('\\', "/"))
    }
}

struct VaernModuleLoader {
    transpiler: SwcTranspiler,
    /// Real paths of the working directory and the entry script's directory.
    /// Parse errors in modules outside both keep their location and lose
    /// their message, which can quote the offending token.
    project_roots: Vec<PathBuf>,
    journal: JournalSlot,
}

impl VaernModuleLoader {
    fn new(transpiler: SwcTranspiler, base_dir: &Path, journal: JournalSlot) -> Self {
        let real = |path: &Path| {
            std::fs::canonicalize(path)
                .ok()
                .map(crate::fs::strip_verbatim)
        };
        let project_roots = [std::env::current_dir().ok(), Some(base_dir.to_path_buf())]
            .into_iter()
            .flatten()
            .filter_map(|dir| real(&dir))
            .collect();
        Self {
            transpiler,
            project_roots,
            journal,
        }
    }

    fn outside_project(&self, name: &str) -> bool {
        match std::fs::canonicalize(name) {
            Ok(path) => {
                let path = crate::fs::strip_verbatim(path);
                !self.project_roots.iter().any(|root| path.starts_with(root))
            }
            Err(_) => true,
        }
    }
}

/// The message for a module outside the project that fails to parse.
fn redacted_parse_error(name: &str, location: Option<(usize, usize)>) -> rquickjs::Error {
    let place = match location {
        Some((line, col)) => format!(" at line {line}, column {col}"),
        None => String::new(),
    };
    rquickjs::Error::new_loading_message(
        name,
        format!(
            "the module failed to parse{place}; the parser's message is not shown for files outside the project, because it can quote their contents"
        ),
    )
}

/// `line:col` from the first stack line that ends with one.
fn stack_location(stack: &str) -> Option<(usize, usize)> {
    stack.lines().find_map(|line| {
        let line = line.trim().trim_end_matches(')');
        let (rest, col) = line.rsplit_once(':')?;
        let (_, row) = rest.rsplit_once(':')?;
        Some((row.parse().ok()?, col.parse().ok()?))
    })
}

impl Loader for VaernModuleLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, rquickjs::module::Declared>> {
        if name == "vaern:http" {
            return Module::declare(
                ctx.clone(),
                name,
                "export const serve = globalThis.__vaern_serve;\nexport default { serve };",
            );
        }
        if name == "vaern:fs" {
            return Module::declare(
                ctx.clone(),
                name,
                "const { readFile, writeFile, appendFile } = globalThis.__vaern_fs;\n\
                 export { readFile, writeFile, appendFile };\n\
                 export default { readFile, writeFile, appendFile };",
            );
        }
        if name == "vaern:ai" {
            return Module::declare(
                ctx.clone(),
                name,
                "const { model, ModelError, tool, ToolError, agent, AgentError, budget, BudgetError } =\n\
                     globalThis.__vaern_ai;\n\
                 export { model, ModelError, tool, ToolError, agent, AgentError, budget, BudgetError };\n\
                 export default { model, ModelError, tool, ToolError, agent, AgentError, budget, BudgetError };",
            );
        }
        let content = FsModuleLoader
            .load(Path::new(name))
            .map_err(|err| rquickjs::Error::new_loading_message(name, err.to_string()))?;
        if let Some(journal) = self.journal.borrow_mut().as_mut() {
            let mut fields = serde_json::Map::new();
            fields.insert("path".into(), name.into());
            fields.insert(
                "sha256".into(),
                crate::journal::sha256_hex(content.as_bytes()).into(),
            );
            journal
                .append("module.load", fields)
                .map_err(|err| rquickjs::Error::new_loading_message(name, err.to_string()))?;
        }
        let outside = self.outside_project(name);
        let transpiled = if name.ends_with(".ts") {
            self.transpiler
                .transpile(name, &content)
                .map_err(|err| match err {
                    VaernError::Syntax { line, col, .. }
                    | VaernError::UnsupportedFeature { line, col, .. }
                        if outside =>
                    {
                        redacted_parse_error(name, Some((line, col)))
                    }
                    _ if outside => redacted_parse_error(name, None),
                    other => rquickjs::Error::new_loading_message(name, format!("{other}")),
                })?
        } else {
            content
        };
        match Module::declare(ctx.clone(), name, transpiled) {
            Err(rquickjs::Error::Exception) if outside => {
                let thrown = ctx.catch();
                let stack = thrown
                    .as_object()
                    .and_then(|obj| obj.get::<_, String>("stack").ok())
                    .unwrap_or_default();
                Err(redacted_parse_error(name, stack_location(&stack)))
            }
            other => other,
        }
    }
}

/// Ends the process with `code`. Declared as returning `()` because the
/// QuickJS function bridge cannot convert the never type into a JS value.
fn exit_process(code: i32) {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code)
}

/// Renders a rejection reason as `Name: message` followed by the stack, whose
/// frames carry the `file:line:col` locations.
fn describe_rejection(reason: &Value<'_>) -> String {
    if let Some(exc) = reason.as_exception() {
        let name = exc
            .get::<_, Option<String>>("name")
            .ok()
            .flatten()
            .unwrap_or_else(|| "Error".to_string());
        let message = exc.message().unwrap_or_default();
        let stack = exc.stack().unwrap_or_default();
        let head = if message.is_empty() {
            name
        } else {
            format!("{name}: {message}")
        };
        let stack = stack.trim_end();
        return if stack.is_empty() {
            head
        } else if stack.starts_with(&head) {
            stack.to_string()
        } else {
            format!("{head}\n{stack}")
        };
    }
    if let Some(text) = reason.as_string().and_then(|s| s.to_string().ok()) {
        return format!("uncaught value: {text}");
    }
    "unhandled promise rejection with a non-error value".to_string()
}

/// Chunks buffered between the JS stream pump and the hyper connection.
const STREAM_CHUNK_CAPACITY: usize = 32;

/// Ceiling on the QuickJS heap. A script that allocates past it gets an
/// out-of-memory error it can report, instead of growing until the host
/// kills the process. Generous for scripts; a per-run limit comes with the
/// policy file.
pub(crate) const HEAP_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// How long the pump waits for buffer space before treating the client as stalled.
const STREAM_CHUNK_SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Notes a dispatched effect so its completion can be journalled.
fn track_effect(
    state: &Rc<RefCell<SharedState>>,
    kind: &'static str,
    req_id: u32,
    req: serde_json::Value,
) {
    let mut s = state.borrow_mut();
    let eid = s.next_eid;
    s.next_eid += 1;
    s.pending_effects.insert(
        (kind, req_id),
        PendingEffect {
            eid,
            started: Instant::now(),
            req,
        },
    );
}

/// 128 bits from the operating system for a journalled run's `Math.random`.
fn random_seed() -> std::result::Result<[u32; 4], String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "the system random source is unavailable".to_string())?;
    let mut seed = [0u32; 4];
    for (word, chunk) in seed.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*chunk);
    }
    Ok(seed)
}

/// The shared HTTP client, built on first use. Fetch and model calls share
/// it so they share kept-alive connections and TLS sessions.
fn shared_client(state: &Rc<RefCell<SharedState>>) -> Result<crate::fetch::HttpClient> {
    let mut s = state.borrow_mut();
    match &s.fetch_client {
        Some(client) => Ok(client.clone()),
        None => crate::fetch::build_http_client().inspect(|client| {
            s.fetch_client = Some(client.clone());
        }),
    }
}

/// A model call failure in the shape `ModelError` is built from.
fn model_error_info(err: VaernError) -> serde_json::Value {
    match err {
        VaernError::Model {
            stage,
            url,
            status,
            message,
            action,
            ..
        } => serde_json::json!({
            "stage": stage, "url": url, "status": status, "message": message, "action": action,
        }),
        VaernError::Budget {
            code,
            unit,
            reserve,
            remaining,
            message,
            action,
        } => serde_json::json!({
            "budget": true, "stage": "budget", "code": code, "unit": unit,
            "reserve": reserve, "remaining": remaining, "message": message, "action": action,
        }),
        VaernError::Journal { .. } => serde_json::json!({
            "stage": "journal", "url": "", "status": 0, "message": err.to_string(),
            "action": "The call completed but could not be recorded; check the journal directory.",
        }),
        other => serde_json::json!({
            "stage": "connect", "url": "", "status": 0, "message": other.to_string(),
            "action": "Inspect the error details.",
        }),
    }
}

/// The error for a chunk written after the client went away.
fn stream_closed(req_id: u64) -> VaernError {
    VaernError::ServerStream {
        req_id,
        message: "the client closed the connection before the body was complete".to_string(),
        action: "Stop writing to this response; the remaining chunks cannot be delivered"
            .to_string(),
    }
}

/// Hands one response chunk to the connection, waiting up to `wait` for
/// buffer space.
///
/// Waiting gives the pump backpressure: a burst larger than the buffer is
/// delivered in full instead of dropping chunks. It runs on a Tokio task, and
/// the JS side awaits its outcome, so a slow client stalls only its own
/// response and never the JavaScript thread.
async fn deliver_stream_chunk(
    tx: &tokio::sync::mpsc::Sender<std::result::Result<bytes::Bytes, std::io::Error>>,
    req_id: u64,
    chunk: bytes::Bytes,
    wait: Duration,
) -> Result<()> {
    match tokio::time::timeout(wait, tx.send(Ok(chunk))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(stream_closed(req_id)),
        Err(_) => {
            // Abort the body rather than let the connection end cleanly, so the
            // client sees a failed transfer instead of a silently short one.
            let tx = tx.clone();
            tokio::spawn(async move {
                let aborted = std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "response stream stalled; body aborted",
                );
                let _ = tx.send(Err(aborted)).await;
            });
            Err(VaernError::ServerStream {
                req_id,
                message: format!(
                    "the client read nothing for {wait:?} while {STREAM_CHUNK_CAPACITY} chunks were buffered"
                ),
                action:
                    "Check that the client consumes the response body, or send fewer, larger chunks"
                        .to_string(),
            })
        }
    }
}

/// [`deliver_stream_chunk`] driven from a plain thread, for tests.
#[cfg(test)]
fn send_stream_chunk(
    handle: &tokio::runtime::Handle,
    tx: &tokio::sync::mpsc::Sender<std::result::Result<bytes::Bytes, std::io::Error>>,
    req_id: u64,
    chunk: bytes::Bytes,
    wait: Duration,
) -> Result<()> {
    handle.block_on(deliver_stream_chunk(tx, req_id, chunk, wait))
}

/// Native runtime implementation powered by QuickJS-ng.
pub struct QuickJsRuntime {
    options: RuntimeOptions,
    qjs_runtime: QjsRuntime,
    context: Context,
    state: Rc<RefCell<SharedState>>,
    transpiler: SwcTranspiler,
    events_rx: EventReceiver,
    /// Times the event loop parked waiting for work. Tests use it to prove an
    /// idle runtime does not wake on a fixed tick.
    parks: std::cell::Cell<u64>,
    /// Shared with the module resolver; see [`VaernModuleResolver::failure`].
    resolve_failure: Rc<RefCell<Option<VaernError>>>,
    active_servers_list: Arc<tokio::sync::Mutex<Vec<Arc<HttpServer>>>>,
    /// Where run journals are written; `None` disables the journal.
    journal_home: Option<PathBuf>,
    /// The journal home and the budget home, which `vaern:fs` refuses.
    reserved_homes: Rc<RefCell<[Option<PathBuf>; 2]>>,
    /// The open journal, shared with the bridges that record effects.
    journal: JournalSlot,
    /// The run's budget, shared with the bridges that reserve and settle.
    budget: BudgetSlot,
    replay: ReplaySlot,
    _tokio_rt: tokio::runtime::Runtime,
}

/// The run's journal, if one is open. Bridges append through it; it is
/// `None` before the run starts and when journalling is off.
type JournalSlot = Rc<RefCell<Option<Journal>>>;

/// The run's budget; see `budget::RunBudget`.
type BudgetSlot = Rc<RefCell<crate::budget::RunBudget>>;

/// The replay in progress, if this run is one. While it is set, effects are
/// served from the recorded journal and nothing leaves the process.
type ReplaySlot = Rc<RefCell<Option<crate::replay::Replay>>>;

impl QuickJsRuntime {
    /// Creates and initializes a new QuickJS runtime instance with host globals.
    pub fn new(options: RuntimeOptions) -> Result<Self> {
        let qjs_runtime = QjsRuntime::new().map_err(|err| VaernError::EngineInit {
            reason: format!("failed to initialize QuickJS runtime: {err}"),
        })?;
        qjs_runtime.set_memory_limit(HEAP_LIMIT_BYTES);

        let context = Context::full(&qjs_runtime).map_err(|err| VaernError::EngineInit {
            reason: format!("failed to initialize QuickJS context: {err}"),
        })?;

        let tokio_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to initialize Tokio runtime: {err}"),
            })?;
        let tokio_handle = tokio_rt.handle().clone();

        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let active_servers_list = Arc::new(tokio::sync::Mutex::new(Vec::<Arc<HttpServer>>::new()));

        let state = Rc::new(RefCell::new(SharedState {
            timers: HashMap::new(),
            abort_senders: HashMap::new(),
            pending_fetches: 0,
            next_eid: 0,
            pending_effects: HashMap::new(),
            pending_fs: 0,
            pending_models: 0,
            fetch_client: None,
            events_tx,
            active_servers: 0,
            server_handles: HashMap::new(),
            server_resp_senders: HashMap::new(),
            server_stream_senders: HashMap::new(),
            stream_sends_waiting: HashSet::new(),
            unhandled_error: None,
            pending_rejections: Vec::new(),
            turn: 0,
        }));
        let transpiler = SwcTranspiler;

        // Install SIGINT listener for graceful draining and shutdown
        let sig_servers = active_servers_list.clone();
        tokio_handle.spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let servers = {
                    let lock = sig_servers.lock().await;
                    lock.clone()
                };
                for server in servers {
                    server.close().await;
                }
                std::process::exit(0);
            }
        });

        let instance = Self {
            options,
            qjs_runtime,
            context,
            state,
            transpiler,
            events_rx,
            parks: std::cell::Cell::new(0),
            resolve_failure: Rc::new(RefCell::new(None)),
            active_servers_list,
            journal_home: None,
            reserved_homes: Rc::new(RefCell::new([None, None])),
            journal: Rc::new(RefCell::new(None)),
            budget: Rc::new(RefCell::new(crate::budget::RunBudget::default())),
            replay: Rc::new(RefCell::new(None)),
            _tokio_rt: tokio_rt,
        };

        instance.register_host_globals(tokio_handle)?;
        Ok(instance)
    }

    fn register_host_globals(&self, tokio_handle: tokio::runtime::Handle) -> Result<()> {
        let state = Rc::clone(&self.state);
        let argv = self.options.argv.clone();

        // A rejection still without a handler once microtasks drain means the
        // script failed; the event loop reports it and the CLI exits non-zero.
        // Handledness is tracked per promise so that handling one rejection
        // cannot clear another. Installed outside `Context::with` because that
        // call holds the runtime lock the setter needs.
        let state_rejection = Rc::clone(&state);
        self.qjs_runtime
            .set_host_promise_rejection_tracker(Some(Box::new(
                move |_ctx, promise, reason, is_handled| {
                    let key = promise_identity(&promise);
                    let mut s = state_rejection.borrow_mut();
                    if is_handled {
                        s.pending_rejections.retain(|(k, _)| *k != key);
                        return;
                    }
                    // A promise is rejected at most once, so a key that is
                    // already pending belongs to an earlier promise that was
                    // collected unhandled and whose address was reused. That
                    // rejection can never be handled now: make it fatal.
                    if let Some(pos) = s.pending_rejections.iter().position(|(k, _)| *k == key) {
                        let (_, earlier) = s.pending_rejections.remove(pos);
                        if s.unhandled_error.is_none() {
                            s.unhandled_error = Some(earlier);
                        }
                    }
                    s.pending_rejections
                        .push((key, describe_rejection(&reason)));
                },
            )));

        self.context.with(|ctx| -> Result<()> {
            let globals = ctx.globals();

            // Native console printer bridge
            // The text is journalled before it is written, so a transcript
            // never lacks output the terminal showed.
            let journal_print = Rc::clone(&self.journal);
            let replay_print = Rc::clone(&self.replay);
            let print_fn = Function::new(
                ctx.clone(),
                move |ctx: Ctx<'_>, target: i32, msg: String| -> rquickjs::Result<()> {
                    let fd = if target == 0 { 1 } else { 2 };
                    // A replay prints only what matches the transcript.
                    if let Some(replay) = replay_print.borrow_mut().as_mut()
                        && !replay.console(fd, &format!("{msg}\n"))
                    {
                        return Ok(());
                    }
                    if let Some(journal) = journal_print.borrow_mut().as_mut() {
                        journal
                            .console(fd, &format!("{msg}\n"))
                            .map_err(|err| rquickjs::Exception::throw_message(&ctx, &err.to_string()))?;
                    }
                    if target == 0 {
                        let mut out = std::io::stdout().lock();
                        let _ = writeln!(out, "{msg}");
                    } else {
                        let mut err = std::io::stderr().lock();
                        let _ = writeln!(err, "{msg}");
                    }
                    Ok(())
                },
            )
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create print bridge: {err}"),
            })?;
            globals
                .set("__vaern_print", print_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register print bridge: {err}"),
                })?;

            // Native exit bridge behind `process.exit`. It flushes both streams
            // first because `std::process::exit` skips destructors.
            let journal_exit = Rc::clone(&self.journal);
            let replay_exit = Rc::clone(&self.replay);
            let exit_fn = Function::new(ctx.clone(), move |code: i32| {
                if let Some(replay) = replay_exit.borrow_mut().as_mut() {
                    let outcome = replay.finish(Ok(code));
                    exit_process(crate::replay::report(&outcome));
                }
                // The process ends here, so this is the run's outcome.
                if let Some(journal) = journal_exit.borrow_mut().as_mut()
                    && let Err(err) = journal.seal(code, None)
                {
                    eprintln!("{err}");
                    exit_process(1);
                }
                exit_process(code)
            })
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create exit bridge: {err}"),
            })?;
            globals
                .set("__vaern_exit", exit_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register exit bridge: {err}"),
                })?;

            // Exceptions thrown by host-invoked callbacks (timers) have no JS
            // caller to propagate to. Recording them here makes the event loop
            // stop with that error instead of exiting 0.
            let state_uncaught = Rc::clone(&state);
            let report_uncaught_fn = Function::new(ctx.clone(), move |err: Value<'_>| {
                let mut s = state_uncaught.borrow_mut();
                if s.unhandled_error.is_none() {
                    s.unhandled_error = Some(describe_rejection(&err));
                }
            })
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create uncaught error bridge: {err}"),
            })?;
            globals
                .set("__vaern_report_uncaught", report_uncaught_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register uncaught error bridge: {err}"),
                })?;

            // Native timer register bridge
            let state_reg = Rc::clone(&state);
            let timer_reg_fn = Function::new(
                ctx.clone(),
                move |id: u32, delay_ms: f64, is_interval: bool| {
                    let delay = if delay_ms < 0.0 {
                        Duration::from_millis(0)
                    } else {
                        Duration::from_millis(delay_ms as u64)
                    };
                    let deadline = Instant::now() + delay;
                    let interval = if is_interval { Some(delay) } else { None };
                    state_reg
                        .borrow_mut()
                        .timers
                        .insert(id, TimerRecord { deadline, interval });
                },
            )
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create timer register bridge: {err}"),
            })?;
            globals
                .set("__vaern_timer_register", timer_reg_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register timer register bridge: {err}"),
                })?;

            // Native timer clear bridge
            let state_clr = Rc::clone(&state);
            let timer_clr_fn = Function::new(ctx.clone(), move |id: u32| {
                state_clr.borrow_mut().timers.remove(&id);
            })
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create timer clear bridge: {err}"),
            })?;
            globals
                .set("__vaern_timer_clear", timer_clr_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register timer clear bridge: {err}"),
                })?;

            // Native fetch start bridge
            let state_fetch = Rc::clone(&state);
            let journal_fetch = Rc::clone(&self.journal);
            let replay_fetch = Rc::clone(&self.replay);
            let tokio_handle_fetch = tokio_handle.clone();
            let fetch_start_fn = Function::new(
                ctx.clone(),
                move |req_id: u32,
                      url: String,
                      method: String,
                      headers_val: Value<'_>,
                      body_val: Value<'_>,
                      timeout_ms: f64,
                      abort_id: u32|
                      -> rquickjs::Result<()> {
                    let mut headers = Vec::new();
                    if let Some(arr) = headers_val.as_array() {
                        for i in 0..arr.len() {
                            let Ok(pair_val) = arr.get::<Value>(i) else {
                                continue;
                            };
                            let Some(pair) = pair_val.as_array() else {
                                continue;
                            };
                            if pair.len() >= 2
                                && let (Ok(k), Ok(v)) =
                                    (pair.get::<String>(0), pair.get::<String>(1))
                            {
                                headers.push((k, v));
                            }
                        }
                    }

                    let body: Option<Vec<u8>> = if let Some(s) = body_val.as_string() {
                        s.to_string().ok().map(|str_val| str_val.into_bytes())
                    } else if let Some(arr) = body_val.as_array() {
                        let mut bytes = Vec::with_capacity(arr.len());
                        for i in 0..arr.len() {
                            if let Ok(num) = arr.get::<u32>(i) {
                                bytes.push(num as u8);
                            }
                        }
                        Some(bytes)
                    } else {
                        None
                    };

                    let timeout = if timeout_ms > 0.0 {
                        Some(Duration::from_millis(timeout_ms as u64))
                    } else {
                        None
                    };

                    if let Some(replay) = replay_fetch.borrow_mut().as_mut() {
                        let req = crate::effect_record::fetch_request(
                            &method,
                            &url,
                            &headers,
                            body.as_deref(),
                        );
                        replay.dispatch("fetch", req_id, &req, None);
                        state_fetch.borrow_mut().pending_fetches += 1;
                        return Ok(());
                    }
                    if journal_fetch.borrow().is_some() {
                        let req = crate::effect_record::fetch_request(
                            &method,
                            &url,
                            &headers,
                            body.as_deref(),
                        );
                        track_effect(&state_fetch, "fetch", req_id, req);
                    }

                    let options = crate::fetch::FetchOptions {
                        method,
                        headers,
                        body,
                        timeout,
                        max_body_bytes: crate::fetch::MAX_RESPONSE_BODY_BYTES,
                    };

                    let abort_rx = if abort_id > 0 {
                        let mut s = state_fetch.borrow_mut();
                        let sender = s.abort_senders.entry(abort_id).or_insert_with(|| {
                            let (tx, _) = tokio::sync::broadcast::channel(1);
                            tx
                        });
                        Some(sender.subscribe())
                    } else {
                        None
                    };

                    let tx = state_fetch.borrow().events_tx.clone();
                    let client = shared_client(&state_fetch);
                    state_fetch.borrow_mut().pending_fetches += 1;

                    match client {
                        Ok(client) => {
                            tokio_handle_fetch.spawn(async move {
                                let result = crate::fetch::execute_fetch_pooled(
                                    &client, &url, options, abort_rx,
                                )
                                .await;
                                let _ = tx.send(LoopEvent::FetchDone(FetchCompletion { req_id, result }));
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(LoopEvent::FetchDone(FetchCompletion {
                                req_id,
                                result: Err(err),
                            }));
                        }
                    }

                    Ok(())
                },
            )
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create fetch start bridge: {err}"),
            })?;
            globals
                .set("__vaern_fetch_start", fetch_start_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register fetch start bridge: {err}"),
                })?;
            globals
                .set(
                    "__vaern_runtime_headers",
                    crate::fetch::RUNTIME_MANAGED_HEADERS.to_vec(),
                )
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register the runtime-managed header list: {err}"),
                })?;

            // Native vaern:fs bridge. `payload` carries write data: UTF-8 text
            // when `kind` is "text", or one Latin-1 code unit per byte when it
            // is "bytes", since typed-array memory is reachable only through
            // `unsafe` accessors.
            let state_fs = Rc::clone(&state);
            let journal_fs = Rc::clone(&self.journal);
            let replay_fs = Rc::clone(&self.replay);
            let reserved_fs = Rc::clone(&self.reserved_homes);
            let tokio_handle_fs = tokio_handle.clone();
            let fs_start_fn = Function::new(
                ctx.clone(),
                move |ctx: Ctx<'_>,
                      req_id: u32,
                      op: String,
                      path: String,
                      kind: String,
                      payload: String|
                      -> rquickjs::Result<()> {
                    let data = if kind == "bytes" {
                        crate::webapi::latin1_to_bytes(&ctx, &payload, "vaern:fs")?
                    } else {
                        payload.into_bytes()
                    };
                    let mode = match op.as_str() {
                        "write" => Some(crate::fs::WriteMode::Truncate),
                        "write_new" => Some(crate::fs::WriteMode::CreateNew),
                        "append" => Some(crate::fs::WriteMode::Append),
                        _ => None,
                    };
                    let op = match (op.as_str(), mode) {
                        ("read", _) => crate::fs::FsOp::Read { path, utf8: false },
                        ("read_utf8", _) => crate::fs::FsOp::Read { path, utf8: true },
                        (_, Some(mode)) => crate::fs::FsOp::Write { path, data, mode },
                        (other, None) => {
                            return Err(rquickjs::Exception::throw_type(
                                &ctx,
                                &format!("vaern:fs: unknown operation '{other}'"),
                            ));
                        }
                    };

                    if let Some(replay) = replay_fs.borrow_mut().as_mut() {
                        let req = crate::effect_record::fs_request(&op);
                        replay.dispatch("fs", req_id, &req, None);
                        state_fs.borrow_mut().pending_fs += 1;
                        return Ok(());
                    }
                    if journal_fs.borrow().is_some() {
                        track_effect(&state_fs, "fs", req_id, crate::effect_record::fs_request(&op));
                    }

                    let tx = state_fs.borrow().events_tx.clone();
                    state_fs.borrow_mut().pending_fs += 1;
                    let reserved: Vec<PathBuf> =
                        reserved_fs.borrow().iter().flatten().cloned().collect();
                    tokio_handle_fs.spawn_blocking(move || {
                        let result = crate::fs::run_outside(op, &reserved);
                        let _ = tx.send(LoopEvent::FsDone(FsCompletion { req_id, result }));
                    });
                    Ok(())
                },
            )
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create fs bridge: {err}"),
            })?;
            globals
                .set("__vaern_fs_start", fs_start_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register fs bridge: {err}"),
                })?;

            // Native vaern:ai bridge. Returns null once the call is dispatched,
            // or the JSON of a ModelError when the options are refused, so
            // nothing is sent and nothing is journalled for a bad call.
            let state_ai = Rc::clone(&state);
            let journal_ai = Rc::clone(&self.journal);
            let budget_ai = Rc::clone(&self.budget);
            let replay_ai = Rc::clone(&self.replay);
            let tokio_handle_ai = tokio_handle.clone();
            let ai_start_fn = Function::new(
                ctx.clone(),
                move |req_id: u32, config: String, call: String| -> Option<String> {
                    let call = match crate::ai::prepare(&config, &call) {
                        Ok(call) => call,
                        Err(err) => return Some(model_error_info(err).to_string()),
                    };
                    // A replay builds no client: it never connects.
                    let client = match replay_ai.borrow().is_some() {
                        true => None,
                        false => match shared_client(&state_ai) {
                            Ok(client) => Some(client),
                            Err(err) => return Some(model_error_info(err).to_string()),
                        },
                    };
                    // The reserve is on disk before anything is sent; a call
                    // that does not fit never opens a connection.
                    let reserve = crate::budget::Amount {
                        nano: crate::ai::reserve_nano(&call),
                        tokens: crate::ai::estimate_input_tokens(&call)
                            .saturating_add(call.max_tokens),
                    };
                    let reserved = budget_ai.borrow_mut().reserve(
                        journal_ai.borrow_mut().as_mut(),
                        "model.call",
                        req_id,
                        reserve,
                    );
                    if let Err(err) = reserved {
                        return Some(model_error_info(err).to_string());
                    }
                    let Some(client) = client else {
                        let req = crate::ai::request_record(&call);
                        let eid = replay_ai.borrow_mut().as_mut().map(|replay| {
                            replay.dispatch("model.call", req_id, &req, Some(call))
                        });
                        let mut s = state_ai.borrow_mut();
                        if let Some(eid) = eid {
                            let started = Instant::now();
                            let pending = PendingEffect { eid, started, req };
                            s.pending_effects.insert(("model.call", req_id), pending);
                        }
                        s.pending_models += 1;
                        return None;
                    };
                    if journal_ai.borrow().is_some() {
                        let req = crate::ai::request_record(&call);
                        track_effect(&state_ai, "model.call", req_id, req);
                    }
                    let tx = state_ai.borrow().events_tx.clone();
                    state_ai.borrow_mut().pending_models += 1;
                    tokio_handle_ai.spawn(async move {
                        let result = if call.stream {
                            let deltas = tx.clone();
                            crate::ai_stream::send(&client, &call, move |delta| {
                                let _ = deltas.send(LoopEvent::ModelDelta(req_id, delta));
                            })
                            .await
                        } else {
                            crate::ai::send(&client, &call).await
                        };
                        let _ = tx.send(LoopEvent::ModelDone(Box::new(ModelCompletion {
                            req_id,
                            call,
                            result,
                        })));
                    });
                    None
                },
            )
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create model call bridge: {err}"),
            })?;
            globals
                .set("__vaern_ai_start", ai_start_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register model call bridge: {err}"),
                })?;

            // Tool effects. A tool's run() is script code, so the prelude
            // reports when it starts and ends. The start is recorded at once,
            // so whatever the tool does follows its tool.call in the journal.
            // Both bridges return the JSON of a journal failure, or null.
            let state_tool = Rc::clone(&state);
            let journal_tool = Rc::clone(&self.journal);
            let budget_tool = Rc::clone(&self.budget);
            let replay_tool = Rc::clone(&self.replay);
            let tool_start_fn = Function::new(
                ctx.clone(),
                move |req_id: u32, name: String, args: String, cost_nano: f64| -> String {
                    if cost_nano.is_finite() && cost_nano >= 1.0 {
                        let reserve = crate::budget::Amount {
                            nano: cost_nano as u64,
                            tokens: 0,
                        };
                        let reserved = budget_tool.borrow_mut().reserve(
                            journal_tool.borrow_mut().as_mut(),
                            "tool.call",
                            req_id,
                            reserve,
                        );
                        if let Err(err) = reserved {
                            return serde_json::json!({ "budget": model_error_info(err) })
                                .to_string();
                        }
                    }
                    let args = serde_json::from_str(&args).unwrap_or(serde_json::Value::Null);
                    if let Some(replay) = replay_tool.borrow_mut().as_mut() {
                        let req = serde_json::json!({ "name": name, "args": args });
                        let eid = replay.dispatch("tool.call", req_id, &req, None);
                        let started = Instant::now();
                        let req = serde_json::json!({ "name": name });
                        let pending = PendingEffect { eid, started, req };
                        let mut s = state_tool.borrow_mut();
                        s.pending_effects.insert(("tool.result", req_id), pending);
                        return serde_json::json!({ "eid": eid }).to_string();
                    }
                    let mut slot = journal_tool.borrow_mut();
                    let Some(journal) = slot.as_mut() else {
                        return serde_json::json!({ "eid": null }).to_string();
                    };
                    let eid = state_tool.borrow().next_eid;
                    let mut fields = serde_json::Map::new();
                    fields.insert("eid".into(), eid.into());
                    fields.insert(
                        "req".into(),
                        serde_json::json!({ "name": name, "args": args }),
                    );
                    if let Err(err) = journal.append("tool.call", fields) {
                        return serde_json::json!({ "error": err.to_string() }).to_string();
                    }
                    let mut s = state_tool.borrow_mut();
                    s.next_eid += 1;
                    s.pending_effects.insert(
                        ("tool.result", req_id),
                        PendingEffect {
                            eid,
                            started: Instant::now(),
                            req: serde_json::json!({ "name": name }),
                        },
                    );
                    serde_json::json!({ "eid": eid }).to_string()
                },
            )
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create tool start bridge: {err}"),
            })?;
            let state_tool = Rc::clone(&state);
            let journal_tool = Rc::clone(&self.journal);
            let budget_tool = Rc::clone(&self.budget);
            let tool_finish_fn = Function::new(
                ctx.clone(),
                move |req_id: u32, ok: bool, outcome: String| -> Option<String> {
                    // A tool reports no usage: its declared cost stays charged.
                    let settled = budget_tool.borrow_mut().settle(
                        journal_tool.borrow_mut().as_mut(),
                        "tool.call",
                        req_id,
                        None,
                    );
                    if let Err(err) = settled {
                        return Some(serde_json::json!({ "error": err.to_string() }).to_string());
                    }
                    let pending = state_tool
                        .borrow_mut()
                        .pending_effects
                        .remove(&("tool.result", req_id))?;
                    let value = serde_json::from_str(&outcome).unwrap_or(serde_json::Value::Null);
                    let outcome = if ok {
                        Outcome::Response(serde_json::json!({ "value": value }))
                    } else {
                        Outcome::Error(value)
                    };
                    let mut slot = journal_tool.borrow_mut();
                    let journal = slot.as_mut()?;
                    journal
                        .append("tool.result", pending.finish(outcome))
                        .err()
                        .map(|err| serde_json::json!({ "error": err.to_string() }).to_string())
                },
            )
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create tool finish bridge: {err}"),
            })?;
            // Tool arguments are checked with the same schema subset as
            // structured replies. Each returns {path, message} JSON, or null.
            let schema_check_fn = Function::new(ctx.clone(), |schema: String| -> Option<String> {
                let schema = serde_json::from_str(&schema).unwrap_or(serde_json::Value::Null);
                crate::ai_schema::check_schema(&schema)
                    .err()
                    .map(|e| serde_json::json!({ "path": e.path, "message": e.message }).to_string())
            })
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create schema check bridge: {err}"),
            })?;
            let schema_validate_fn = Function::new(
                ctx.clone(),
                |value: String, schema: String| -> Option<String> {
                    let value = serde_json::from_str(&value).unwrap_or(serde_json::Value::Null);
                    let schema = serde_json::from_str(&schema).unwrap_or(serde_json::Value::Null);
                    crate::ai_schema::validate(&value, &schema).err().map(|e| {
                        serde_json::json!({ "path": e.path, "message": e.message }).to_string()
                    })
                },
            )
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create schema validate bridge: {err}"),
            })?;
            let budget_set = Rc::clone(&self.budget);
            let journal_budget = Rc::clone(&self.journal);
            let budget_set_fn = Function::new(ctx.clone(), move |request: String| -> String {
                let request: serde_json::Value =
                    serde_json::from_str(&request).unwrap_or(serde_json::Value::Null);
                let limits = crate::budget::Limits {
                    nano: request.get("nano").and_then(serde_json::Value::as_u64),
                    tokens: request.get("tokens").and_then(serde_json::Value::as_u64),
                };
                let name = request.get("name").and_then(serde_json::Value::as_str);
                let opened =
                    budget_set
                        .borrow_mut()
                        .request(journal_budget.borrow_mut().as_mut(), name, limits);
                match opened {
                    Ok(left) => serde_json::json!({
                        "remaining": { "nano": left.nano, "tokens": left.tokens },
                    }),
                    Err(err) => serde_json::json!({ "error": model_error_info(err) }),
                }
                .to_string()
            })
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create budget bridge: {err}"),
            })?;
            let budget_left = Rc::clone(&self.budget);
            let budget_left_fn = Function::new(ctx.clone(), move || -> String {
                let left = budget_left.borrow().remaining();
                serde_json::json!({ "nano": left.nano, "tokens": left.tokens }).to_string()
            })
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create budget remaining bridge: {err}"),
            })?;
            for (name, bridge) in [
                ("__vaern_budget_set", budget_set_fn),
                ("__vaern_budget_remaining", budget_left_fn),
                ("__vaern_tool_start", tool_start_fn),
                ("__vaern_tool_finish", tool_finish_fn),
                ("__vaern_schema_check", schema_check_fn),
                ("__vaern_schema_validate", schema_validate_fn),
            ] {
                globals
                    .set(name, bridge)
                    .map_err(|err| VaernError::EngineInit {
                        reason: format!("failed to register {name}: {err}"),
                    })?;
            }

            // Native abort signal bridge
            let state_abort = Rc::clone(&state);
            let abort_fn = Function::new(ctx.clone(), move |abort_id: u32| {
                if let Some(sender) = state_abort.borrow().abort_senders.get(&abort_id) {
                    let _ = sender.send(());
                }
            })
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create abort bridge: {err}"),
            })?;
            globals
                .set("__vaern_abort_signal", abort_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register abort bridge: {err}"),
                })?;

            // Native server start bridge
            let state_srv = Rc::clone(&state);
            let tokio_handle_srv = tokio_handle.clone();
            let active_srv_list = self.active_servers_list.clone();
            let replay_srv = Rc::clone(&self.replay);
            let server_start_fn = Function::new(
                ctx.clone(),
                move |server_id: u32, hostname: String, port: u16| -> rquickjs::Result<String> {
                    if let Some(replay) = replay_srv.borrow_mut().as_mut() {
                        let error = replay.refuse("serve()");
                        return Ok(serde_json::json!({ "ok": false, "error": error }).to_string());
                    }
                    let bind_res = tokio_handle_srv.block_on(async {
                        crate::server::bind_listener(&hostname, port).await
                    });

                    let listener = match bind_res {
                        Ok(l) => l,
                        Err(err) => {
                            let escaped = format!("{err}")
                                .replace('\\', "\\\\")
                                .replace('"', "\\\"")
                                .replace('\n', "\\n");
                            return Ok(format!(r#"{{"ok":false,"error":"{escaped}"}}"#));
                        }
                    };

                    let local_addr = match listener.local_addr() {
                        Ok(a) => a,
                        Err(err) => {
                            let escaped = format!("failed to get local address: {err}")
                                .replace('\\', "\\\\")
                                .replace('"', "\\\"")
                                .replace('\n', "\\n");
                            return Ok(format!(r#"{{"ok":false,"error":"{escaped}"}}"#));
                        }
                    };
                    let bound_port = local_addr.port();
                    let bound_host = hostname.clone();

                    let req_tx = state_srv.borrow().events_tx.clone();
                    let server = Arc::new(HttpServer::start(
                        server_id,
                        bound_host.clone(),
                        bound_port,
                        listener,
                        req_tx,
                        tokio_handle_srv.clone(),
                        crate::server::ServerLimits::default(),
                    ));

                    state_srv.borrow_mut().active_servers += 1;
                    state_srv.borrow_mut().server_handles.insert(server_id, server.clone());

                    let srv_clone = server.clone();
                    let list_clone = active_srv_list.clone();
                    tokio_handle_srv.spawn(async move {
                        list_clone.lock().await.push(srv_clone);
                    });

                    Ok(format!(
                        r#"{{"ok":true,"hostname":"{bound_host}","port":{bound_port}}}"#
                    ))
                },
            )
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create server start bridge: {err}"),
            })?;
            globals
                .set("__vaern_server_start", server_start_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register server start bridge: {err}"),
                })?;

            // Native server close bridge
            let state_close = Rc::clone(&state);
            let tokio_handle_close = tokio_handle.clone();
            let active_srv_close = self.active_servers_list.clone();
            let server_close_fn = Function::new(
                ctx.clone(),
                move |server_id: u32| -> rquickjs::Result<()> {
                    let (server_opt, tx) = {
                        let mut s = state_close.borrow_mut();
                        let srv = s.server_handles.remove(&server_id);
                        (srv, s.events_tx.clone())
                    };

                    if let Some(server) = server_opt {
                        let list_clone = active_srv_close.clone();
                        tokio_handle_close.spawn(async move {
                            server.close().await;
                            {
                                let mut lock = list_clone.lock().await;
                                lock.retain(|s| s.server_id != server_id);
                            }
                            let _ = tx.send(LoopEvent::ServerClosed(server_id));
                        });
                    } else {
                        let _ = tx.send(LoopEvent::ServerClosed(server_id));
                    }
                    Ok(())
                },
            )
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create server close bridge: {err}"),
            })?;
            globals
                .set("__vaern_server_close", server_close_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register server close bridge: {err}"),
                })?;

            // Native server respond bridge
            let state_resp = Rc::clone(&state);
            let server_respond_fn = Function::new(
                ctx.clone(),
                move |req_id: f64,
                      status: u16,
                      status_text: String,
                      headers_val: Value<'_>,
                      body_val: Value<'_>|
                      -> rquickjs::Result<()> {
                    let req_id_u64 = req_id as u64;
                    let mut headers = Vec::new();
                    if let Some(arr) = headers_val.as_array() {
                        for i in 0..arr.len() {
                            if let Ok(pair_val) = arr.get::<Value>(i)
                                && let Some(pair) = pair_val.as_array()
                                && pair.len() >= 2
                                && let (Ok(k), Ok(v)) = (pair.get::<String>(0), pair.get::<String>(1))
                            {
                                headers.push((k, v));
                            }
                        }
                    }

                    let body_bytes: Vec<u8> = if let Some(s) = body_val.as_string() {
                        match s.to_string() {
                            Ok(v) => v.into_bytes(),
                            Err(_) => Vec::new(),
                        }
                    } else if let Some(arr) = body_val.as_array() {
                        let mut b = Vec::with_capacity(arr.len());
                        for i in 0..arr.len() {
                            if let Ok(num) = arr.get::<u32>(i) {
                                b.push(num as u8);
                            }
                        }
                        b
                    } else {
                        Vec::new()
                    };

                    let response = ServerResponse {
                        status,
                        status_text,
                        headers,
                        body: ServerResponseBody::Full(body_bytes),
                    };

                    if let Some(sender) = state_resp.borrow_mut().server_resp_senders.remove(&req_id_u64) {
                        let _ = sender.send(response);
                    }

                    Ok(())
                },
            )
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create server respond bridge: {err}"),
            })?;
            globals
                .set("__vaern_server_respond", server_respond_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register server respond bridge: {err}"),
                })?;

            // Native server stream start bridge
            let state_stream_start = Rc::clone(&state);
            let server_stream_start_fn = Function::new(
                ctx.clone(),
                move |req_id: f64, status: u16, status_text: String, headers_val: Value<'_>| -> rquickjs::Result<()> {
                    let req_id_u64 = req_id as u64;
                    let mut headers = Vec::new();
                    if let Some(arr) = headers_val.as_array() {
                        for i in 0..arr.len() {
                            if let Ok(pair_val) = arr.get::<Value>(i)
                                && let Some(pair) = pair_val.as_array()
                                && pair.len() >= 2
                                && let (Ok(k), Ok(v)) = (pair.get::<String>(0), pair.get::<String>(1))
                            {
                                headers.push((k, v));
                            }
                        }
                    }

                    let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel(STREAM_CHUNK_CAPACITY);
                    state_stream_start.borrow_mut().server_stream_senders.insert(req_id_u64, chunk_tx);

                    let response = ServerResponse {
                        status,
                        status_text,
                        headers,
                        body: ServerResponseBody::Stream(chunk_rx),
                    };

                    if let Some(sender) = state_stream_start.borrow_mut().server_resp_senders.remove(&req_id_u64) {
                        let _ = sender.send(response);
                    }

                    Ok(())
                },
            )
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create server stream start bridge: {err}"),
            })?;
            globals
                .set("__vaern_server_respond_stream_start", server_stream_start_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register server stream start bridge: {err}"),
                })?;

            // Native server stream chunk bridge
            let state_stream_chunk = Rc::clone(&state);
            let tokio_handle_chunk = tokio_handle.clone();
            let server_stream_chunk_fn = Function::new(
                ctx.clone(),
                // The chunk arrives as a byte string (see webapi::latin1_to_bytes).
                // It used to be a JS number array read back one element at a
                // time, about 9 ms per 64 KiB in a debug build, all of it on
                // the JavaScript thread while timers waited.
                move |ctx: Ctx<'_>, req_id: f64, chunk: String, is_done: bool| -> rquickjs::Result<bool> {
                    let req_id_u64 = req_id as u64;
                    let chunk_bytes = crate::webapi::latin1_to_bytes(&ctx, &chunk, "response stream")?;

                    // Returns true when the chunk is queued. False means it is
                    // waiting for buffer space on a Tokio task, and JS must await
                    // __vaern_dispatch_stream_sent before writing the next one.
                    let sender = state_stream_chunk.borrow().server_stream_senders.get(&req_id_u64).cloned();
                    let mut queued = true;
                    if !chunk_bytes.is_empty()
                        && let Some(tx) = sender
                    {
                        if state_stream_chunk.borrow().stream_sends_waiting.contains(&req_id_u64) {
                            return Err(rquickjs::Exception::throw_message(
                                &ctx,
                                "response stream: a chunk was written while the previous one was still waiting for the client; await each write",
                            ));
                        }
                        let chunk = bytes::Bytes::from(chunk_bytes);
                        let waiting = match tx.try_reserve() {
                            Ok(permit) => {
                                permit.send(Ok(chunk));
                                None
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(())) => {
                                state_stream_chunk.borrow_mut().server_stream_senders.remove(&req_id_u64);
                                let err = stream_closed(req_id_u64);
                                return Err(rquickjs::Exception::throw_message(&ctx, &err.to_string()));
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Full(())) => Some(chunk),
                        };
                        if let Some(chunk) = waiting {
                            let events = {
                                let mut s = state_stream_chunk.borrow_mut();
                                s.stream_sends_waiting.insert(req_id_u64);
                                s.events_tx.clone()
                            };
                            tokio_handle_chunk.spawn(async move {
                                let result = deliver_stream_chunk(&tx, req_id_u64, chunk, STREAM_CHUNK_SEND_TIMEOUT).await;
                                let _ = events.send(LoopEvent::StreamChunkSent(StreamChunkSent { req_id: req_id_u64, result }));
                            });
                            queued = false;
                        }
                    }

                    if is_done {
                        state_stream_chunk.borrow_mut().server_stream_senders.remove(&req_id_u64);
                    }

                    Ok(queued)
                },
            )
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to create server stream chunk bridge: {err}"),
            })?;
            globals
                .set("__vaern_server_respond_stream_chunk", server_stream_chunk_fn)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to register server stream chunk bridge: {err}"),
                })?;

            crate::webapi::register(&ctx, &globals)?;

            // Evaluate standard JavaScript bootstrap environment
            let bootstrap_js = r#"
                const __timers = new Map();
                let __nextTimerId = 1;

                globalThis.setTimeout = (cb, delay = 0, ...args) => {
                    const id = __nextTimerId++;
                    __timers.set(id, { cb, args, interval: false });
                    __vaern_timer_register(id, Number(delay) || 0, false);
                    return id;
                };

                globalThis.setInterval = (cb, delay = 0, ...args) => {
                    const id = __nextTimerId++;
                    __timers.set(id, { cb, args, interval: true });
                    __vaern_timer_register(id, Number(delay) || 0, true);
                    return id;
                };

                globalThis.clearTimeout = (id) => {
                    __timers.delete(id);
                    __vaern_timer_clear(id);
                };

                globalThis.clearInterval = (id) => {
                    __timers.delete(id);
                    __vaern_timer_clear(id);
                };

                globalThis.__vaern_trigger_timer = (id) => {
                    const t = __timers.get(id);
                    if (!t) return;
                    if (!t.interval) {
                        __timers.delete(id);
                    }
                    try {
                        t.cb(...t.args);
                    } catch (e) {
                        __vaern_report_uncaught(e);
                    }
                };

                globalThis.queueMicrotask = (cb) => {
                    Promise.resolve().then(cb);
                };

                // Resolves after one pass of the event loop, through the timer
                // captured here so a script that replaces setTimeout does not
                // change it.
                const __setTimeoutOwn = globalThis.setTimeout;
                const __yieldToEventLoop = () => new Promise((resolve) => __setTimeoutOwn(resolve, 0));

                // QuickJS keeps only the frames in `stack`, so the name and
                // message line is added here, as other runtimes print it.
                function __formatError(err, depth) {
                    const head = err.message ? `${err.name || 'Error'}: ${err.message}` : String(err.name || 'Error');
                    const frames = typeof err.stack === 'string' ? err.stack.replace(/\n+$/, '') : '';
                    let out = frames ? (frames.startsWith(head) ? frames : `${head}\n${frames}`) : head;
                    if (err.cause !== undefined && depth < 8) {
                        const cause = err.cause instanceof Error ? __formatError(err.cause, depth + 1) : __formatArg(err.cause);
                        out += `\nCaused by: ${cause}`;
                    }
                    return out;
                }

                function __jsonReplacer(_key, value) {
                    if (value instanceof Error) return `${value.name}: ${value.message}`;
                    if (typeof value === 'bigint') return `${value}n`;
                    return value;
                }

                function __formatArg(arg) {
                    if (typeof arg === 'string') return arg;
                    if (typeof arg === 'undefined') return 'undefined';
                    if (arg === null) return 'null';
                    if (typeof arg === 'symbol') return arg.toString();
                    if (typeof arg === 'bigint') return arg.toString() + 'n';
                    if (arg instanceof Error) return __formatError(arg, 0);
                    if (typeof arg === 'object') {
                        try {
                            return JSON.stringify(arg, __jsonReplacer);
                        } catch (_) {
                            return String(arg);
                        }
                    }
                    return String(arg);
                }

                globalThis.console = {
                    log: (...args) => __vaern_print(0, args.map(__formatArg).join(' ')),
                    info: (...args) => __vaern_print(0, args.map(__formatArg).join(' ')),
                    warn: (...args) => __vaern_print(1, args.map(__formatArg).join(' ')),
                    error: (...args) => __vaern_print(1, args.map(__formatArg).join(' ')),
                };

                globalThis.process = {
                    exitCode: 0,
                    argv: [],
                    env: {},
                    exit(code) {
                        const c = code === undefined ? process.exitCode : code;
                        __vaern_exit((Number(c) || 0) | 0);
                    },
                };

                // WHATWG TextEncoder & TextDecoder (UTF-8 only)
                function __toBytes(input) {
                    if (input === undefined || input === null) return new Uint8Array(0);
                    if (input instanceof Uint8Array) return input;
                    if (ArrayBuffer.isView(input)) return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
                    if (input instanceof ArrayBuffer) return new Uint8Array(input);
                    throw new TypeError("TextDecoder.decode: expected an ArrayBuffer or a view of one");
                }

                function __latin1(bytes) {
                    let out = "";
                    for (let i = 0; i < bytes.length; i += 8192) {
                        out += String.fromCharCode.apply(null, bytes.subarray(i, i + 8192));
                    }
                    return out;
                }

                class TextEncoder {
                    get encoding() { return "utf-8"; }
                    encode(str = "") {
                        return __vaern_utf8_encode(String(str).toWellFormed());
                    }
                }
                globalThis.TextEncoder = TextEncoder;

                // WebIDL dictionary conversion: undefined and null mean "all
                // defaults", any other non-object is a TypeError.
                function __options(where, value) {
                    if (value === undefined || value === null) return {};
                    if (typeof value === 'object' || typeof value === 'function') return value;
                    throw new TypeError(`${where}: options must be an object, got ${typeof value}`);
                }

                class TextDecoder {
                    constructor(label = "utf-8", options) {
                        options = __options("TextDecoder", options);
                        const name = String(label).trim().toLowerCase();
                        if (name !== "utf-8" && name !== "utf8" && name !== "unicode-1-1-utf-8") {
                            throw new RangeError(`TextDecoder: unsupported encoding '${label}'; only utf-8 is available`);
                        }
                        this._fatal = Boolean(options.fatal);
                        this._ignoreBOM = Boolean(options.ignoreBOM);
                        this._pending = new Uint8Array(0);
                        this._started = false;
                    }
                    get encoding() { return "utf-8"; }
                    get fatal() { return this._fatal; }
                    get ignoreBOM() { return this._ignoreBOM; }

                    decode(input, options) {
                        options = __options("TextDecoder.decode", options);
                        // Only an omitted input means "no bytes"; null is not a buffer.
                        if (input === null) {
                            throw new TypeError("TextDecoder.decode: expected an ArrayBuffer or a view of one, got null");
                        }
                        const incoming = __toBytes(input);
                        let bytes = incoming;
                        if (this._pending.length) {
                            bytes = new Uint8Array(this._pending.length + incoming.length);
                            bytes.set(this._pending);
                            bytes.set(incoming, this._pending.length);
                        }
                        let end = bytes.length;
                        if (options.stream) {
                            // Hold back a sequence the next chunk may complete.
                            for (let back = 1; back <= 3 && back <= bytes.length; back++) {
                                const b = bytes[bytes.length - back];
                                if ((b & 0xc0) === 0x80) continue;
                                const need = b >= 0xf0 ? 4 : b >= 0xe0 ? 3 : b >= 0xc0 ? 2 : 1;
                                if (need > back) end = bytes.length - back;
                                break;
                            }
                        }
                        this._pending = options.stream ? bytes.slice(end) : new Uint8Array(0);
                        let head = bytes.subarray(0, end);
                        if (!this._started && head.length) {
                            this._started = true;
                            if (!this._ignoreBOM && head[0] === 0xef && head[1] === 0xbb && head[2] === 0xbf) {
                                head = head.subarray(3);
                            }
                        }
                        const text = __vaern_utf8_decode(__latin1(head), this._fatal);
                        if (!options.stream) this._started = false;
                        return text;
                    }
                }
                globalThis.TextDecoder = TextDecoder;

                // Reads a body stream to the end as one byte array, so text
                // decoding never sees a code point split across chunks.
                async function __readAllBytes(stream) {
                    const reader = stream.getReader();
                    const parts = [];
                    let total = 0;
                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) break;
                        const part = typeof value === 'string' ? __vaern_utf8_encode(value.toWellFormed()) : __toBytes(value);
                        parts.push(part);
                        total += part.length;
                    }
                    const out = new Uint8Array(total);
                    let offset = 0;
                    for (const part of parts) {
                        out.set(part, offset);
                        offset += part.length;
                    }
                    return out;
                }

                // WHATWG URL & URLSearchParams, parsed by the `url` crate
                class URLSearchParams {
                    constructor(init = "") {
                        this._list = [];
                        this._url = null;
                        if (init instanceof URLSearchParams) {
                            this._list = init._list.map(([k, v]) => [k, v]);
                        } else if (init !== null && typeof init === 'object') {
                            if (typeof init[Symbol.iterator] === 'function') {
                                for (const pair of init) {
                                    const entry = Array.from(pair);
                                    if (entry.length !== 2) {
                                        throw new TypeError("URLSearchParams: each pair needs exactly a name and a value");
                                    }
                                    this._list.push([String(entry[0]), String(entry[1])]);
                                }
                            } else {
                                for (const key of Object.keys(init)) this._list.push([key, String(init[key])]);
                            }
                        } else {
                            const str = String(init);
                            this._list = __vaern_form_parse(str.startsWith('?') ? str.slice(1) : str);
                        }
                    }
                    _reset(search) {
                        this._list = __vaern_form_parse(search.startsWith('?') ? search.slice(1) : search);
                    }
                    _update() {
                        if (this._url) this._url._apply('search', this.toString(), false);
                    }
                    get size() { return this._list.length; }
                    append(name, value) {
                        this._list.push([String(name), String(value)]);
                        this._update();
                    }
                    delete(name, value) {
                        const n = String(name);
                        this._list = this._list.filter(([k, v]) => k !== n || (value !== undefined && v !== String(value)));
                        this._update();
                    }
                    get(name) {
                        const hit = this._list.find(([k]) => k === String(name));
                        return hit ? hit[1] : null;
                    }
                    getAll(name) {
                        return this._list.filter(([k]) => k === String(name)).map(([, v]) => v);
                    }
                    has(name, value) {
                        const n = String(name);
                        return this._list.some(([k, v]) => k === n && (value === undefined || v === String(value)));
                    }
                    set(name, value) {
                        const n = String(name);
                        const at = this._list.findIndex(([k]) => k === n);
                        if (at === -1) {
                            this._list.push([n, String(value)]);
                        } else {
                            this._list[at] = [n, String(value)];
                            this._list = this._list.filter(([k], i) => k !== n || i <= at);
                        }
                        this._update();
                    }
                    sort() {
                        this._list = this._list
                            .map((pair, i) => [pair, i])
                            .sort(([a, i], [b, j]) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : i - j))
                            .map(([pair]) => pair);
                        this._update();
                    }
                    forEach(callback, thisArg) {
                        for (const [k, v] of this._list) callback.call(thisArg, v, k, this);
                    }
                    *entries() { for (const [k, v] of this._list) yield [k, v]; }
                    *keys() { for (const [k] of this._list) yield k; }
                    *values() { for (const [, v] of this._list) yield v; }
                    [Symbol.iterator]() { return this.entries(); }
                    toString() { return __vaern_form_serialize(this._list); }
                }
                globalThis.URLSearchParams = URLSearchParams;

                function __parseUrl(input, base) {
                    return base === undefined ? __vaern_url_parse(String(input)) : __vaern_url_parse(String(input), String(base));
                }

                class URL {
                    constructor(input, base) {
                        const parts = __parseUrl(input, base);
                        if (!parts) {
                            throw new TypeError(`Invalid URL: ${input}`);
                        }
                        this._parts = parts;
                        this._params = new URLSearchParams(parts.search);
                        this._params._url = this;
                    }
                    static canParse(input, base) {
                        return Boolean(__parseUrl(input, base));
                    }
                    static parse(input, base) {
                        try {
                            return new URL(input, base);
                        } catch (_) {
                            return null;
                        }
                    }
                    _apply(part, value, resetParams) {
                        const parts = __vaern_url_set(this._parts.href, part, String(value));
                        if (!parts) {
                            if (part === 'href') throw new TypeError(`Invalid URL: ${value}`);
                            return;
                        }
                        this._parts = parts;
                        if (resetParams) this._params._reset(parts.search);
                    }
                    get href() { return this._parts.href; }
                    set href(v) { this._apply('href', v, true); }
                    get origin() { return this._parts.origin; }
                    get protocol() { return this._parts.protocol; }
                    set protocol(v) { this._apply('protocol', v, false); }
                    get username() { return this._parts.username; }
                    set username(v) { this._apply('username', v, false); }
                    get password() { return this._parts.password; }
                    set password(v) { this._apply('password', v, false); }
                    get host() { return this._parts.host; }
                    set host(v) { this._apply('host', v, false); }
                    get hostname() { return this._parts.hostname; }
                    set hostname(v) { this._apply('hostname', v, false); }
                    get port() { return this._parts.port; }
                    set port(v) { this._apply('port', v, false); }
                    get pathname() { return this._parts.pathname; }
                    set pathname(v) { this._apply('pathname', v, false); }
                    get search() { return this._parts.search; }
                    set search(v) { this._apply('search', v, true); }
                    get hash() { return this._parts.hash; }
                    set hash(v) { this._apply('hash', v, false); }
                    get searchParams() { return this._params; }
                    toString() { return this._parts.href; }
                    toJSON() { return this._parts.href; }
                }
                globalThis.URL = URL;

                // WHATWG ReadableStream
                class ReadableStreamDefaultReader {
                    constructor(stream) {
                        this._stream = stream;
                    }
                    async read() {
                        return this._stream._readNext();
                    }
                    async cancel(reason) {
                        return this._stream._cancel(reason);
                    }
                }

                class ReadableStreamDefaultController {
                    constructor(stream) {
                        this._stream = stream;
                    }
                    enqueue(chunk) {
                        this._stream._enqueue(chunk);
                    }
                    close() {
                        this._stream._close();
                    }
                    error(err) {
                        this._stream._error(err);
                    }
                }

                class ReadableStream {
                    constructor(underlyingSource = {}) {
                        this._queue = [];
                        this._waiters = [];
                        this._closed = false;
                        this._error = null;
                        this.locked = false;

                        this._controller = new ReadableStreamDefaultController(this);
                        if (typeof underlyingSource.start === 'function') {
                            try {
                                const res = underlyingSource.start(this._controller);
                                if (res && typeof res.catch === 'function') {
                                    res.catch(err => this._error(err));
                                }
                            } catch (err) {
                                this._error(err);
                            }
                        }
                    }

                    _enqueue(chunk) {
                        if (this._closed) throw new TypeError("Stream is closed");
                        if (this._waiters.length > 0) {
                            const waiter = this._waiters.shift();
                            waiter.resolve({ done: false, value: chunk });
                        } else {
                            this._queue.push(chunk);
                        }
                    }

                    _close() {
                        if (this._closed) return;
                        this._closed = true;
                        while (this._waiters.length > 0) {
                            const waiter = this._waiters.shift();
                            waiter.resolve({ done: true, value: undefined });
                        }
                    }

                    _error(err) {
                        this._error = err;
                        this._closed = true;
                        while (this._waiters.length > 0) {
                            const waiter = this._waiters.shift();
                            waiter.reject(err);
                        }
                    }

                    _readNext() {
                        if (this._queue.length > 0) {
                            const val = this._queue.shift();
                            return Promise.resolve({ done: false, value: val });
                        }
                        if (this._closed) {
                            if (this._error) return Promise.reject(this._error);
                            return Promise.resolve({ done: true, value: undefined });
                        }
                        return new Promise((resolve, reject) => {
                            this._waiters.push({ resolve, reject });
                        });
                    }

                    _cancel(_reason) {
                        this._close();
                        return Promise.resolve();
                    }

                    getReader() {
                        if (this.locked) {
                            throw new TypeError("ReadableStream is locked");
                        }
                        this.locked = true;
                        return new ReadableStreamDefaultReader(this);
                    }
                }
                globalThis.ReadableStream = ReadableStream;

                // WHATWG Headers
                class Headers {
                    constructor(init) {
                        this._map = new Map();
                        if (init) {
                            if (init instanceof Headers) {
                                init.forEach((v, k) => this.set(k, v));
                            } else if (Array.isArray(init)) {
                                for (const item of init) {
                                    if (Array.isArray(item) && item.length >= 2) {
                                        this.set(item[0], item[1]);
                                    }
                                }
                            } else if (typeof init === 'object') {
                                for (const [k, v] of Object.entries(init)) {
                                    this.set(k, v);
                                }
                            }
                        }
                    }

                    get(name) {
                        const lower = String(name).toLowerCase();
                        return this._map.has(lower) ? this._map.get(lower) : null;
                    }

                    set(name, value) {
                        const lower = String(name).toLowerCase();
                        this._map.set(lower, String(value));
                    }

                    append(name, value) {
                        const lower = String(name).toLowerCase();
                        const existing = this._map.get(lower);
                        if (existing !== undefined) {
                            this._map.set(lower, existing + ', ' + String(value));
                        } else {
                            this._map.set(lower, String(value));
                        }
                    }

                    has(name) {
                        return this._map.has(String(name).toLowerCase());
                    }

                    delete(name) {
                        this._map.delete(String(name).toLowerCase());
                    }

                    forEach(callback, thisArg) {
                        for (const [k, v] of this._map.entries()) {
                            callback.call(thisArg, v, k, this);
                        }
                    }

                    *entries() {
                        for (const [k, v] of this._map.entries()) {
                            yield [k, v];
                        }
                    }

                    *keys() {
                        for (const k of this._map.keys()) {
                            yield k;
                        }
                    }

                    *values() {
                        for (const v of this._map.values()) {
                            yield v;
                        }
                    }

                    [Symbol.iterator]() {
                        return this.entries();
                    }
                }
                globalThis.Headers = Headers;

                // WHATWG Request
                class Request {
                    constructor(input, init = {}) {
                        if (input instanceof Request) {
                            this.url = input.url;
                            this.method = init.method ? String(init.method).toUpperCase() : input.method;
                            this.headers = new Headers(init.headers || input.headers);
                            this._body = init.body !== undefined ? init.body : input._body;
                        } else {
                            this.url = String(input);
                            this.method = String(init.method || "GET").toUpperCase();
                            this.headers = init.headers instanceof Headers ? init.headers : new Headers(init.headers);
                            this._body = init.body;
                        }
                        this.bodyUsed = false;

                        if (this._body !== undefined && this._body !== null) {
                            if (this._body instanceof ReadableStream || (this._body && typeof this._body.getReader === 'function')) {
                                this.body = this._body;
                                this._bodyBytes = null;
                            } else {
                                let u8;
                                if (typeof this._body === 'string') {
                                    u8 = new TextEncoder().encode(this._body);
                                } else if (this._body instanceof Uint8Array) {
                                    u8 = this._body;
                                } else if (this._body instanceof ArrayBuffer) {
                                    u8 = new Uint8Array(this._body);
                                } else if (Array.isArray(this._body)) {
                                    u8 = new Uint8Array(this._body);
                                } else {
                                    u8 = new TextEncoder().encode(String(this._body));
                                }
                                this._bodyBytes = u8;
                                this.body = new ReadableStream({
                                    start(c) {
                                        c.enqueue(u8);
                                        c.close();
                                    }
                                });
                            }
                        } else {
                            this.body = null;
                            this._bodyBytes = new Uint8Array(0);
                        }
                    }

                    async text() {
                        if (this.bodyUsed) {
                            throw new TypeError("Body has already been consumed");
                        }
                        this.bodyUsed = true;
                        if (this._bodyBytes) {
                            return new TextDecoder().decode(this._bodyBytes);
                        }
                        if (this.body) {
                            return new TextDecoder().decode(await __readAllBytes(this.body));
                        }
                        return "";
                    }

                    async json() {
                        const txt = await this.text();
                        return JSON.parse(txt);
                    }

                    async arrayBuffer() {
                        if (this.bodyUsed) {
                            throw new TypeError("Body has already been consumed");
                        }
                        this.bodyUsed = true;
                        if (this._bodyBytes) {
                            return this._bodyBytes.buffer.slice(
                                this._bodyBytes.byteOffset,
                                this._bodyBytes.byteOffset + this._bodyBytes.byteLength
                            );
                        }
                        if (this.body) {
                            const bytes = await __readAllBytes(this.body);
                            return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
                        }
                        return new ArrayBuffer(0);
                    }
                }
                globalThis.Request = Request;

                // WHATWG Response
                class Response {
                    constructor(bodyBuffer, init = {}) {
                        this.status = Number(init.status) || 200;
                        this.statusText = String(init.statusText || "OK");
                        this.headers = init.headers instanceof Headers ? init.headers : new Headers(init.headers);
                        this.url = String(init.url || "");
                        this.ok = this.status >= 200 && this.status < 300;
                        this.bodyUsed = false;
                        this._isStream = false;

                        if (bodyBuffer instanceof ReadableStream || (bodyBuffer && typeof bodyBuffer.getReader === 'function')) {
                            this.body = bodyBuffer;
                            this._isStream = true;
                            this._bodyBuffer = null;
                            this._bodyBytes = null;
                        } else if (typeof bodyBuffer === 'string') {
                            const u8 = new TextEncoder().encode(bodyBuffer);
                            this._bodyBytes = u8;
                            this._bodyBuffer = u8.buffer;
                            this.body = new ReadableStream({
                                start(c) {
                                    c.enqueue(u8);
                                    c.close();
                                }
                            });
                        } else if (bodyBuffer instanceof Uint8Array) {
                            this._bodyBytes = bodyBuffer;
                            this._bodyBuffer = bodyBuffer.buffer;
                            this.body = new ReadableStream({
                                start(c) {
                                    c.enqueue(bodyBuffer);
                                    c.close();
                                }
                            });
                        } else if (bodyBuffer instanceof ArrayBuffer) {
                            this._bodyBytes = new Uint8Array(bodyBuffer);
                            this._bodyBuffer = bodyBuffer;
                            this.body = new ReadableStream({
                                start(c) {
                                    c.enqueue(new Uint8Array(bodyBuffer));
                                    c.close();
                                }
                            });
                        } else {
                            this._bodyBytes = new Uint8Array(0);
                            this._bodyBuffer = new ArrayBuffer(0);
                            this.body = null;
                        }
                    }

                    async text() {
                        if (this.bodyUsed) {
                            throw new TypeError("Body has already been consumed");
                        }
                        this.bodyUsed = true;
                        if (this._isStream && this.body) {
                            return new TextDecoder().decode(await __readAllBytes(this.body));
                        }
                        if (this._bodyBytes) {
                            return new TextDecoder().decode(this._bodyBytes);
                        }
                        return "";
                    }

                    async json() {
                        const txt = await this.text();
                        return JSON.parse(txt);
                    }

                    async arrayBuffer() {
                        if (this.bodyUsed) {
                            throw new TypeError("Body has already been consumed");
                        }
                        this.bodyUsed = true;
                        if (this._isStream && this.body) {
                            const bytes = await __readAllBytes(this.body);
                            return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
                        }
                        if (this._bodyBuffer instanceof ArrayBuffer) {
                            return this._bodyBuffer;
                        }
                        if (this._bodyBytes) {
                            return this._bodyBytes.buffer.slice(
                                this._bodyBytes.byteOffset,
                                this._bodyBytes.byteOffset + this._bodyBytes.byteLength
                            );
                        }
                        return new ArrayBuffer(0);
                    }
                }
                globalThis.Response = Response;

                // AbortSignal & AbortController
                let __nextAbortId = 1;
                class AbortSignal {
                    constructor(id) {
                        this._id = id;
                        this.aborted = false;
                        this.reason = undefined;
                        this._listeners = [];
                    }

                    addEventListener(type, listener) {
                        if (type === "abort") {
                            if (this.aborted) {
                                listener();
                            } else {
                                this._listeners.push(listener);
                            }
                        }
                    }

                    removeEventListener(type, listener) {
                        if (type === "abort") {
                            this._listeners = this._listeners.filter(l => l !== listener);
                        }
                    }

                    _trigger(reason) {
                        if (this.aborted) return;
                        this.aborted = true;
                        this.reason = reason;
                        for (const l of this._listeners) {
                            try { l(); } catch (_) {}
                        }
                    }

                    static abort(reason) {
                        const sig = new AbortSignal(__nextAbortId++);
                        const err = reason || new Error("The operation was aborted");
                        err.name = "AbortError";
                        sig._trigger(err);
                        return sig;
                    }

                    static timeout(ms) {
                        const controller = new AbortController();
                        setTimeout(() => {
                            const err = new Error("The operation timed out");
                            err.name = "TimeoutError";
                            controller.abort(err);
                        }, ms);
                        return controller.signal;
                    }
                }
                globalThis.AbortSignal = AbortSignal;

                class AbortController {
                    constructor() {
                        this.signal = new AbortSignal(__nextAbortId++);
                    }

                    abort(reason) {
                        const err = reason || new Error("The operation was aborted");
                        err.name = "AbortError";
                        this.signal._trigger(err);
                        __vaern_abort_signal(this.signal._id);
                    }
                }
                globalThis.AbortController = AbortController;

                // Framing and routing headers the runtime sets itself; the
                // list comes from fetch::RUNTIME_MANAGED_HEADERS.
                const __runtimeHeaders = new Set(__vaern_runtime_headers);
                const __checkHeaders = (where, list) => {
                    for (const [key] of list) {
                        if (__runtimeHeaders.has(key)) {
                            throw new TypeError(`${where}: header '${key}' is set by the runtime and cannot be supplied; remove it`);
                        }
                    }
                };

                // WHATWG fetch
                const __fetchCallbacks = new Map();
                let __nextFetchId = 1;

                globalThis.fetch = async (input, init = {}) => {
                    const url = typeof input === 'string' ? input : String(input);
                    const method = String(init.method || "GET").toUpperCase();
                    if (init.redirect !== undefined && init.redirect !== 'follow') {
                        throw new TypeError(`fetch: redirect '${init.redirect}' is not supported; redirects are always followed (up to 20 hops)`);
                    }

                    const headersList = [];
                    if (init.headers) {
                        const h = init.headers instanceof Headers ? init.headers : new Headers(init.headers);
                        h.forEach((val, key) => {
                            headersList.push([key, val]);
                        });
                    }
                    __checkHeaders("fetch", headersList);

                    let bodyBytes = null;
                    if (init.body !== undefined && init.body !== null) {
                        if (typeof init.body === 'string') {
                            bodyBytes = init.body;
                        } else if (init.body instanceof Uint8Array) {
                            bodyBytes = Array.from(init.body);
                        } else if (typeof init.body.getReader === 'function') {
                            const reader = init.body.getReader();
                            const chunks = [];
                            while (true) {
                                const { done, value } = await reader.read();
                                if (done) break;
                                if (value instanceof Uint8Array) {
                                    for (let i = 0; i < value.length; i++) chunks.push(value[i]);
                                }
                            }
                            bodyBytes = chunks;
                        } else {
                            throw new TypeError("Unsupported request body type");
                        }
                    }

                    const timeoutMs = typeof init.timeout === 'number' ? init.timeout : 30000;
                    const signal = init.signal;
                    if (signal && signal.aborted) {
                        const err = signal.reason || new Error("The operation was aborted");
                        err.name = "AbortError";
                        throw err;
                    }

                    const abortId = signal ? signal._id : 0;

                    return new Promise((resolve, reject) => {
                        const reqId = __nextFetchId++;
                        __fetchCallbacks.set(reqId, { resolve, reject, signal });

                        if (signal) {
                            signal.addEventListener("abort", () => {
                                const cb = __fetchCallbacks.get(reqId);
                                if (cb) {
                                    __fetchCallbacks.delete(reqId);
                                    const err = signal.reason || new Error("The operation was aborted");
                                    err.name = "AbortError";
                                    cb.reject(err);
                                }
                            });
                        }

                        __vaern_fetch_start(reqId, url, method, headersList, bodyBytes, timeoutMs, abortId);
                    });
                };

                globalThis.__vaern_dispatch_fetch = (info) => {
                    const cb = __fetchCallbacks.get(info.reqId);
                    if (!cb) return;

                    if (info.success) {
                        // Built before the callback is removed, so a throw
                        // here leaves it for __vaern_reject_fetch.
                        const headers = new Headers(info.headers);
                        const res = new Response(info.body, {
                            status: info.status,
                            statusText: info.statusText,
                            headers,
                            url: info.url
                        });
                        __fetchCallbacks.delete(info.reqId);
                        cb.resolve(res);
                    } else {
                        __fetchCallbacks.delete(info.reqId);
                        if (info.message && info.message.includes("aborted by AbortSignal")) {
                            const err = (cb.signal && cb.signal.reason) || new Error("The operation was aborted");
                            err.name = "AbortError";
                            cb.reject(err);
                        } else {
                            const err = new TypeError(info.message || "Fetch failed");
                            err.stage = info.stage;
                            err.url = info.url;
                            err.action = info.action;
                            cb.reject(err);
                        }
                    }
                };

                globalThis.__vaern_reject_fetch = (reqId, err) => {
                    const cb = __fetchCallbacks.get(reqId);
                    if (!cb) return false;
                    __fetchCallbacks.delete(reqId);
                    cb.reject(err);
                    return true;
                };

                // vaern:fs: whole-file reads and writes
                const __fsCallbacks = new Map();
                let __nextFsId = 1;

                class FsError extends Error {
                    constructor(info) {
                        super(info.message);
                        this.name = "FsError";
                        this.stage = info.stage;
                        this.code = info.code;
                        this.path = info.path;
                        this.action = info.action;
                    }
                }

                function __fsPath(fn, path) {
                    if (typeof path !== "string") {
                        throw new TypeError(`${fn}: path must be a string`);
                    }
                    if (path.length === 0 || path.includes("\0")) {
                        throw new TypeError(`${fn}: path must be non-empty and free of NUL characters`);
                    }
                }

                function __fsEncoding(fn, options) {
                    const encoding = typeof options === "string"
                        ? options
                        : (options !== null && typeof options === "object" ? options.encoding : undefined);
                    if (encoding === undefined || encoding === null) return null;
                    if (encoding === "utf8" || encoding === "utf-8") return "utf8";
                    throw new TypeError(`${fn}: unsupported encoding ${JSON.stringify(String(encoding))}; use "utf8", or omit it for bytes`);
                }

                function __fsData(fn, data) {
                    if (typeof data === "string") return ["text", data.toWellFormed()];
                    if (data instanceof ArrayBuffer || ArrayBuffer.isView(data)) {
                        return ["bytes", __latin1(__toBytes(data))];
                    }
                    throw new TypeError(`${fn}: data must be a string, ArrayBuffer or ArrayBuffer view`);
                }

                function __fsStart(op, path, kind, payload) {
                    return new Promise((resolve, reject) => {
                        const reqId = __nextFsId++;
                        __fsCallbacks.set(reqId, { resolve, reject });
                        __vaern_fs_start(reqId, op, path, kind, payload);
                    });
                }

                globalThis.__vaern_dispatch_fs = (info) => {
                    const cb = __fsCallbacks.get(info.reqId);
                    if (!cb) return;
                    // Built before the callback is removed, so a throw here
                    // leaves it for __vaern_reject_fs.
                    const value = info.success
                        ? (info.body instanceof ArrayBuffer ? new Uint8Array(info.body) : info.body)
                        : new FsError(info);
                    __fsCallbacks.delete(info.reqId);
                    if (info.success) cb.resolve(value); else cb.reject(value);
                };

                globalThis.__vaern_reject_fs = (reqId, err) => {
                    const cb = __fsCallbacks.get(reqId);
                    if (!cb) return false;
                    __fsCallbacks.delete(reqId);
                    cb.reject(err);
                    return true;
                };

                globalThis.__vaern_fs = Object.freeze({
                    async readFile(path, options) {
                        __fsPath("readFile", path);
                        const utf8 = __fsEncoding("readFile", options) === "utf8";
                        return __fsStart(utf8 ? "read_utf8" : "read", path, "text", "");
                    },
                    async writeFile(path, data, options) {
                        __fsPath("writeFile", path);
                        __fsEncoding("writeFile", options);
                        const flag = options !== null && typeof options === "object" && options.flag !== undefined
                            ? options.flag
                            : "w";
                        if (flag !== "w" && flag !== "wx") {
                            throw new TypeError(`writeFile: unsupported flag ${JSON.stringify(String(flag))}; use "w" or "wx"`);
                        }
                        const [kind, payload] = __fsData("writeFile", data);
                        await __fsStart(flag === "wx" ? "write_new" : "write", path, kind, payload);
                    },
                    async appendFile(path, data, options) {
                        __fsPath("appendFile", path);
                        __fsEncoding("appendFile", options);
                        const [kind, payload] = __fsData("appendFile", data);
                        await __fsStart("append", path, kind, payload);
                    },
                });

                // vaern:http serve implementation
                const __serverHandlers = new Map();
                const __serverCloseCallbacks = new Map();
                let __nextServerId = 1;

                globalThis.__vaern_trigger_server_close = (serverId) => {
                    const cb = __serverCloseCallbacks.get(serverId);
                    if (cb) {
                        __serverCloseCallbacks.delete(serverId);
                        cb();
                    }
                };

                globalThis.__vaern_serve = (handler, options = {}) => {
                    if (typeof handler !== 'function') {
                        throw new TypeError("Handler must be a function");
                    }
                    const serverId = __nextServerId++;
                    const hostname = options.hostname !== undefined ? String(options.hostname) : "127.0.0.1";
                    const port = options.port !== undefined ? Number(options.port) : 8000;
                    const infoRaw = __vaern_server_start(serverId, hostname, port);
                    const info = JSON.parse(infoRaw);
                    if (!info.ok) {
                        throw new Error(info.error);
                    }
                    __serverHandlers.set(serverId, handler);

                    return {
                        addr: {
                            hostname: info.hostname,
                            port: info.port,
                        },
                        close: async () => {
                            __serverHandlers.delete(serverId);
                            return new Promise((resolve) => {
                                __serverCloseCallbacks.set(serverId, resolve);
                                __vaern_server_close(serverId);
                            });
                        }
                    };
                };

                // Streamed responses waiting for the client to make buffer room,
                // keyed by request id; settled by __vaern_dispatch_stream_sent.
                const __streamWaits = new Map();
                globalThis.__vaern_dispatch_stream_sent = (reqId, message) => {
                    const wait = __streamWaits.get(reqId);
                    if (!wait) return false;
                    __streamWaits.delete(reqId);
                    if (message === undefined) wait.resolve();
                    else wait.reject(new Error(message));
                    return true;
                };

                globalThis.__vaern_dispatch_server_request = (reqInfo) => {
                    const handler = __serverHandlers.get(reqInfo.serverId);
                    if (!handler) {
                        __vaern_server_respond(reqInfo.reqId, 404, "Not Found", [["Content-Type", "text/plain"]], "Not Found\n");
                        return;
                    }

                    const fullUrl = `http://${reqInfo.hostname}:${reqInfo.port}${reqInfo.uri}`;
                    const req = new Request(fullUrl, {
                        method: reqInfo.method,
                        headers: reqInfo.headers,
                        body: reqInfo.body && reqInfo.body.length > 0 ? new Uint8Array(reqInfo.body) : null,
                    });

                    Promise.resolve()
                        .then(() => handler(req))
                        .then(async (response) => {
                            if (!(response instanceof Response)) {
                                response = new Response(String(response || ""));
                            }

                            const status = response.status;
                            const statusText = response.statusText;
                            const headersList = [];
                            response.headers.forEach((val, key) => {
                                headersList.push([key, val]);
                            });
                            __checkHeaders("serve: handler response", headersList);

                            if (response._isStream || (response.body && typeof response.body.getReader === 'function')) {
                                __vaern_server_respond_stream_start(reqInfo.reqId, status, statusText, headersList);
                                const reader = response.body.getReader();
                                // Reads of an already-filled stream settle as
                                // microtasks, so a fast client would let the
                                // pump hold the thread for the whole body;
                                // every 16 chunks it lets timers and other
                                // events run.
                                let sinceYield = 0;
                                try {
                                    while (true) {
                                        if (++sinceYield > 16) {
                                            sinceYield = 0;
                                            await __yieldToEventLoop();
                                        }
                                        const { done, value } = await reader.read();
                                        if (done) {
                                            __vaern_server_respond_stream_chunk(reqInfo.reqId, "", true);
                                            break;
                                        }
                                        let chunkBytes;
                                        if (typeof value === 'string') {
                                            chunkBytes = new TextEncoder().encode(value);
                                        } else if (value instanceof Uint8Array) {
                                            chunkBytes = value;
                                        } else {
                                            chunkBytes = new TextEncoder().encode(String(value));
                                        }
                                        if (!__vaern_server_respond_stream_chunk(reqInfo.reqId, __latin1(chunkBytes), false)) {
                                            await new Promise((resolve, reject) => {
                                                __streamWaits.set(reqInfo.reqId, { resolve, reject });
                                            });
                                        }
                                    }
                                } catch (streamErr) {
                                    __vaern_print(1, `[ERROR] Stream read error: ${streamErr}`);
                                    __vaern_server_respond_stream_chunk(reqInfo.reqId, "", true);
                                }
                            } else {
                                let bodyBytes = [];
                                if (response._bodyBytes) {
                                    bodyBytes = Array.from(response._bodyBytes);
                                } else if (response._bodyBuffer) {
                                    bodyBytes = Array.from(new Uint8Array(response._bodyBuffer));
                                }
                                __vaern_server_respond(reqInfo.reqId, status, statusText, headersList, bodyBytes);
                            }
                        })
                        .catch((err) => {
                            __vaern_print(1, `[ERROR] ${reqInfo.method} ${reqInfo.uri}: ${__formatArg(err)}`);
                            const bodyBytes = Array.from(new TextEncoder().encode("Internal Server Error\n"));
                            __vaern_server_respond(
                                reqInfo.reqId,
                                500,
                                "Internal Server Error",
                                [["Content-Type", "text/plain"]],
                                bodyBytes
                            );
                        });
                };
            "#;

            ctx.eval::<(), _>(bootstrap_js)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to initialize JavaScript environment: {err}"),
                })?;
            ctx.eval::<(), _>(include_str!("js/ai.js"))
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to initialize vaern:ai: {err}"),
                })?;
            ctx.eval::<(), _>(include_str!("js/random.js"))
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to initialize the seeded Math.random: {err}"),
                })?;

            // Populate process.argv
            let argv_arr = Array::new(ctx.clone()).map_err(|err| VaernError::EngineInit {
                reason: format!("failed to allocate argv array: {err}"),
            })?;
            for (idx, item) in argv.iter().enumerate() {
                argv_arr
                    .set(idx, item.as_str())
                    .map_err(|err| VaernError::EngineInit {
                        reason: format!("failed to populate argv item {idx}: {err}"),
                    })?;
            }
            let process_obj: Object =
                globals
                    .get("process")
                    .map_err(|err| VaernError::EngineInit {
                        reason: format!("failed to retrieve process object: {err}"),
                    })?;
            process_obj
                .set("argv", argv_arr)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to assign process.argv: {err}"),
                })?;

            // Populate process.env. Entries that are not valid UTF-8 are
            // skipped because JS strings cannot represent them.
            let env_obj = Object::new(ctx.clone()).map_err(|err| VaernError::EngineInit {
                reason: format!("failed to allocate process.env: {err}"),
            })?;
            for (key, value) in std::env::vars_os() {
                if let (Ok(key), Ok(value)) = (key.into_string(), value.into_string()) {
                    env_obj
                        .set(key.as_str(), value.as_str())
                        .map_err(|err| VaernError::EngineInit {
                            reason: format!("failed to populate process.env.{key}: {err}"),
                        })?;
                }
            }
            process_obj
                .set("env", env_obj)
                .map_err(|err| VaernError::EngineInit {
                    reason: format!("failed to assign process.env: {err}"),
                })?;

            Ok(())
        })
    }

    /// Writes the record for a completed effect. An error means the effect
    /// happened but could not be recorded; the caller hands it to the script
    /// in place of the result.
    fn record_effect(
        &self,
        kind: &'static str,
        req_id: u32,
        outcome: Outcome,
    ) -> Option<VaernError> {
        let (pending, turn) = {
            let mut s = self.state.borrow_mut();
            (s.pending_effects.remove(&(kind, req_id))?, s.turn)
        };
        let mut slot = self.journal.borrow_mut();
        let journal = slot.as_mut()?;
        let mut fields = pending.finish(outcome);
        fields.insert("turn".into(), turn.into());
        journal.append(kind, fields).err()
    }

    fn dispatch_fetch_completion(&self, completion: FetchCompletion) {
        let outcome = crate::effect_record::fetch_outcome(&completion.result);
        let completion = match self.record_effect("fetch", completion.req_id, outcome) {
            Some(err) => FetchCompletion {
                req_id: completion.req_id,
                result: Err(err),
            },
            None => completion,
        };
        let req_id = completion.req_id;
        let unsettled = self.context.with(|ctx| {
            if Self::settle_fetch(&ctx, completion).is_ok() {
                return None;
            }
            Self::reject_undelivered(&ctx, "__vaern_reject_fetch", "fetch request", req_id)
        });
        self.note_unsettled(unsettled);
    }

    /// Rejects the promise of request `req_id` with the exception a failed
    /// delivery left pending, most often out of memory while the result was
    /// built in the heap. The script can then handle it; otherwise the
    /// promise would never settle and the run could end with status 0.
    /// Returns what to report when even that fails.
    fn reject_undelivered(ctx: &Ctx<'_>, hook: &str, what: &str, req_id: u32) -> Option<String> {
        let exception = ctx.catch();
        let rejected = ctx
            .globals()
            .get::<_, Function>(hook)
            .and_then(|reject| reject.call::<_, bool>((req_id, exception)));
        match rejected {
            Ok(true) => None,
            Ok(false) => Some(format!(
                "{what} {req_id} failed while its result was delivered"
            )),
            Err(err) => Some(format!("{what} {req_id} could not be rejected: {err}")),
        }
    }

    /// Fails the run with `message` when a result reached no promise.
    fn note_unsettled(&self, message: Option<String>) {
        if let Some(message) = message {
            let mut s = self.state.borrow_mut();
            if s.unhandled_error.is_none() {
                s.unhandled_error = Some(message);
            }
        }
    }

    fn settle_fetch(ctx: &Ctx<'_>, completion: FetchCompletion) -> rquickjs::Result<()> {
        {
            let dispatch_fn: Function = ctx.globals().get("__vaern_dispatch_fetch")?;
            let info = Object::new(ctx.clone())?;
            info.set("reqId", completion.req_id)?;

            match completion.result {
                Ok(resp) => {
                    info.set("success", true)?;
                    info.set("status", resp.status)?;
                    info.set("statusText", resp.status_text.as_str())?;
                    info.set("url", resp.url.as_str())?;

                    let headers_arr = Array::new(ctx.clone())?;
                    for (i, (k, v)) in resp.headers.iter().enumerate() {
                        let pair = Array::new(ctx.clone())?;
                        pair.set(0, k.as_str())?;
                        pair.set(1, v.as_str())?;
                        headers_arr.set(i, pair)?;
                    }
                    info.set("headers", headers_arr)?;

                    let body_ab = ArrayBuffer::new(ctx.clone(), resp.body)?;
                    info.set("body", body_ab)?;
                }
                Err(err) => {
                    let (stage, url, message, action) = match err {
                        VaernError::Fetch {
                            stage,
                            url,
                            message,
                            action,
                        } => (stage, url, message, action),
                        other => (
                            "read".to_string(),
                            "".to_string(),
                            other.to_string(),
                            "inspect error details".to_string(),
                        ),
                    };

                    info.set("success", false)?;
                    info.set("stage", stage.as_str())?;
                    info.set("url", url.as_str())?;
                    info.set("message", message.as_str())?;
                    info.set("action", action.as_str())?;
                }
            }

            dispatch_fn.call::<_, ()>((info,))?;
            Ok(())
        }
    }

    /// Settles a model call's promise. The charge comes from reported usage;
    /// a reply without usage is charged what the call could have cost.
    fn dispatch_model_completion(&self, completion: Box<ModelCompletion>) {
        let ModelCompletion {
            req_id,
            call,
            result,
        } = *completion;
        let eid = self
            .state
            .borrow()
            .pending_effects
            .get(&("model.call", req_id))
            .map(|pending| pending.eid);
        let charged = match &result {
            Ok(reply) => match crate::ai::billable_usage(reply) {
                Some((input, output)) => crate::ai::charge_nano(&call.config, input, output),
                None => crate::ai::reserve_nano(&call),
            },
            // Nothing reached the server, so nothing can have been billed.
            Err(VaernError::Model { sent: false, .. }) => 0,
            // The request went out. Whether it was billed is unknown, so
            // the failure costs what the call could have cost.
            Err(_) => crate::ai::reserve_nano(&call),
        };
        let outcome = crate::effect_record::model_outcome(&result, charged);
        let result = match self.record_effect("model.call", req_id, outcome) {
            Some(err) => Err(err),
            None => result,
        };
        let charge = match &result {
            Ok(reply) => {
                crate::ai::billable_usage(reply).map(|(input, output)| crate::budget::Amount {
                    nano: charged,
                    tokens: input.saturating_add(output),
                })
            }
            Err(VaernError::Model { sent: false, .. }) => Some(crate::budget::Amount::default()),
            Err(_) => None,
        };
        let settled = self.budget.borrow_mut().settle(
            self.journal.borrow_mut().as_mut(),
            "model.call",
            req_id,
            charge,
        );
        let result = match settled {
            Err(err) => Err(err),
            Ok(()) => result,
        };
        let info = match result {
            // The reply is recorded and charged whether or not it matches the
            // schema: the call happened and was billed either way.
            Ok(reply) => match crate::ai::structured(&call, &reply) {
                Ok(json) => serde_json::json!({
                    "reqId": req_id,
                    "ok": true,
                    "text": reply.text,
                    "json": json,
                    "finishReason": reply.finish_reason,
                    "inputTokens": reply.input_tokens,
                    "outputTokens": reply.output_tokens,
                    "chargedNano": charged,
                    "eid": eid,
                    "toolCalls": reply.tool_calls,
                }),
                Err(mismatch) => serde_json::json!({
                    "reqId": req_id,
                    "ok": false,
                    "stage": "schema",
                    "url": call.endpoint,
                    "status": reply.status,
                    "path": mismatch.path,
                    "message": format!(
                        "the reply does not match the schema at {}: {}",
                        mismatch.path, mismatch.message
                    ),
                    "action": "Retry, simplify the schema, or use a model that supports structured output.",
                    "chargedNano": charged,
                    "eid": eid,
                }),
            },
            Err(err) => {
                let failed_call = matches!(err, VaernError::Model { .. });
                let mut info = model_error_info(err);
                if let Some(fields) = info.as_object_mut() {
                    fields.insert("reqId".into(), req_id.into());
                    fields.insert("ok".into(), false.into());
                    if failed_call {
                        fields.insert("chargedNano".into(), charged.into());
                        fields.insert("eid".into(), eid.into());
                    }
                }
                info
            }
        };
        let unsettled = self.context.with(|ctx| {
            let delivered = ctx
                .globals()
                .get::<_, Function>("__vaern_dispatch_model")
                .and_then(|dispatch| dispatch.call::<_, ()>((info.to_string(),)));
            if delivered.is_ok() {
                return None;
            }
            Self::reject_undelivered(&ctx, "__vaern_reject_model", "vaern:ai call", req_id)
        });
        self.note_unsettled(unsettled);
    }

    /// The full resolution error behind a failed import, when `message` is
    /// the shortened form QuickJS threw for it.
    fn take_resolve_failure(&self, message: &str) -> Option<VaernError> {
        let failure = self.resolve_failure.borrow_mut().take()?;
        match &failure {
            VaernError::ModuleResolution { specifier, .. }
                if message.contains(&format!("Error loading module '{specifier}'")) =>
            {
                Some(failure)
            }
            _ => None,
        }
    }

    fn dispatch_fs_completion(&self, completion: FsCompletion) {
        let req_id = completion.req_id;
        let outcome = crate::effect_record::fs_outcome(&completion.result);
        let completion = match self.record_effect("fs", req_id, outcome) {
            Some(err) => FsCompletion {
                req_id,
                result: Err(err),
            },
            None => completion,
        };
        let unsettled = self.context.with(|ctx| {
            if Self::settle_fs(&ctx, completion).is_ok() {
                return None;
            }
            Self::reject_undelivered(&ctx, "__vaern_reject_fs", "vaern:fs request", req_id)
        });
        self.note_unsettled(unsettled);
    }

    fn settle_fs<'js>(ctx: &Ctx<'js>, completion: FsCompletion) -> rquickjs::Result<()> {
        let dispatch_fn: Function = ctx.globals().get("__vaern_dispatch_fs")?;
        let info = Object::new(ctx.clone())?;
        info.set("reqId", completion.req_id)?;
        match completion.result {
            Ok(crate::fs::FsOutput::Bytes(bytes)) => {
                info.set("success", true)?;
                info.set("body", ArrayBuffer::new(ctx.clone(), bytes)?)?;
            }
            Ok(crate::fs::FsOutput::Text(text)) => {
                info.set("success", true)?;
                info.set("body", text)?;
            }
            Ok(crate::fs::FsOutput::Written) => {
                info.set("success", true)?;
            }
            Err(err) => {
                let message = err.to_string();
                let (stage, code, path, action) = match err {
                    VaernError::Fs {
                        stage,
                        code,
                        path,
                        action,
                        ..
                    } => (stage, code, path, action),
                    _ => (
                        "read".to_string(),
                        "EIO".to_string(),
                        String::new(),
                        "inspect error details".to_string(),
                    ),
                };
                info.set("success", false)?;
                info.set("message", message)?;
                info.set("stage", stage)?;
                info.set("code", code)?;
                info.set("path", path)?;
                info.set("action", action)?;
            }
        }
        dispatch_fn.call::<_, ()>((info,))?;
        Ok(())
    }

    fn dispatch_server_request(&self, req: ServerIncomingRequest) {
        let req_id = req.req_id;
        let line = format!("{} {}", req.method, req.uri);
        let failure = self.context.with(|ctx| {
            let (dispatch_fn, info) = match Self::server_request_info(&ctx, &req) {
                Ok(built) => built,
                Err(_) => return Some((describe_rejection(&ctx.catch()), Some(req.resp_sender))),
            };
            // Registered before the call: a request for an unknown server is
            // answered from inside it.
            self.state
                .borrow_mut()
                .server_resp_senders
                .insert(req_id, req.resp_sender);
            match dispatch_fn.call::<_, ()>((info,)) {
                Ok(()) => None,
                Err(_) => {
                    let reason = describe_rejection(&ctx.catch());
                    let sender = self.state.borrow_mut().server_resp_senders.remove(&req_id);
                    Some((reason, sender))
                }
            }
        });
        // The request never reached the handler, so nothing else will answer
        // it: the client would wait until it gave up.
        if let Some((reason, sender)) = failure {
            eprintln!(
                "[ERROR] {line}: the request could not be delivered to the handler: {reason}"
            );
            if let Some(sender) = sender {
                let _ = sender.send(ServerResponse {
                    status: 500,
                    status_text: "Internal Server Error".to_string(),
                    headers: vec![("content-type".to_string(), "text/plain".to_string())],
                    body: ServerResponseBody::Full(b"Internal Server Error\n".to_vec()),
                });
            }
        }
    }

    /// Builds the object the JavaScript side receives for `req`, and finds
    /// the function to hand it to.
    fn server_request_info<'js>(
        ctx: &Ctx<'js>,
        req: &ServerIncomingRequest,
    ) -> rquickjs::Result<(Function<'js>, Object<'js>)> {
        let dispatch_fn: Function = ctx.globals().get("__vaern_dispatch_server_request")?;
        let info = Object::new(ctx.clone())?;
        info.set("serverId", req.server_id)?;
        info.set("reqId", req.req_id as f64)?;
        info.set("method", req.method.as_str())?;
        info.set("uri", req.uri.as_str())?;
        info.set("hostname", req.hostname.as_str())?;
        info.set("port", req.port)?;

        let headers_arr = Array::new(ctx.clone())?;
        for (i, (k, v)) in req.headers.iter().enumerate() {
            let pair = Array::new(ctx.clone())?;
            pair.set(0, k.as_str())?;
            pair.set(1, v.as_str())?;
            headers_arr.set(i, pair)?;
        }
        info.set("headers", headers_arr)?;

        let body_arr = Array::new(ctx.clone())?;
        for (i, &byte) in req.body.iter().enumerate() {
            body_arr.set(i, byte)?;
        }
        info.set("body", body_arr)?;
        Ok((dispatch_fn, info))
    }

    fn dispatch_stream_chunk_sent(&self, sent: StreamChunkSent) {
        let req_id = sent.req_id;
        let message = {
            let mut s = self.state.borrow_mut();
            s.stream_sends_waiting.remove(&req_id);
            match sent.result {
                Ok(()) => None,
                Err(err) => {
                    s.server_stream_senders.remove(&req_id);
                    Some(err.to_string())
                }
            }
        };
        let delivered = self.context.with(|ctx| {
            ctx.globals()
                .get::<_, Function>("__vaern_dispatch_stream_sent")
                .and_then(|dispatch| dispatch.call::<_, bool>((req_id as f64, message)))
        });
        // The pump is suspended until this settles; failing to settle it would
        // leave the response open with no error anywhere.
        let unsettled = match delivered {
            Ok(true) => None,
            Ok(false) => Some(format!(
                "response stream {req_id}: a chunk finished sending with no write waiting for it"
            )),
            Err(err) => Some(format!(
                "response stream {req_id}: the chunk outcome could not be delivered: {err}"
            )),
        };
        if let Some(message) = unsettled {
            let mut s = self.state.borrow_mut();
            if s.unhandled_error.is_none() {
                s.unhandled_error = Some(message);
            }
        }
    }

    fn handle_event(&self, event: LoopEvent) {
        match event {
            LoopEvent::ServerRequest(req) => self.dispatch_server_request(req),
            LoopEvent::StreamChunkSent(sent) => self.dispatch_stream_chunk_sent(sent),
            LoopEvent::FetchDone(completion) => {
                if self.state.borrow().pending_fetches > 0 {
                    self.state.borrow_mut().pending_fetches -= 1;
                }
                self.dispatch_fetch_completion(completion);
            }
            LoopEvent::FsDone(completion) => {
                {
                    let mut s = self.state.borrow_mut();
                    s.pending_fs = s.pending_fs.saturating_sub(1);
                }
                self.dispatch_fs_completion(completion);
            }
            LoopEvent::ModelDone(completion) => {
                {
                    let mut s = self.state.borrow_mut();
                    s.pending_models = s.pending_models.saturating_sub(1);
                }
                self.dispatch_model_completion(completion);
            }
            LoopEvent::ModelDelta(req_id, delta) => {
                let delivered = self.context.with(|ctx| -> rquickjs::Result<()> {
                    let deliver: Function = ctx.globals().get("__vaern_model_delta")?;
                    deliver.call::<_, ()>((req_id, delta))
                });
                if let Err(err) = delivered {
                    let mut s = self.state.borrow_mut();
                    if s.unhandled_error.is_none() {
                        s.unhandled_error = Some(format!(
                            "vaern:ai stream {req_id} could not deliver a fragment to the script: {err}"
                        ));
                    }
                }
            }
            LoopEvent::ServerClosed(closed_server_id) => {
                if self.state.borrow().active_servers > 0 {
                    self.state.borrow_mut().active_servers -= 1;
                }
                self.context.with(|ctx| {
                    let trigger_fn: rquickjs::Result<Function> =
                        ctx.globals().get("__vaern_trigger_server_close");
                    if let Ok(f) = trigger_fn {
                        let _ = f.call::<_, ()>((closed_server_id,));
                    }
                });
            }
        }
    }

    fn run_event_loop(&self, file: &str) -> Result<i32> {
        loop {
            while let Ok(event) = self.events_rx.try_recv() {
                self.handle_event(event);
            }
            // A replay hands back recorded outcomes in the order the run
            // completed them, a recorded turn's worth at a time.
            if let Some(replay) = self.replay.borrow_mut().as_mut() {
                replay.begin_turn();
            }
            loop {
                let replayed = self
                    .replay
                    .borrow_mut()
                    .as_mut()
                    .and_then(|r| r.next_event());
                let Some(event) = replayed else { break };
                self.handle_event(event);
            }

            // Drain all pending microtasks
            while self.qjs_runtime.is_job_pending() {
                if let Err(err) = self.qjs_runtime.execute_pending_job() {
                    return Err(VaernError::Execution {
                        file: file.to_string(),
                        message: format!("unhandled microtask rejection: {err}"),
                    });
                }
            }
            self.state.borrow_mut().turn += 1;

            let fatal = {
                let mut s = self.state.borrow_mut();
                match s.unhandled_error.take() {
                    Some(message) => Some(message),
                    None if !s.pending_rejections.is_empty() => {
                        Some(s.pending_rejections.remove(0).1)
                    }
                    None => None,
                }
            };
            if let Some(message) = fatal {
                return Err(VaernError::Execution {
                    file: file.to_string(),
                    message,
                });
            }
            let divergence = self
                .replay
                .borrow_mut()
                .as_mut()
                .and_then(|r| r.take_divergence());
            if let Some(divergence) = divergence {
                return Err(divergence);
            }

            // Inspect active timers, pending fetch status, and active servers
            let (
                expired,
                min_deadline,
                has_pending_fetches,
                has_active_servers,
                _has_active_timers,
            ) = {
                let s = self.state.borrow();
                // File operations and stream chunks waiting for the client keep
                // the loop alive the same way a fetch does.
                let pending_fetches = s.pending_fetches > 0
                    || s.pending_fs > 0
                    || s.pending_models > 0
                    || !s.stream_sends_waiting.is_empty();
                let active_timers = !s.timers.is_empty();
                let active_servers = s.active_servers > 0;
                if !pending_fetches && !active_timers && !active_servers {
                    break;
                }
                let now = Instant::now();
                let mut exp = Vec::new();
                let mut min_dl = None;

                for (&id, record) in s.timers.iter() {
                    if record.deadline <= now {
                        exp.push(id);
                    } else {
                        match min_dl {
                            None => min_dl = Some(record.deadline),
                            Some(earliest) if record.deadline < earliest => {
                                min_dl = Some(record.deadline);
                            }
                            _ => {}
                        }
                    }
                }
                (exp, min_dl, pending_fetches, active_servers, active_timers)
            };

            if !expired.is_empty() {
                for id in expired {
                    let interval = self.state.borrow().timers.get(&id).and_then(|t| t.interval);
                    if let Some(duration) = interval {
                        if let Some(timer) = self.state.borrow_mut().timers.get_mut(&id) {
                            timer.deadline = Instant::now() + duration;
                        }
                    } else {
                        self.state.borrow_mut().timers.remove(&id);
                    }

                    self.context.with(|ctx| {
                        let trigger_fn: rquickjs::Result<Function> =
                            ctx.globals().get("__vaern_trigger_timer");
                        if let Ok(f) = trigger_fn {
                            let _ = f.call::<_, ()>((id,));
                        }
                    });

                    // An uncaught timer exception ends the run before any
                    // other expired timer fires, as it would in Node.
                    if let Some(message) = self.state.borrow_mut().unhandled_error.take() {
                        return Err(VaernError::Execution {
                            file: file.to_string(),
                            message,
                        });
                    }
                }
            } else if let Some(replay) = self.replay.borrow_mut().as_mut()
                && !has_active_servers
                && (replay.has_work() || (has_pending_fetches && min_deadline.is_none()))
            {
                // Nothing arrives on the channel during a replay, so waiting
                // on it with no timer due would never end.
                if !replay.has_work() {
                    replay.stall();
                }
            } else {
                // Park until a Tokio task sends work or the nearest timer is
                // due. Every fetch, server request and server close arrives as
                // an event, so nothing can complete unseen while parked.
                self.parks.set(self.parks.get() + 1);
                let event = match min_deadline {
                    Some(deadline) => {
                        let wait = deadline.saturating_duration_since(Instant::now());
                        match self.events_rx.recv_timeout(wait) {
                            Ok(event) => Some(event),
                            Err(RecvTimeoutError::Timeout) => None,
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                    None if has_pending_fetches || has_active_servers => {
                        match self.events_rx.recv() {
                            Ok(event) => Some(event),
                            Err(_) => break,
                        }
                    }
                    None => break,
                };
                if let Some(event) = event {
                    self.handle_event(event);
                }
            }
        }

        // Retrieve process.exitCode
        let exit_code = self.context.with(|ctx| {
            let process_obj: rquickjs::Result<Object> = ctx.globals().get("process");
            if let Ok(proc) = process_obj {
                proc.get::<_, i32>("exitCode").unwrap_or_default()
            } else {
                0
            }
        });

        Ok(exit_code)
    }
}

impl Runtime for QuickJsRuntime {
    fn run_file(&mut self, path: &Path) -> Result<i32> {
        let outcome = self.run_file_inner(path);
        self.seal_journal(outcome)
    }

    fn eval(&mut self, filename: &str, source: &str) -> Result<i32> {
        let outcome = self.eval_inner(filename, source);
        self.seal_journal(outcome)
    }
}

impl QuickJsRuntime {
    /// Records the next run's effects in a journal under `home` (normally
    /// `.vaern` in the working directory). `None`, the default, records
    /// nothing.
    pub fn set_journal_home(&mut self, home: Option<PathBuf>) {
        self.reserved_homes.borrow_mut()[0].clone_from(&home);
        self.journal_home = home;
    }

    /// Caps the run's budget (from `--budget-usd` and `--budget-tokens`)
    /// and sets where named budget ledgers live.
    pub fn set_budget(&mut self, caps: crate::budget::Limits, home: Option<PathBuf>) {
        self.reserved_homes.borrow_mut()[1].clone_from(&home);
        self.budget.borrow_mut().configure(caps, home);
    }

    /// Replays a recorded run: its sources must be unchanged, every effect
    /// is served from its journal, and the transcript and outcome must
    /// match. Construct the runtime with the plan's `argv`.
    pub fn replay(&mut self, plan: crate::replay::Plan) -> Result<crate::replay::Summary> {
        plan.check_sources()?;
        self.journal_home = None;
        self.budget
            .borrow_mut()
            .replay(plan.caps, plan.ledgers.clone());
        let seed = plan.seed;
        let script = PathBuf::from(&plan.script);
        *self.replay.borrow_mut() = Some(crate::replay::Replay::new(plan));
        self.seed_random(seed)?;
        let outcome = self.run_file_inner(&script);
        let mut slot = self.replay.borrow_mut();
        match slot.as_mut() {
            Some(replay) => replay.finish(outcome),
            None => Err(VaernError::EngineInit {
                reason: "the replay state was lost during the run".into(),
            }),
        }
    }

    /// Opens the run's journal and records `run.start` and the entry module.
    fn begin_journal(&mut self, script: &str, source: &str) -> Result<()> {
        let Some(home) = &self.journal_home else {
            return Ok(());
        };
        let mut journal = Journal::create(home)?;
        let seed = random_seed().map_err(|reason| VaernError::Journal {
            stage: "create".into(),
            path: journal.path().display().to_string(),
            reason,
            action: "The system random source failed; retry, or run with --no-journal.".into(),
        })?;
        let digest = crate::journal::sha256_hex(source.as_bytes());
        let mut start = serde_json::Map::new();
        start.insert("vaern".into(), env!("CARGO_PKG_VERSION").into());
        start.insert("script".into(), script.into());
        start.insert("script_sha256".into(), digest.clone().into());
        start.insert("argv".into(), self.options.argv.clone().into());
        let caps = self.budget.borrow().caps();
        start.insert(
            "budget_caps".into(),
            serde_json::json!({ "nano_usd": caps.nano, "tokens": caps.tokens }),
        );
        start.insert(
            "random_seed".into(),
            seed.iter()
                .map(|word| format!("{word:08x}"))
                .collect::<String>()
                .into(),
        );
        journal.append("run.start", start)?;
        self.seed_random(seed)?;
        let mut module = serde_json::Map::new();
        module.insert("path".into(), script.into());
        module.insert("sha256".into(), digest.into());
        journal.append("module.load", module)?;
        *self.journal.borrow_mut() = Some(journal);
        Ok(())
    }

    /// Replaces `Math.random` with the generator in `js/random.js`, seeded
    /// with the four words recorded in `run.start`.
    fn seed_random(&self, seed: [u32; 4]) -> Result<()> {
        self.context
            .with(|ctx| {
                let seed_fn: Function = ctx.globals().get("__vaern_seed_random")?;
                seed_fn.call::<_, ()>((seed[0], seed[1], seed[2], seed[3]))
            })
            .map_err(|err| VaernError::EngineInit {
                reason: format!("failed to seed Math.random: {err}"),
            })
    }

    /// Seals the journal with the run's outcome. A run that succeeded but
    /// cannot be sealed fails, since its record would be incomplete.
    fn seal_journal(&mut self, outcome: Result<i32>) -> Result<i32> {
        let mut slot = self.journal.borrow_mut();
        let Some(journal) = slot.as_mut() else {
            return outcome;
        };
        let sealed = match &outcome {
            Ok(code) => journal.seal(*code, None),
            Err(err) => journal.seal(1, Some(&err.to_string())),
        };
        match (outcome, sealed) {
            (Ok(_), Err(err)) => Err(err),
            (outcome, _) => outcome,
        }
    }

    fn run_file_inner(&mut self, path: &Path) -> Result<i32> {
        let path_str = path.display().to_string();
        if !path.exists() {
            return Err(VaernError::Io {
                path: path_str,
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"),
            });
        }

        let source = FsModuleLoader.load(path)?;
        self.begin_journal(&path_str, &source)?;

        let transpiled = if path.extension().is_some_and(|ext| ext == "ts") {
            self.transpiler.transpile(&path_str, &source)?
        } else {
            source
        };

        let base_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();

        let loader =
            VaernModuleLoader::new(self.transpiler.clone(), &base_dir, Rc::clone(&self.journal));
        self.qjs_runtime.set_loader(
            VaernModuleResolver {
                base_dir,
                failure: Rc::clone(&self.resolve_failure),
            },
            loader,
        );

        let eval_result = self.context.with(|ctx| -> rquickjs::Result<()> {
            let module = Module::declare(ctx.clone(), path_str.clone(), transpiled)?;
            let (_evaluated, promise) = module.eval()?;
            // Check state
            let _ = promise;
            Ok(())
        });

        if let Err(err) = eval_result {
            let msg = self.context.with(|ctx| {
                let exc = ctx.catch();
                // The stack of a parse error or failed import holds only the
                // frame, so the name and message are added to it.
                if exc.as_exception().is_some() {
                    describe_rejection(&exc)
                } else if let Some(s) = exc.as_string() {
                    s.to_string().unwrap_or_else(|_| err.to_string())
                } else {
                    err.to_string()
                }
            });
            if let Some(failure) = self.take_resolve_failure(&msg) {
                return Err(failure);
            }
            return Err(VaernError::Execution {
                file: path_str,
                message: msg,
            });
        }

        self.run_event_loop(&path_str)
    }

    fn eval_inner(&mut self, filename: &str, source: &str) -> Result<i32> {
        self.begin_journal(filename, source)?;
        let transpiled = if filename.ends_with(".ts") {
            self.transpiler.transpile(filename, source)?
        } else {
            source.to_string()
        };

        let base_dir = Path::new(".").to_path_buf();
        let loader =
            VaernModuleLoader::new(self.transpiler.clone(), &base_dir, Rc::clone(&self.journal));
        self.qjs_runtime.set_loader(
            VaernModuleResolver {
                base_dir,
                failure: Rc::clone(&self.resolve_failure),
            },
            loader,
        );

        let eval_result = self.context.with(|ctx| -> rquickjs::Result<()> {
            let module = Module::declare(ctx.clone(), filename.to_string(), transpiled)?;
            let (_evaluated, promise) = module.eval()?;
            let _ = promise;
            Ok(())
        });

        if let Err(err) = eval_result {
            let msg = self.context.with(|ctx| {
                let exc = ctx.catch();
                // The stack of a parse error or failed import holds only the
                // frame, so the name and message are added to it.
                if exc.as_exception().is_some() {
                    describe_rejection(&exc)
                } else if let Some(s) = exc.as_string() {
                    s.to_string().unwrap_or_else(|_| err.to_string())
                } else {
                    err.to_string()
                }
            });
            if let Some(failure) = self.take_resolve_failure(&msg) {
                return Err(failure);
            }
            return Err(VaernError::Execution {
                file: filename.to_string(),
                message: msg,
            });
        }

        self.run_event_loop(filename)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eval_hello() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = "console.log('hello from test');";
        let exit_code = runtime.eval("test.js", code).unwrap();
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_eval_typescript_types() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            interface Item { id: number; }
            const x: Item = { id: 10 };
            console.log(x.id);
        "#;
        let exit_code = runtime.eval("test.ts", code).unwrap();
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_process_exit_code() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = "process.exitCode = 42;";
        let exit_code = runtime.eval("exit.js", code).unwrap();
        assert_eq!(exit_code, 42);
    }

    #[test]
    fn test_uncaught_throw_is_an_error_with_location() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let err = runtime
            .eval("throws.js", "throw new Error('boom');")
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("Error: boom"), "{text}");
        assert!(text.contains("throws.js:1:"), "{text}");
    }

    #[test]
    fn test_unhandled_rejection_is_an_error() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let err = runtime
            .eval("late.js", "Promise.reject(new Error('late'));")
            .unwrap_err();
        assert!(err.to_string().contains("Error: late"));
    }

    #[test]
    fn test_handled_rejection_is_not_an_error() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = "Promise.reject(new Error('handled')).catch(() => {});";
        assert_eq!(runtime.eval("handled.js", code).unwrap(), 0);
    }

    #[test]
    fn test_handling_one_rejection_keeps_another_unhandled() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            Promise.reject(new Error('A unhandled'));
            const b = Promise.reject(new Error('B handled'));
            b.catch(() => {});
        "#;
        let err = runtime.eval("two_rejections.js", code).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("Error: A unhandled"), "{text}");
        assert!(!text.contains("B handled"), "{text}");
    }

    #[test]
    fn test_late_handler_in_same_tick_is_not_an_error() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            const a = Promise.reject(new Error('a'));
            const b = Promise.reject(new Error('b'));
            b.catch(() => {});
            a.catch(() => {});
        "#;
        assert_eq!(runtime.eval("late_handlers.js", code).unwrap(), 0);
    }

    #[test]
    fn test_process_env_reads_host_environment() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        // Cargo sets CARGO_PKG_NAME for every test process.
        let code = "if (process.env.CARGO_PKG_NAME !== 'vaern-core') process.exitCode = 1;";
        assert_eq!(runtime.eval("env.js", code).unwrap(), 0);
    }

    #[test]
    fn test_process_argv() {
        // Mirrors what the CLI passes: `[exec, script, ...args]`, without the
        // `run` subcommand, as in Node.
        let options = RuntimeOptions {
            argv: vec!["vaern".into(), "file.ts".into(), "--flag".into()],
        };
        let mut runtime = QuickJsRuntime::new(options).unwrap();
        let code = r#"
            const [exec, script, flag] = process.argv;
            if (process.argv.length !== 3 || exec !== 'vaern' || script !== 'file.ts' || flag !== '--flag') {
                process.exitCode = 1;
            }
        "#;
        let exit_code = runtime.eval("argv.js", code).unwrap();
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_timers_execution() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            let count = 0;
            setTimeout(() => {
                count += 1;
                if (count === 1) {
                    process.exitCode = 7;
                }
            }, 10);
        "#;
        let exit_code = runtime.eval("timer.js", code).unwrap();
        assert_eq!(exit_code, 7);
    }

    #[test]
    fn test_throwing_timer_is_an_error() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            setTimeout(() => { throw new Error('timer boom'); }, 1);
            setTimeout(() => { process.exitCode = 0; }, 50);
        "#;
        let err = runtime.eval("timer_throw.js", code).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("Error: timer boom"), "{text}");
        assert!(text.contains("timer_throw.js:2:"), "{text}");
    }

    #[test]
    fn test_throwing_interval_is_an_error() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = "setInterval(() => { throw 'tick failed'; }, 1);";
        let err = runtime.eval("interval_throw.js", code).unwrap_err();
        assert!(err.to_string().contains("uncaught value: tick failed"));
    }

    type ChunkChannel = (
        tokio::sync::mpsc::Sender<std::result::Result<bytes::Bytes, std::io::Error>>,
        tokio::sync::mpsc::Receiver<std::result::Result<bytes::Bytes, std::io::Error>>,
    );

    fn stream_test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn test_stream_chunks_wait_for_a_stalled_consumer() {
        let rt = stream_test_runtime();
        let (tx, mut rx): ChunkChannel = tokio::sync::mpsc::channel(STREAM_CHUNK_CAPACITY);
        let total = STREAM_CHUNK_CAPACITY * 4;
        // The consumer starts late, so the buffer is full long before it reads.
        let reader = rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut received = 0;
            while let Some(Ok(chunk)) = rx.recv().await {
                assert_eq!(chunk.as_ref(), &[received as u8]);
                received += 1;
            }
            received
        });
        for i in 0..total {
            let chunk = bytes::Bytes::from(vec![i as u8]);
            send_stream_chunk(rt.handle(), &tx, 1, chunk, Duration::from_secs(5)).unwrap();
        }
        drop(tx);
        assert_eq!(rt.block_on(reader).unwrap(), total);
    }

    #[test]
    fn test_stream_chunk_to_closed_connection_is_an_error() {
        let rt = stream_test_runtime();
        let (tx, rx): ChunkChannel = tokio::sync::mpsc::channel(STREAM_CHUNK_CAPACITY);
        drop(rx);
        let err = send_stream_chunk(
            rt.handle(),
            &tx,
            7,
            bytes::Bytes::from_static(b"x"),
            Duration::from_secs(5),
        )
        .unwrap_err();
        match err {
            VaernError::ServerStream {
                req_id, message, ..
            } => {
                assert_eq!(req_id, 7);
                assert!(message.contains("closed the connection"), "{message}");
            }
            other => panic!("expected ServerStream, got {other}"),
        }
    }

    #[test]
    fn test_stream_chunk_to_stalled_client_times_out_and_aborts_body() {
        let rt = stream_test_runtime();
        let (tx, mut rx): ChunkChannel = tokio::sync::mpsc::channel(1);
        tx.try_send(Ok(bytes::Bytes::from_static(b"full"))).unwrap();
        let err = send_stream_chunk(
            rt.handle(),
            &tx,
            9,
            bytes::Bytes::from_static(b"x"),
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(
            matches!(err, VaernError::ServerStream { req_id: 9, .. }),
            "{err}"
        );
        // The connection must end with an error frame, not a clean short body.
        let frames = rt.block_on(async {
            let first = rx.recv().await;
            let second = rx.recv().await;
            (first, second)
        });
        assert!(matches!(frames.0, Some(Ok(_))));
        assert!(matches!(frames.1, Some(Err(_))));
    }

    #[test]
    fn test_stream_burst_larger_than_buffer_is_delivered_in_full() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        // 512 chunks enqueued synchronously reach the bridge in one microtask
        // drain, sixteen times the channel capacity.
        let code = r#"
            import { serve } from "vaern:http";
            const CHUNKS = 512;
            const CHUNK = "x".repeat(1023) + "\n";
            const server = serve(() => {
                const stream = new ReadableStream({
                    start(controller) {
                        for (let i = 0; i < CHUNKS; i++) {
                            controller.enqueue(new TextEncoder().encode(CHUNK));
                        }
                        controller.close();
                    },
                });
                return new Response(stream);
            }, { port: 8997, hostname: "127.0.0.1" });
            const res = await fetch("http://127.0.0.1:8997/burst");
            const body = await res.text();
            await server.close();
            if (body.length !== CHUNKS * CHUNK.length) {
                console.error(`received ${body.length} bytes`);
                process.exitCode = 1;
            }
        "#;
        assert_eq!(runtime.eval("burst.ts", code).unwrap(), 0);
    }

    #[test]
    fn test_stalled_stream_client_does_not_block_timers() {
        // Connects, sends one request, then reads nothing for 4 s. Filling the
        // buffers between the pump and the socket costs the JS thread some
        // CPU up front; after that, the old blocking send froze the thread
        // until the client went away.
        let client = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            let mut stream = std::net::TcpStream::connect("127.0.0.1:8996").unwrap();
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: t\r\n\r\n")
                .unwrap();
            std::thread::sleep(Duration::from_millis(4000));
        });
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            import { serve } from "vaern:http";
            let last = Date.now();
            let maxGap = 0;
            const ticker = setInterval(() => {
                const now = Date.now();
                maxGap = Math.max(maxGap, now - last);
                last = now;
            }, 10);
            const chunk = new Uint8Array(64 * 1024);
            const server = serve(() => new Response(new ReadableStream({
                start(controller) {
                    for (let i = 0; i < 512; i++) controller.enqueue(chunk);
                    controller.close();
                },
            })), { port: 8996, hostname: "127.0.0.1" });
            setTimeout(async () => {
                // Measured here too: after a freeze this callback can run
                // before the ticker does.
                maxGap = Math.max(maxGap, Date.now() - last);
                clearInterval(ticker);
                await server.close();
                if (maxGap > 2000) {
                    console.error(`timers stalled for ${maxGap} ms`);
                    process.exitCode = 1;
                }
            }, 3000);
        "#;
        let exit_code = runtime.eval("stalled_stream.ts", code).unwrap();
        client.join().unwrap();
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_streaming_a_queued_body_to_a_fast_client_lets_timers_run() {
        // All 32 MiB is queued up front and the client keeps up, so the pump
        // never waits for buffer room; without yielding it would hold the
        // thread for the whole body (about 3 s in a debug build).
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            import { serve } from "vaern:http";
            let last = Date.now();
            let maxGap = 0;
            const ticker = setInterval(() => {
                const now = Date.now();
                maxGap = Math.max(maxGap, now - last);
                last = now;
            }, 10);
            const chunk = new Uint8Array(64 * 1024);
            const server = serve(() => new Response(new ReadableStream({
                start(c) {
                    for (let i = 0; i < 512; i++) c.enqueue(chunk);
                    c.close();
                },
            })), { port: 8994, hostname: "127.0.0.1" });
            const res = await fetch("http://127.0.0.1:8994/");
            const body = await res.arrayBuffer();
            maxGap = Math.max(maxGap, Date.now() - last);
            clearInterval(ticker);
            await server.close();
            if (body.byteLength !== 512 * 64 * 1024 || maxGap > 1000) {
                console.error(`${body.byteLength} bytes, timers stalled for ${maxGap} ms`);
                process.exitCode = 1;
            }
        "#;
        assert_eq!(runtime.eval("fast_stream.ts", code).unwrap(), 0);
    }

    #[test]
    fn test_eval_fetch_double_read_rejection() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            const res = new Response("hello world");
            const t1 = await res.text();
            if (t1 !== "hello world") {
                process.exitCode = 1;
            }
            try {
                await res.text();
                process.exitCode = 2;
            } catch (err) {
                if (err instanceof TypeError) {
                    process.exitCode = 0;
                } else {
                    process.exitCode = 3;
                }
            }
        "#;
        let exit_code = runtime.eval("double_read.js", code).unwrap();
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_eval_fetch_invalid_json() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            const res = new Response("not valid json at all");
            try {
                await res.json();
                process.exitCode = 1;
            } catch (err) {
                if (err instanceof SyntaxError) {
                    process.exitCode = 0;
                } else {
                    process.exitCode = 2;
                }
            }
        "#;
        let exit_code = runtime.eval("invalid_json.js", code).unwrap();
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_eval_fetch_url_parsing_error() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            try {
                await fetch("not-a-valid-url");
                process.exitCode = 1;
            } catch (err) {
                if (err && err.stage === "dns") {
                    process.exitCode = 0;
                } else {
                    process.exitCode = 2;
                }
            }
        "#;
        let exit_code = runtime.eval("url_err.js", code).unwrap();
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_eval_request_double_read_rejection() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            const req = new Request("http://127.0.0.1:8000/test", {
                method: "POST",
                body: "hello payload",
            });
            const t1 = await req.text();
            if (t1 !== "hello payload") {
                process.exitCode = 1;
            }
            try {
                await req.text();
                process.exitCode = 2;
            } catch (err) {
                if (err instanceof TypeError && err.message.includes("consumed")) {
                    process.exitCode = 0;
                } else {
                    process.exitCode = 3;
                }
            }
        "#;
        let exit_code = runtime.eval("req_double_read.js", code).unwrap();
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_heap_limit_stops_runaway_allocation() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        // 384 MiB of live buffers, past the 256 MiB heap limit.
        let code = r#"
            const held = [];
            for (let i = 0; i < 384; i++) held.push(new Uint8Array(1024 * 1024));
            console.log("allocated", held.length);
        "#;
        let err = runtime.eval("hog.js", code).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("out of memory"), "{text}");
        assert!(text.contains("hog.js"), "{text}");
    }

    #[test]
    fn test_idle_server_parks_until_work_arrives() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            import { serve } from "vaern:http";
            const server = serve(() => new Response("ok"), { port: 0, hostname: "127.0.0.1" });
            setTimeout(() => server.close(), 300);
        "#;
        let exit_code = runtime.eval("idle_server.ts", code).unwrap();
        assert_eq!(exit_code, 0);
        // A loop that ticks while a server is open wakes hundreds of times in
        // 300 ms. Parking on the event channel wakes for the timer and the
        // close event only.
        let parks = runtime.parks.get();
        assert!(
            parks < 10,
            "event loop parked {parks} times for 300 ms of idle server"
        );
    }

    #[test]
    fn test_timer_wait_is_one_park() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let started = Instant::now();
        let exit_code = runtime
            .eval("one_timer.js", "setTimeout(() => {}, 200);")
            .unwrap();
        assert_eq!(exit_code, 0);
        assert!(started.elapsed() >= Duration::from_millis(200));
        let parks = runtime.parks.get();
        assert!(
            parks <= 2,
            "event loop parked {parks} times for one 200 ms timer"
        );
    }

    #[test]
    fn test_eval_serve_port_in_use() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            import { serve } from "vaern:http";
            const s1 = serve(() => new Response("ok"), { port: 8999, hostname: "127.0.0.1" });
            try {
                const s2 = serve(() => new Response("conflict"), { port: 8999, hostname: "127.0.0.1" });
                process.exitCode = 1;
            } catch (err) {
                if (err && String(err).includes("failed to bind TCP listener")) {
                    process.exitCode = 0;
                } else {
                    console.error("unexpected error:", err);
                    process.exitCode = 2;
                }
            }
            await s1.close();
        "#;
        let exit_code = runtime.eval("port_conflict.ts", code).unwrap();
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_eval_serve_invalid_hostname() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            import { serve } from "vaern:http";
            try {
                const s = serve(() => new Response("ok"), { port: 8998, hostname: "invalid:::hostname" });
                process.exitCode = 1;
            } catch (err) {
                if (err && String(err).includes("invalid hostname")) {
                    process.exitCode = 0;
                } else {
                    process.exitCode = 2;
                }
            }
        "#;
        let exit_code = runtime.eval("invalid_host.ts", code).unwrap();
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_stream_text_keeps_code_points_split_across_chunks() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            const expected = "hé€\u{1F600}!";
            const bytes = new TextEncoder().encode(expected);
            const body = new ReadableStream({
                start(c) {
                    for (const b of bytes) c.enqueue(new Uint8Array([b]));
                    c.close();
                }
            });
            const text = await new Response(body).text();
            process.exitCode = text === expected ? 0 : 1;
        "#;
        assert_eq!(runtime.eval("split_utf8.js", code).unwrap(), 0);
    }

    #[test]
    fn test_stream_array_buffer_keeps_binary_bytes() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            const body = new ReadableStream({
                start(c) {
                    c.enqueue(new Uint8Array([0xff, 0x00]));
                    c.enqueue(new Uint8Array([0xc3]));
                    c.close();
                }
            });
            const bytes = new Uint8Array(await new Response(body).arrayBuffer());
            process.exitCode = bytes.join(",") === "255,0,195" ? 0 : 1;
        "#;
        assert_eq!(runtime.eval("binary_body.js", code).unwrap(), 0);
    }

    #[test]
    fn test_served_stream_carries_every_byte_value_and_view_offsets() {
        // Chunks cross the bridge as byte strings; every value 0x00..0xFF must
        // survive, and a view must contribute only its own window.
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            import { serve } from "vaern:http";
            const all = new Uint8Array(256 + 32);
            for (let i = 0; i < all.length; i++) all[i] = (i - 16) & 0xff;
            const server = serve(() => new Response(new ReadableStream({
                start(c) {
                    c.enqueue(all.subarray(16, 16 + 256));
                    c.enqueue("é\u{1F600}");
                    c.close();
                },
            })), { port: 8995, hostname: "127.0.0.1" });
            const res = await fetch("http://127.0.0.1:8995/");
            const got = new Uint8Array(await res.arrayBuffer());
            await server.close();
            const want = [...Array(256).keys(), ...new TextEncoder().encode("é\u{1F600}")];
            const same = got.length === want.length && want.every((b, i) => got[i] === b);
            if (!same) console.error(`got ${got.length} bytes: ${got.slice(0, 8).join(",")}...`);
            process.exitCode = same ? 0 : 1;
        "#;
        assert_eq!(runtime.eval("byte_values.js", code).unwrap(), 0);
    }

    #[test]
    fn test_console_formats_errors_with_message_and_stack() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            const out = __formatArg(new Error("boom", { cause: new TypeError("inner") }));
            const lines = out.split("\n");
            const nested = __formatArg({ err: new RangeError("deep"), n: 1n });
            process.exitCode =
                (lines[0] === "Error: boom" ? 0 : 1) |
                (lines[1].includes("format.js:2:") ? 0 : 2) |
                (out.includes("Caused by: TypeError: inner") ? 0 : 4) |
                (nested === '{"err":"RangeError: deep","n":"1n"}' ? 0 : 8);
        "#;
        assert_eq!(runtime.eval("format.js", code).unwrap(), 0);
    }

    #[test]
    fn test_text_codec_follows_whatwg_utf8() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            const replaced = new TextDecoder().decode(new Uint8Array([0xff, 0x41])) === "�A";
            const bom = new TextDecoder().decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x42])) === "B";
            let fatal = false;
            try {
                new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array([0xc3]));
            } catch (e) {
                fatal = e instanceof TypeError;
            }
            const surrogate = new TextEncoder().encode("a\ud800").join(",") === "97,239,191,189";
            process.exitCode = (replaced ? 0 : 1) | (bom ? 0 : 2) | (fatal ? 0 : 4) | (surrogate ? 0 : 8);
        "#;
        assert_eq!(runtime.eval("codec.js", code).unwrap(), 0);
    }

    #[test]
    fn test_text_decoder_converts_arguments_as_webidl_does() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            const throwsType = (f) => { try { f(); return false; } catch (e) { return e instanceof TypeError; } };
            const decoder = new TextDecoder("utf-8", null);
            const bytes = new Uint8Array([0x68, 0x69]);
            process.exitCode =
                (decoder.fatal === false && decoder.decode(bytes, null) === "hi" ? 0 : 1) |
                (decoder.decode() === "" && decoder.decode(undefined) === "" ? 0 : 2) |
                (throwsType(() => decoder.decode(null)) ? 0 : 4) |
                (throwsType(() => new TextDecoder("utf-8", 5)) ? 0 : 8) |
                (throwsType(() => decoder.decode(bytes, "stream")) ? 0 : 16);
        "#;
        assert_eq!(runtime.eval("decoder_args.js", code).unwrap(), 0);
    }

    #[test]
    fn test_url_follows_whatwg_parser() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            const auth = new URL("https://user:pw@example.com:8443/a/../b");
            const v6 = new URL("http://[::1]:3000/x");
            const rel = new URL("../up?q=a b", "https://example.com/dir/sub/page");
            const u = new URL("https://example.com/?tag=a");
            u.searchParams.append("q", "x y");
            process.exitCode =
                (auth.username === "user" && auth.password === "pw" && auth.pathname === "/b" ? 0 : 1) |
                (v6.hostname === "[::1]" && v6.port === "3000" ? 0 : 2) |
                (rel.href === "https://example.com/dir/up?q=a%20b" ? 0 : 4) |
                (u.href === "https://example.com/?tag=a&q=x+y" ? 0 : 8);
        "#;
        assert_eq!(runtime.eval("url.js", code).unwrap(), 0);
    }

    #[test]
    fn test_fetch_rejects_unsupported_redirect_modes() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            let rejected = 0;
            for (const redirect of ["manual", "error"]) {
                try {
                    await fetch("http://127.0.0.1:9/", { redirect });
                } catch (err) {
                    if (err instanceof TypeError && err.message.includes(redirect)) rejected++;
                }
            }
            process.exitCode = rejected === 2 ? 0 : 1;
        "#;
        assert_eq!(runtime.eval("redirect_mode.js", code).unwrap(), 0);
    }

    #[test]
    fn test_runtime_managed_headers_throw_type_error() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = r#"
            import { serve } from "vaern:http";
            let fails = 0;
            for (const name of ["Host", "Content-Length", "transfer-encoding", "connection", "Upgrade", "TE", "trailer"]) {
                try {
                    await fetch("http://127.0.0.1:9/", { headers: { [name]: "1" } });
                    fails |= 1;
                } catch (err) {
                    if (!(err instanceof TypeError) || !err.message.includes(`'${name.toLowerCase()}'`)) fails |= 2;
                }
            }
            const server = serve(
                () => new Response("hi", { headers: { "Content-Length": "999" } }),
                { port: 0, hostname: "127.0.0.1" },
            );
            const res = await fetch(`http://127.0.0.1:${server.addr.port}/`);
            if (res.status !== 500) fails |= 4;
            const ok = serve(
                () => new Response("hi", { headers: { "x-kept": "yes" } }),
                { port: 0, hostname: "127.0.0.1" },
            );
            const kept = await fetch(`http://127.0.0.1:${ok.addr.port}/`, { headers: { "x-sent": "1" } });
            if (kept.status !== 200 || kept.headers.get("x-kept") !== "yes") fails |= 8;
            await server.close();
            await ok.close();
            process.exitCode = fails;
        "#;
        assert_eq!(runtime.eval("managed_headers.js", code).unwrap(), 0);
    }

    #[test]
    fn test_fetches_from_one_runtime_share_a_connection() {
        let (addr, accepted) = crate::test_tls::start_counting_http_server().unwrap();
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = format!(
            r#"
            for (let i = 0; i < 3; i++) {{
                const res = await fetch("http://{addr}/");
                if ((await res.text()) !== "ok") process.exitCode = 1;
            }}
        "#
        );
        assert_eq!(runtime.eval("pool.js", &code).unwrap(), 0);
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn test_vaern_fs_reads_and_writes_text_and_bytes() {
        let dir = std::env::temp_dir().join(format!("vaern-rt-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir_js = dir.display().to_string().replace('\\', "/");
        let code = format!(
            r#"
            import fs, {{ readFile, writeFile, appendFile }} from "vaern:fs";
            const dir = "{dir_js}";
            let fails = 0;
            await writeFile(dir + "/t.txt", "café \u{{1F600}}\n");
            await appendFile(dir + "/t.txt", "second");
            if ((await readFile(dir + "/t.txt", "utf8")) !== "café \u{{1F600}}\nsecond") fails |= 1;
            if ((await fs.readFile(dir + "/t.txt", {{ encoding: "utf-8" }})).length !== 14) fails |= 2;

            await writeFile(dir + "/b.bin", new Uint8Array([0, 128, 255]));
            await appendFile(dir + "/b.bin", new Uint8Array([1, 2, 3]).buffer);
            const bytes = await readFile(dir + "/b.bin");
            if (!(bytes instanceof Uint8Array) || bytes.join() !== "0,128,255,1,2,3") fails |= 4;

            try {{
                await writeFile(dir + "/t.txt", "x", {{ flag: "wx" }});
                fails |= 8;
            }} catch (err) {{
                if (err.name !== "FsError" || err.code !== "EEXIST" || err.stage !== "open") fails |= 8;
            }}
            try {{
                await readFile(dir + "/missing.txt", "utf8");
                fails |= 16;
            }} catch (err) {{
                if (err.code !== "ENOENT" || !err.path.endsWith("missing.txt") || !err.message.includes("ENOENT")) fails |= 16;
            }}
            for (const call of [() => readFile(42), () => readFile("x", "latin1"), () => writeFile("x", 5), () => writeFile("x", "", {{ flag: "a" }})]) {{
                try {{
                    await call();
                    fails |= 32;
                }} catch (err) {{
                    if (!(err instanceof TypeError)) fails |= 32;
                }}
            }}
            process.exitCode = fails;
        "#
        );
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let exit_code = runtime.eval("fs.js", &code).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_failed_import_reports_the_full_typed_resolution_error() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let err = runtime
            .eval("missing.js", "import 'vaern-absent-package-for-tests';")
            .unwrap_err();
        match err {
            VaernError::ModuleResolution {
                specifier, tried, ..
            } => {
                assert_eq!(specifier, "vaern-absent-package-for-tests");
                let cwd = std::env::current_dir().unwrap();
                assert_eq!(tried.len(), cwd.ancestors().count());
                assert!(
                    tried
                        .iter()
                        .all(|t| t.ends_with("vaern-absent-package-for-tests"))
                );
            }
            other => panic!("expected a resolution error, got {other}"),
        }
    }

    #[test]
    fn test_oversized_module_source_is_refused_on_import_and_as_entry() {
        let root = tempfile::tempdir().unwrap();
        let big = root.path().join("big.js");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(crate::module_loader::MAX_SOURCE_BYTES + 1)
            .unwrap();
        let main = root.path().join("main.js");
        std::fs::write(&main, "import './big.js';\n").unwrap();

        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let text = runtime.run_file(&main).unwrap_err().to_string();
        assert!(text.contains("64 MiB module size limit"), "{text}");
        assert!(text.contains("big.js"), "{text}");

        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        match runtime.run_file(&big) {
            Err(VaernError::SourceTooLarge { .. }) => {}
            Err(other) => panic!("expected SourceTooLarge, got {other}"),
            Ok(code) => panic!("expected SourceTooLarge, ran with status {code}"),
        }
    }

    fn journal_records(home: &Path) -> Vec<serde_json::Value> {
        let id = std::fs::read_to_string(home.join("runs/latest")).unwrap();
        let path = home.join("runs").join(id).join("journal.jsonl");
        assert!(crate::journal::verify(&path).unwrap().sealed);
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["r"].clone())
            .collect()
    }

    #[test]
    fn test_run_records_start_modules_transcript_and_end() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        std::fs::write(root.path().join("dep.js"), "export const n = 2;\n").unwrap();
        let main = root.path().join("main.js");
        let source = "import { n } from './dep.js';\nconsole.log('n =', n);\nconsole.error('é');\n";
        std::fs::write(&main, source).unwrap();

        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        assert_eq!(runtime.run_file(&main).unwrap(), 0);

        let records = journal_records(&home);
        let kinds: Vec<&str> = records
            .iter()
            .map(|r| r["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "run.start",
                "module.load",
                "module.load",
                "console",
                "console",
                "run.end"
            ]
        );
        assert_eq!(
            records[0]["script_sha256"],
            crate::journal::sha256_hex(source.as_bytes())
        );
        assert!(records[2]["path"].as_str().unwrap().ends_with("dep.js"));
        assert_eq!(
            (records[3]["fd"].as_u64(), records[3]["text"].as_str()),
            (Some(1), Some("n = 2\n"))
        );
        assert_eq!(
            (records[4]["fd"].as_u64(), records[4]["text"].as_str()),
            (Some(2), Some("é\n"))
        );

        let end = &records[5];
        assert_eq!(end["exit_code"], 0);
        assert_eq!(end["records"], 6);
        // The documented transcript construction: <fd>:<byte length>:<text>.
        let transcript = "1:6:n = 2\n2:3:é\n";
        assert_eq!(
            end["transcript_sha256"],
            crate::journal::sha256_hex(transcript.as_bytes())
        );
    }

    #[test]
    fn test_fetch_and_fs_effects_are_journalled_with_secrets_redacted() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let data = root.path().join("data.txt");
        let missing = root.path().join("missing.txt");
        let code = format!(
            r#"
            import {{ serve }} from "vaern:http";
            import {{ writeFile, readFile }} from "vaern:fs";
            const server = serve(async (req) => new Response("pong:" + await req.text(),
                {{ headers: {{ "set-cookie": "sid=secret-cookie" }} }}),
                {{ port: 8993, hostname: "127.0.0.1" }});
            const res = await fetch("http://127.0.0.1:8993/ping", {{
                method: "POST",
                headers: {{ authorization: "Bearer sk-secret-token", "x-trace": "t1" }},
                body: new Uint8Array([0xff, 0x01]),
            }});
            await res.text();
            await server.close();
            await writeFile({data:?}, "hello");
            await readFile({data:?}, "utf8");
            await readFile({missing:?}).catch(() => {{}});
            "#,
            data = data.display().to_string(),
            missing = missing.display().to_string(),
        );
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        assert_eq!(runtime.eval("effects.js", &code).unwrap(), 0);

        let records = journal_records(&home);
        let effects: Vec<&serde_json::Value> = records
            .iter()
            .filter(|r| r["kind"] == "fetch" || r["kind"] == "fs")
            .collect();
        assert_eq!(effects.len(), 4, "{records:#?}");
        let eids: Vec<u64> = effects.iter().map(|r| r["eid"].as_u64().unwrap()).collect();
        assert_eq!(eids, [0, 1, 2, 3]);

        let fetch = effects[0];
        assert_eq!(fetch["kind"], "fetch");
        assert_eq!(fetch["req"]["method"], "POST");
        assert_eq!(fetch["req"]["url"], "http://127.0.0.1:8993/ping");
        assert_eq!(fetch["req"]["body"], serde_json::json!({ "b64": "/wE=" }));
        assert_eq!(fetch["res"]["status"], 200);
        assert_eq!(fetch["res"]["body"]["b64"], serde_json::Value::Null);
        assert!(
            fetch["res"]["body"]["utf8"]
                .as_str()
                .unwrap()
                .starts_with("pong:")
        );
        assert!(fetch["dur_us"].as_u64().is_some());
        let line = serde_json::to_string(&records).unwrap();
        assert!(!line.contains("sk-secret-token"), "authorization leaked");
        assert!(!line.contains("secret-cookie"), "set-cookie leaked");
        assert!(line.contains(r#"["x-trace","t1"]"#), "{line}");

        assert_eq!(effects[1]["req"]["op"], "write");
        assert_eq!(effects[1]["req"]["len"], 5);
        assert_eq!(
            effects[1]["req"]["sha256"],
            crate::journal::sha256_hex(b"hello")
        );
        assert_eq!(effects[2]["req"]["op"], "read_utf8");
        assert_eq!(effects[2]["res"]["text"], "hello");
        assert_eq!(effects[3]["error"]["code"], "ENOENT");
    }

    /// An OpenAI-compatible stub on `port`. `/ok/` answers with usage,
    /// `/nousage/` without it, `/fail/` with a 500, `/junk/` with text, and
    /// `/echo/` with the first message's content as the reply. `/sse/`
    /// streams that content word by word, and `/sse-cut/` stops before the
    /// stream is finished. `/agent/` plays a tool-calling model: it calls a
    /// tool chosen by the user message until a tool result comes back, and
    /// for "loop" and "bad" it never stops calling.
    fn model_stub(port: u16) -> String {
        format!(
            r#"
            import {{ serve }} from "vaern:http";
            globalThis.seen = [];
            const stub = serve(async (req) => {{
                const body = await req.json();
                seen.push({{ url: req.url, auth: req.headers.get("authorization"), body }});
                if (req.url.includes("/fail/")) return new Response("overloaded", {{ status: 500 }});
                if (req.url.includes("/junk/")) return new Response("<html>");
                if (req.url.includes("/agent/")) {{
                    const last = body.messages[body.messages.length - 1];
                    const ask = body.messages.find((m) => m.role === "user").content;
                    const callTool = (name, args) => ({{ choices: [{{ finish_reason: "tool_calls",
                        message: {{ role: "assistant", content: null, tool_calls: [{{
                            id: "c" + body.messages.length, type: "function",
                            function: {{ name, arguments: args }} }}] }} }}] }});
                    let reply;
                    if (last.role === "tool" && ask !== "loop" && ask !== "bad") {{
                        reply = {{ choices: [{{ finish_reason: "stop",
                            message: {{ role: "assistant", content: "result: " + last.content }} }}] }};
                    }} else if (ask === "bad") reply = callTool("add", '{{"a":"x","b":1}}');
                    else if (ask === "unknown") reply = callTool("nosuch", "{{}}");
                    else if (ask === "boom") reply = callTool("boom", "{{}}");
                    else reply = callTool("add", '{{"a":2,"b":3}}');
                    reply.usage = {{ prompt_tokens: 10, completion_tokens: 2 }};
                    return new Response(JSON.stringify(reply),
                        {{ headers: {{ "content-type": "application/json" }} }});
                }}
                if (req.url.includes("/sse")) {{
                    const words = body.messages[0].content.split(" ");
                    const event = (data) => "data: " + JSON.stringify(data) + "\n\n";
                    let sse = words.map((w, i) =>
                        event({{ choices: [{{ delta: {{ content: (i ? " " : "") + w }} }}] }})).join("");
                    if (!req.url.includes("/sse-cut/")) {{
                        sse += event({{ choices: [{{ delta: {{}}, finish_reason: "stop" }}] }});
                        sse += event({{ choices: [], usage: {{ prompt_tokens: 4, completion_tokens: words.length }} }});
                        sse += "data: [DONE]\n\n";
                    }}
                    return new Response(sse, {{ headers: {{ "content-type": "text/event-stream" }} }});
                }}
                const content = req.url.includes("/echo/")
                    ? body.messages[0].content : "hi " + body.messages[0].content;
                const reply = {{
                    choices: [{{ message: {{ role: "assistant", content }},
                                 finish_reason: "stop" }}],
                }};
                if (req.url.includes("/ok/")) reply.usage = {{ prompt_tokens: 20, completion_tokens: 5 }};
                return new Response(JSON.stringify(reply),
                    {{ headers: {{ "content-type": "application/json" }} }});
            }}, {{ port: {port}, hostname: "127.0.0.1" }});
            const base = "http://127.0.0.1:{port}";
            "#
        )
    }

    #[test]
    fn test_model_chat_is_metered_and_journalled_without_the_key() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let code = model_stub(8992)
            + r#"
            import { model, ModelError } from "vaern:ai";
            const m = model({
                baseUrl: base + "/ok/v1/", model: "stub-1", apiKey: "sk-model-secret",
                price: { inputPerMTok: 0.15, outputPerMTok: 0.6 }, maxOutputTokens: 64,
            });
            const r = await m.chat({ messages: [{ role: "user", content: "there" }] });
            if (r.text !== "hi there" || r.finishReason !== "stop") throw new Error(JSON.stringify(r));
            if (r.usage.inputTokens !== 20 || r.usage.outputTokens !== 5) throw new Error("usage");
            // 20 x 150 + 5 x 600 nano-USD.
            if (r.costUsd !== 6000 / 1e9) throw new Error("cost " + r.costUsd);
            const sent = seen[0];
            if (!sent.url.endsWith("/ok/v1/chat/completions")) throw new Error(sent.url);
            if (sent.auth !== "Bearer sk-model-secret") throw new Error("auth " + sent.auth);
            if (sent.body.model !== "stub-1" || sent.body.max_tokens !== 64) throw new Error("body");
            // Refused before dispatch: nothing is sent and nothing journalled.
            const refused = await m.chat({ messages: [] }).catch((e) => e);
            if (!(refused instanceof ModelError) || refused.stage !== "config") throw refused;
            if (seen.length !== 1) throw new Error("a refused call was sent");
            await stub.close();
            "#;
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        assert_eq!(runtime.eval("model.js", &code).unwrap(), 0);

        let records = journal_records(&home);
        let calls: Vec<&serde_json::Value> = records
            .iter()
            .filter(|r| r["kind"] == "model.call")
            .collect();
        assert_eq!(calls.len(), 1, "{records:#?}");
        let call = calls[0];
        assert_eq!(call["req"]["base_url"], "http://127.0.0.1:8992/ok/v1/");
        assert_eq!(call["req"]["body"]["max_tokens"], 64);
        assert_eq!(call["res"]["status"], 200);
        assert_eq!(call["res"]["usage"]["input_tokens"], 20);
        assert_eq!(call["res"]["charged_nano_usd"], 6000);
        assert!(call["res"]["body"].as_str().unwrap().contains("hi there"));
        let line = serde_json::to_string(&records).unwrap();
        assert!(!line.contains("sk-model-secret"), "the API key leaked");
    }

    #[test]
    fn test_schema_replies_are_parsed_checked_and_still_charged_on_mismatch() {
        let code = model_stub(8990)
            + r#"
            import { model, ModelError } from "vaern:ai";
            const m = model({ baseUrl: base + "/echo/v1", model: "m",
                              price: { inputPerMTok: 1, outputPerMTok: 1 } });
            const schema = { type: "object", properties: { n: { type: "integer" } },
                             required: ["n"], additionalProperties: false };
            const say = (content, s = schema) => m.chat({ messages: [{ role: "user", content }], schema: s });

            const ok = await say('{"n": 3}');
            if (ok.json?.n !== 3 || ok.text !== '{"n": 3}') throw new Error(JSON.stringify(ok));
            const format = seen[0].body.response_format;
            if (format.type !== "json_schema" || format.json_schema.name !== "reply"
                || format.json_schema.strict !== true || format.json_schema.schema.required[0] !== "n") {
                throw new Error(JSON.stringify(format));
            }
            const plain = await m.chat({ messages: [{ role: "user", content: "x" }] });
            if ("json" in plain) throw new Error("json without a schema");

            const wrong = await say('{"n": "3"}').catch((e) => e);
            if (!(wrong instanceof ModelError) || wrong.stage !== "schema" || wrong.path !== "$.n"
                || !(wrong.costUsd > 0)) throw wrong;
            const prose = await say("sure, here it is").catch((e) => e);
            if (prose.stage !== "schema" || prose.path !== "$") throw prose;

            const sent = seen.length;
            const refused = await say("{}", { type: "string", pattern: "x" }).catch((e) => e);
            if (refused.stage !== "config" || !refused.message.includes("pattern")) throw refused;
            if (seen.length !== sent) throw new Error("a refused schema was sent");
            await stub.close();
            console.log("schema checked");
            "#;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        assert_eq!(runtime.eval("schema.js", &code).unwrap(), 0);
        let records = journal_records(&home);
        let last_console = records.iter().rfind(|r| r["kind"] == "console").unwrap();
        assert_eq!(last_console["text"], "schema checked\n");
        // Mismatched replies are journalled as completed calls.
        let calls = records.iter().filter(|r| r["kind"] == "model.call");
        assert!(calls.clone().all(|r| r["res"]["status"] == 200));
        assert_eq!(calls.count(), 4);
    }

    #[test]
    fn test_streamed_replies_arrive_in_order_and_are_journalled_as_received() {
        let code = model_stub(8989)
            + r#"
            import { model, ModelError } from "vaern:ai";
            // 1 USD per million tokens is 1000 nano-USD per token.
            const m = model({ baseUrl: base + "/sse/v1", model: "m",
                              price: { inputPerMTok: 1, outputPerMTok: 1 } });
            const ask = (content) => ({ messages: [{ role: "user", content }] });

            const lazy = m.stream(ask("never read"));
            await new Promise((r) => setTimeout(r, 20));
            if (seen.length !== 0) throw new Error("an unread stream was sent");

            const events = [];
            for await (const ev of m.stream(ask("one two three"))) events.push(ev);
            const deltas = events.filter((e) => !e.done).map((e) => e.delta);
            if (deltas.join("|") !== "one| two| three") throw new Error(deltas.join("|"));
            const last = events[events.length - 1];
            if (!last.done || last.text !== "one two three" || last.finishReason !== "stop"
                || last.usage.inputTokens !== 4 || last.usage.outputTokens !== 3
                || last.costUsd !== 7000 / 1e9) throw new Error(JSON.stringify(last));
            const sent = seen[0].body;
            if (sent.stream !== true || sent.stream_options?.include_usage !== true) {
                throw new Error(JSON.stringify(sent));
            }

            for await (const ev of m.stream(ask("a b c"))) break;

            const cut = model({ baseUrl: base + "/sse-cut/v1", model: "m" });
            let failure = null;
            try { for await (const ev of cut.stream(ask("x y"))) {} } catch (e) { failure = e; }
            if (!(failure instanceof ModelError) || failure.stage !== "parse") throw failure;

            const refused = await m.stream({ ...ask("x"), schema: { type: "string" } }).next()
                .catch((e) => e);
            if (refused.stage !== "config") throw refused;
            await stub.close();
            console.log("streams checked");
            "#;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        assert_eq!(runtime.eval("stream.js", &code).unwrap(), 0);
        let records = journal_records(&home);
        let last_console = records.iter().rfind(|r| r["kind"] == "console").unwrap();
        assert_eq!(last_console["text"], "streams checked\n");
        let calls: Vec<&serde_json::Value> = records
            .iter()
            .filter(|r| r["kind"] == "model.call")
            .collect();
        // The stream left early still completed and was recorded.
        assert_eq!(calls.len(), 3, "{records:#?}");
        let full = calls[0]["res"]["body"].as_str().unwrap();
        assert!(full.starts_with("data: {"), "{full}");
        assert!(full.ends_with("data: [DONE]\n\n"), "{full}");
        assert_eq!(calls[0]["res"]["charged_nano_usd"], 7000);
        // Records are written when a call completes, and the stream left
        // early is still in flight when the cut one starts, so either can be
        // recorded first. Exactly one of the three failed, at the parse stage.
        let failed: Vec<&str> = calls
            .iter()
            .filter(|call| call["error"].is_object())
            .map(|call| call["error"]["stage"].as_str().unwrap_or("none"))
            .collect();
        assert_eq!(failed, ["parse"], "{calls:#?}");
    }

    #[test]
    fn test_tool_calls_check_arguments_and_journal_start_and_result() {
        let code = r#"
            import { tool, ToolError } from "vaern:ai";
            const add = tool({
                name: "add", description: "Adds two numbers.",
                parameters: { type: "object", properties: { a: { type: "number" }, b: { type: "number" } },
                              required: ["a", "b"], additionalProperties: false },
                run: async ({ a, b }) => ({ sum: a + b }),
            });
            const r = await add.call({ a: 2, b: 3 });
            if (r.sum !== 5) throw new Error(JSON.stringify(r));

            const bad = await add.call({ a: "2", b: 3 }).catch((e) => e);
            if (!(bad instanceof ToolError) || bad.stage !== "args" || bad.path !== "$.a") throw bad;

            const boom = tool({ name: "boom", run: async () => { throw new RangeError("nope"); } });
            const failed = await boom.call({}).catch((e) => e);
            if (failed.stage !== "run" || failed.message !== "RangeError: nope"
                || failed.cause.message !== "nope" || failed.tool !== "boom") throw failed;

            const odd = tool({ name: "odd", run: async () => () => 1 });
            const unserialisable = await odd.call({}).catch((e) => e);
            if (unserialisable.stage !== "result") throw unserialisable;

            const refusals = [
                () => tool({ name: "two words", run() {} }),
                () => tool({ name: "t" }),
                () => tool({ name: "t", parameters: { type: "object", properties: { x: { format: "date" } } }, run() {} }),
                () => tool({ name: "t", parameters: { type: "string" }, run() {} }),
                () => tool({ name: "t", cost: { usd: -1 }, run() {} }),
            ].map((make) => { try { make(); return "made"; } catch (e) { return e.name; } });
            if (refusals.join() !== "TypeError,TypeError,TypeError,TypeError,TypeError") {
                throw new Error(refusals.join());
            }
            console.log("tools checked");
        "#;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        assert_eq!(runtime.eval("tools.js", code).unwrap(), 0);

        let records = journal_records(&home);
        let last_console = records.iter().rfind(|r| r["kind"] == "console").unwrap();
        assert_eq!(last_console["text"], "tools checked\n");
        let tools: Vec<&serde_json::Value> = records
            .iter()
            .filter(|r| r["kind"] == "tool.call" || r["kind"] == "tool.result")
            .collect();
        // Arguments that fail their schema never start the tool.
        let kinds: Vec<&str> = tools.iter().map(|r| r["kind"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                "tool.call",
                "tool.result",
                "tool.call",
                "tool.result",
                "tool.call",
                "tool.result"
            ]
        );
        assert_eq!(
            tools[0]["req"],
            serde_json::json!({ "name": "add", "args": { "a": 2, "b": 3 } })
        );
        assert_eq!(tools[1]["eid"], tools[0]["eid"]);
        assert_eq!(tools[1]["res"]["value"], serde_json::json!({ "sum": 5 }));
        assert!(tools[1]["dur_us"].as_u64().is_some());
        assert_eq!(tools[3]["error"]["stage"], "run");
        assert_eq!(tools[3]["error"]["message"], "RangeError: nope");
        assert_eq!(tools[5]["error"]["stage"], "result");
    }

    #[test]
    fn test_agent_round_trips_tool_calls_and_stops_at_its_step_limit() {
        let code = model_stub(8988)
            + r#"
            import { model, tool, agent, AgentError, ToolError } from "vaern:ai";
            const m = model({ baseUrl: base + "/agent/v1", model: "m",
                              price: { inputPerMTok: 1, outputPerMTok: 1 } });
            const add = tool({ name: "add", description: "Adds.",
                parameters: { type: "object", properties: { a: { type: "number" }, b: { type: "number" } },
                              required: ["a", "b"] },
                run: async ({ a, b }) => ({ sum: a + b }) });
            const boom = tool({ name: "boom", run: async () => { throw new Error("broken"); } });
            const a = agent({ model: m, system: "Be brief.", tools: [add, boom], maxSteps: 3 });

            const out = await a.run("add");
            if (out.text !== 'result: {"sum":5}') throw new Error(out.text);
            if (out.steps.map((s) => s.type).join() !== "model,tool,model") throw new Error("steps");
            if (out.steps[1].result.sum !== 5) throw new Error("tool step");
            if (out.usage.inputTokens !== 20 || out.usage.outputTokens !== 4
                || out.costUsd !== 24000 / 1e9) throw new Error(JSON.stringify(out));
            if (seen[0].body.tools[0].function.name !== "add") throw new Error("tools not sent");
            const second = seen[1].body.messages;
            if (second.map((x) => x.role).join() !== "system,user,assistant,tool"
                || second[3].tool_call_id !== second[2].tool_calls[0].id) {
                throw new Error(JSON.stringify(second));
            }

            const failed = await a.run("boom");
            if (failed.text !== "result: Error: Error: broken") throw new Error(failed.text);
            const unknown = await a.run("unknown");
            if (!unknown.text.startsWith("result: Error: the model called an unknown tool")) {
                throw new Error(unknown.text);
            }
            const bad = await a.run("bad").catch((e) => e);
            if (!(bad instanceof ToolError) || bad.stage !== "args" || bad.path !== "$.a") throw bad;
            const loop = await a.run("loop").catch((e) => e);
            if (!(loop instanceof AgentError) || loop.code !== "STEP_LIMIT"
                || loop.steps.filter((s) => s.type === "model").length !== 3) throw loop;

            const refusals = [
                () => agent({ model: {} }),
                () => agent({ model: m, tools: [add, add] }),
                () => agent({ model: m, maxSteps: 0 }),
            ].map((make) => { try { make(); return "made"; } catch (e) { return e.name; } });
            if (refusals.join() !== "TypeError,TypeError,TypeError") throw new Error(refusals.join());
            await stub.close();
            console.log("agents checked");
            "#;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        assert_eq!(runtime.eval("agent.js", &code).unwrap(), 0);

        let records = journal_records(&home);
        let last_console = records.iter().rfind(|r| r["kind"] == "console").unwrap();
        assert_eq!(last_console["text"], "agents checked\n");
        let count = |kind: &str| records.iter().filter(|r| r["kind"] == kind).count();
        // add 2, boom 2, unknown 2, bad 2 and loop 3 model calls; tools ran
        // once each for add and boom and three times for loop.
        assert_eq!(count("model.call"), 11);
        assert_eq!(count("tool.call"), 5);
        assert_eq!(count("tool.result"), 5);
    }

    #[test]
    fn test_a_reserve_that_does_not_fit_opens_no_connection() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                counter.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
        let code = format!(
            r#"
            import {{ model, tool, budget, BudgetError }} from "vaern:ai";
            const left = budget({{ usd: 0.001, tokens: 5000 }});
            if (left.usd !== 0.001 || left.tokens !== 5000) throw new Error(JSON.stringify(left));
            const ask = {{ messages: [{{ role: "user", content: "hi" }}] }};
            const base = "http://127.0.0.1:{port}/v1";
            // 1000 output tokens at 10 USD per million reserve 0.01 USD.
            const pricey = model({{ baseUrl: base, model: "m", timeoutMs: 500, maxOutputTokens: 1000,
                                    price: {{ inputPerMTok: 1, outputPerMTok: 10 }} }});
            const usd = await pricey.chat(ask).catch((e) => e);
            if (!(usd instanceof BudgetError) || usd.code !== "RESERVE_EXCEEDS_REMAINING"
                || usd.unit !== "usd" || !(usd.reserve >= 0.01) || usd.remaining !== 0.001) throw usd;
            const wordy = model({{ baseUrl: base, model: "m", timeoutMs: 500, maxOutputTokens: 6000 }});
            const tokens = await wordy.stream(ask).next().catch((e) => e);
            if (tokens.unit !== "tokens" || tokens.remaining !== 5000) throw tokens;
            const costly = tool({{ name: "costly", cost: {{ usd: 0.002 }},
                                   run: async () => {{ throw new Error("ran"); }} }});
            const ran = await costly.call({{}}).catch((e) => e);
            if (!(ran instanceof BudgetError) || ran.unit !== "usd") throw ran;
            // Raising is ignored; lowering applies.
            if (budget({{ usd: 5 }}).usd !== 0.001 || budget({{ tokens: 10 }}).tokens !== 10) {{
                throw new Error("limits");
            }}
            const late = (() => {{ try {{ budget({{ name: "late" }}); }} catch (e) {{ return e.code; }} }})();
            if (late !== "INVALID") throw new Error(String(late));
            console.log("refused");
            "#
        );
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        assert_eq!(runtime.eval("refuse.js", &code).unwrap(), 0);
        let records = journal_records(&home);
        let last_console = records.iter().rfind(|r| r["kind"] == "console").unwrap();
        assert_eq!(last_console["text"], "refused\n");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            0,
            "a call that did not fit opened a connection"
        );
        assert!(!records.iter().any(|r| r["kind"] == "model.call"));
    }

    #[test]
    fn test_budgets_settle_actual_usage_and_named_ledgers_carry_across_runs() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let script = model_stub(8987)
            + r#"
            import { model, tool, budget } from "vaern:ai";
            const before = budget({ name: "team", usd: 1, tokens: 1000 });
            const m = model({ baseUrl: base + "/ok/v1", model: "m", maxOutputTokens: 64,
                              price: { inputPerMTok: 0.15, outputPerMTok: 0.6 } });
            await m.chat({ messages: [{ role: "user", content: "x" }] });
            const priced = tool({ name: "priced", cost: { usd: 0.000001 }, run: async () => 1 });
            await priced.call({});
            const after = budget.remaining();
            // 6000 nano-USD for the call and 1000 for the tool; 25 tokens.
            if (Math.round((before.usd - after.usd) * 1e9) !== 7000 || before.tokens - after.tokens !== 25) {
                throw new Error(JSON.stringify([before, after]));
            }
            console.log(JSON.stringify({ nano: Math.round(before.usd * 1e9), tokens: before.tokens }));
            await stub.close();
            "#;
        for _ in 0..2 {
            let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
            runtime.set_journal_home(Some(home.clone()));
            runtime.set_budget(crate::budget::Limits::default(), Some(home.clone()));
            assert_eq!(runtime.eval("budgeted.js", &script).unwrap(), 0);
        }
        // The second run started with what the first had spent.
        let records = journal_records(&home);
        let printed = records.iter().rfind(|r| r["kind"] == "console").unwrap();
        assert_eq!(printed["text"], "{\"nano\":999993000,\"tokens\":975}\n");
        let kinds: Vec<&str> = records
            .iter()
            .filter_map(|r| r["kind"].as_str().filter(|k| k.starts_with("budget.")))
            .collect();
        assert_eq!(
            kinds,
            [
                "budget.reserve",
                "budget.settle",
                "budget.reserve",
                "budget.settle"
            ]
        );
        let ledger = crate::journal::verify(&home.join("budgets/team.jsonl")).unwrap();
        assert_eq!(ledger.records, 8);
    }

    #[test]
    fn test_a_call_that_was_never_sent_gives_its_reserve_back_and_a_sent_one_keeps_it() {
        use std::io::{BufRead, Read};
        // Answers the first request with a 500 and leaves the second without
        // an answer until the caller's timeout has passed.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stub = std::thread::spawn(move || {
            for answer in [true, false] {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                if answer {
                    let reply = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 4\r\nconnection: close\r\n\r\nbusy";
                    reader.get_mut().write_all(reply.as_bytes()).unwrap();
                } else {
                    std::thread::sleep(Duration::from_millis(900));
                }
            }
        });
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let main = root.path().join("main.js");
        let script = r#"
            import { model, budget, ModelError } from "vaern:ai";
            budget({ tokens: 1000 });
            const price = { inputPerMTok: 1, outputPerMTok: 1 };
            const ask = { messages: [{ role: "user", content: "x" }], maxTokens: 10 };
            const left = () => budget.remaining().tokens;
            const failure = (promise) => promise.then(
                () => { throw new Error("the call resolved"); },
                (err) => { if (!(err instanceof ModelError)) throw err; return err; });

            // Nothing listens on port 1: no connection, so nothing was sent.
            const nowhere = model({ baseUrl: "http://127.0.0.1:1/v1", model: "m", price });
            const refused = await failure(nowhere.chat(ask));
            if (refused.stage !== "connect" || refused.costUsd !== 0 || left() !== 1000) {
                throw new Error(JSON.stringify([refused.stage, refused.costUsd, left()]));
            }
            const stream = await failure((async () => {
                for await (const ev of nowhere.stream(ask)) {}
            })());
            if (stream.stage !== "connect" || left() !== 1000) throw new Error("stream " + left());

            // The server answered 500: the request was sent, the reserve stays.
            const m = model({ baseUrl: "http://127.0.0.1:PORT/v1", model: "m", price, timeoutMs: 300 });
            const failed = await failure(m.chat(ask));
            const reserve = 1000 - left();
            if (failed.stage !== "http" || !(reserve >= 10) || failed.costUsd !== reserve * 1000 / 1e9) {
                throw new Error(JSON.stringify([failed.stage, failed.costUsd, reserve]));
            }
            // No answer in time: the request was sent, so this one stays too.
            const late = await failure(m.chat(ask));
            if (late.stage !== "connect" || late.costUsd !== failed.costUsd) {
                throw new Error(JSON.stringify([late.stage, late.costUsd]));
            }
            console.log(left() === 1000 - 2 * reserve);
            "#
        .replace("PORT", &port.to_string());
        std::fs::write(&main, script).unwrap();
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        assert_eq!(runtime.run_file(&main).unwrap(), 0);
        stub.join().unwrap();

        let records = journal_records(&home);
        let last_console = records.iter().rfind(|r| r["kind"] == "console").unwrap();
        assert_eq!(last_console["text"], "true\n");
        // The journal's charge agrees with what the budget kept.
        let calls: Vec<(bool, u64)> = records
            .iter()
            .filter(|r| r["kind"] == "model.call")
            .map(|r| {
                let error = &r["error"];
                (
                    error["sent"].as_bool().unwrap(),
                    error["charged_nano_usd"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(calls.len(), 4, "{records:#?}");
        assert_eq!(&calls[..2], [(false, 0), (false, 0)]);
        assert!(calls[2].0 && calls[2].1 >= 10_000, "{calls:?}");
        assert_eq!(calls[3], calls[2]);

        // A replay rebuilds the same budget from the recorded outcomes.
        let summary = replay_latest(&home).unwrap();
        assert_eq!((summary.console, summary.exit_code), (1, 0));
    }

    #[test]
    fn test_journal_keeps_caps_ledger_balances_and_error_actions_for_replay() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let script = model_stub(8986)
            + r#"
            import { model, budget } from "vaern:ai";
            budget({ name: "kept", usd: 1 });
            const at = (path) => model({ baseUrl: base + path, model: "m",
                                         price: { inputPerMTok: 1, outputPerMTok: 1 } });
            const ask = { messages: [{ role: "user", content: "x" }], maxTokens: 8 };
            await at("/ok/v1").chat(ask);
            await at("/fail/v1").chat(ask).catch(() => {});
            await stub.close();
            "#;
        let caps = crate::budget::Limits {
            nano: Some(2_000_000_000),
            tokens: None,
        };
        let mut opened = Vec::new();
        for _ in 0..2 {
            let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
            runtime.set_journal_home(Some(home.clone()));
            runtime.set_budget(caps, Some(home.clone()));
            assert_eq!(runtime.eval("kept.js", &script).unwrap(), 0);
            let records = journal_records(&home);
            assert_eq!(
                records[0]["budget_caps"],
                serde_json::json!({ "nano_usd": 2_000_000_000u64, "tokens": null })
            );
            let open = records.iter().find(|r| r["kind"] == "ledger.open").unwrap();
            assert_eq!(open["name"], "kept");
            opened.push(open["spent_nano_usd"].as_u64().unwrap());
            let failed = records
                .iter()
                .find(|r| r["kind"] == "model.call" && r["error"].is_object())
                .unwrap();
            assert!(failed["error"]["action"].as_str().unwrap().len() > 10);
        }
        // The second run opened the ledger with what the first left in it.
        assert_eq!(opened[0], 0);
        assert!(opened[1] > 0, "{opened:?}");
    }

    #[test]
    fn test_zero_usage_on_a_reply_with_text_keeps_the_reserve_charged() {
        let zero = r#""usage":{"prompt_tokens":0,"completion_tokens":0}"#;
        let reply = |text: &str| {
            format!(
                r#"{{"choices":[{{"message":{{"content":"{text}"}},"finish_reason":"stop"}}],{zero}}}"#
            )
        };
        let sse = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"hi\"}},\"finish_reason\":\"stop\"}}]}}\n\n\
             data: {{\"choices\":[],{zero}}}\n\ndata: [DONE]\n\n"
        );
        let path = "/v1/chat/completions";
        let (port, stub) = canned_http(vec![
            vec![(path, "application/json", reply("hello"))],
            vec![(path, "text/event-stream", sse)],
            vec![(path, "application/json", reply(""))],
        ]);
        // 1 USD per million tokens is 1000 nano-USD per token.
        let code = r#"
            import { model, budget } from "vaern:ai";
            budget({ usd: 1, tokens: 1000 });
            const m = model({ baseUrl: "http://127.0.0.1:PORT/v1", model: "m",
                              price: { inputPerMTok: 1, outputPerMTok: 1 } });
            const ask = { messages: [{ role: "user", content: "x" }], maxTokens: 10 };
            const left = () => budget.remaining().tokens;

            const said = await m.chat(ask);
            const reserve = 1000 - left();
            if (!(reserve >= 10) || said.costUsd !== reserve * 1000 / 1e9
                || said.usage.inputTokens !== 0) throw new Error(JSON.stringify([said, reserve]));

            // A streamed request is longer, so its reserve is its own.
            let last;
            for await (const ev of m.stream(ask)) last = ev;
            const streamed = 1000 - reserve - left();
            if (last.text !== "hi" || !(streamed >= 10)
                || last.costUsd !== streamed * 1000 / 1e9) {
                throw new Error(JSON.stringify([last, streamed]));
            }

            // An empty reply reporting zero is taken at its word.
            const empty = await m.chat(ask);
            if (empty.costUsd !== 0 || left() !== 1000 - reserve - streamed) {
                throw new Error(JSON.stringify([empty, left()]));
            }
            "#
        .replace("PORT", &port.to_string());
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        assert_eq!(runtime.eval("zero_usage.js", &code).unwrap(), 0);
        stub.join().unwrap();
        let charged: Vec<u64> = journal_records(&home)
            .iter()
            .filter(|r| r["kind"] == "model.call")
            .map(|r| r["res"]["charged_nano_usd"].as_u64().unwrap())
            .collect();
        assert_eq!(charged.len(), 3);
        assert!(charged[0] >= 10_000 && charged[1] >= 10_000, "{charged:?}");
        assert_eq!(charged[2], 0);
    }

    #[test]
    fn test_model_errors_name_their_stage_and_unreported_usage_is_charged() {
        let code = model_stub(8991)
            + r#"
            import { model, ModelError } from "vaern:ai";
            const price = { inputPerMTok: 1, outputPerMTok: 1 };
            const ask = (path) => model({ baseUrl: base + path, model: "m", price })
                .chat({ messages: [{ role: "user", content: "x" }], maxTokens: 10 });
            const stageOf = (p) => p.then(() => "resolved", (e) => e instanceof ModelError
                ? e.stage + ":" + (e.status ?? "-") : "other:" + e);
            const got = [
                await stageOf(ask("/fail/v1")),
                await stageOf(ask("/junk/v1")),
                await stageOf(model({ model: "m" }).chat({ messages: [{ role: "user", content: "x" }] })),
            ].join(",");
            if (got !== "http:500,parse:200,config:-") throw new Error(got);
            // No usage reported: the call is charged its reservation, at least
            // the full output cap of 10 tokens at 1 USD per million.
            const r = await ask("/nousage/v1");
            if (r.usage.inputTokens !== null || !(r.costUsd >= 10 / 1e6)) throw new Error(JSON.stringify(r));
            await stub.close();
            // With the stub closed, only the call in flight keeps the run alive.
            const refused = await stageOf(model({ baseUrl: "http://127.0.0.1:1/v1", model: "m" })
                .chat({ messages: [{ role: "user", content: "x" }] }));
            if (refused !== "connect:-") throw new Error(refused);
            console.log("model errors checked");
            "#;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        assert_eq!(runtime.eval("model_errors.js", &code).unwrap(), 0);

        let records = journal_records(&home);
        // The script ran to its last line rather than stopping at an await.
        let last_console = records.iter().rfind(|r| r["kind"] == "console").unwrap();
        assert_eq!(last_console["text"], "model errors checked\n");
        let stages: Vec<&str> = records
            .iter()
            .filter(|r| r["kind"] == "model.call")
            .map(|r| r["error"]["stage"].as_str().unwrap_or("ok"))
            .collect();
        // The config failure was refused before dispatch and is not recorded.
        assert_eq!(stages, ["http", "parse", "ok", "connect"], "{records:#?}");
    }

    #[test]
    fn test_math_random_draws_from_the_seed_in_run_start() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        let code = "for (let i = 0; i < 3; i++) console.log(String(Math.random()));";
        assert_eq!(runtime.eval("random.js", code).unwrap(), 0);

        let records = journal_records(&home);
        let hex = records[0]["random_seed"].as_str().unwrap();
        assert_eq!(hex.len(), 32);
        let mut s = [0u32; 4];
        for (i, word) in s.iter_mut().enumerate() {
            *word = u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).unwrap();
        }
        if s == [0; 4] {
            s[0] = 1;
        }
        // Reference xoshiro128**, two outputs per 53-bit result.
        let mut next = || {
            let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
            let t = s[1] << 9;
            s[2] ^= s[0];
            s[3] ^= s[1];
            s[1] ^= s[2];
            s[0] ^= s[3];
            s[2] ^= t;
            s[3] = s[3].rotate_left(11);
            result
        };
        let drawn: Vec<f64> = records
            .iter()
            .filter(|r| r["kind"] == "console")
            .map(|r| r["text"].as_str().unwrap().trim().parse().unwrap())
            .collect();
        assert_eq!(drawn.len(), 3);
        for value in drawn {
            let high = f64::from(next() >> 5);
            let low = f64::from(next() >> 6);
            assert_eq!(value, (high * 67108864.0 + low) / 9007199254740992.0);
        }
    }

    /// Answers HTTP requests with canned `(path, content type, body)`
    /// replies. Each group's connections are all accepted first, then
    /// answered in the group's order, 30 ms apart, so a request sent later
    /// can complete first, in a later event-loop turn.
    fn canned_http(
        groups: Vec<Vec<(&'static str, &'static str, String)>>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, Read};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for group in groups {
                let mut waiting = Vec::new();
                for _ in 0..group.len() {
                    let (stream, _) = listener.accept().unwrap();
                    let mut reader = std::io::BufReader::new(stream);
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).unwrap();
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        let lower = line.to_ascii_lowercase();
                        if let Some(value) = lower.strip_prefix("content-length:") {
                            length = value.trim().parse().unwrap();
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    reader.read_exact(&mut vec![0; length]).unwrap();
                    waiting.push((request_line, reader));
                }
                for (path, content_type, body) in group {
                    let at = waiting
                        .iter()
                        .position(|(line, _)| line.split(' ').nth(1) == Some(path))
                        .unwrap();
                    let (_, mut reader) = waiting.remove(at);
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    reader.get_mut().write_all(reply.as_bytes()).unwrap();
                    std::thread::sleep(Duration::from_millis(30));
                }
            }
        });
        (port, server)
    }

    fn replay_latest(home: &Path) -> Result<crate::replay::Summary> {
        let path = crate::journal::locate(home, "latest")?;
        let plan = crate::replay::Plan::load(&path)?;
        let argv = plan.argv.clone();
        QuickJsRuntime::new(RuntimeOptions { argv })?.replay(plan)
    }

    #[test]
    fn test_replay_reproduces_the_transcript_byte_for_byte_without_any_io() {
        let chat = r#"{"choices":[{"message":{"content":"hello"},"finish_reason":"stop"}],
                       "usage":{"prompt_tokens":5,"completion_tokens":2}}"#;
        let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"str\"}}]}\n\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"eam\"},\"finish_reason\":\"stop\"}]}\n\n\
                      data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n\
                      data: [DONE]\n\n";
        let (port, stub) = canned_http(vec![
            vec![("/v1/chat/completions", "application/json", chat.to_string())],
            vec![(
                "/v1/chat/completions",
                "text/event-stream",
                stream.to_string(),
            )],
            // Two requests at once, the one sent second answered first.
            vec![
                ("/t", "text/plain", "tool".to_string()),
                ("/a", "text/plain", "plain".to_string()),
            ],
        ]);
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let out = root.path().join("out.txt");
        let main = root.path().join("main.js");
        let script = r#"
            import { model, tool, budget } from "vaern:ai";
            import { readFile, writeFile } from "vaern:fs";
            const base = "http://127.0.0.1:PORT";
            budget({ usd: 0.5 });
            const m = model({ baseUrl: base + "/v1", model: "m",
                              price: { inputPerMTok: 1, outputPerMTok: 2 } });
            const r = await m.chat({ messages: [{ role: "user", content: "hi" }], maxTokens: 16 });
            console.log(r.text, r.effect, r.costUsd, Math.random());
            for await (const e of m.stream({ messages: [{ role: "user", content: "go" }], maxTokens: 16 })) {
                console.log(JSON.stringify(e));
            }
            const lookup = tool({ name: "lookup", run: async () => (await fetch(base + "/t")).text() });
            const landed = (t) => (console.log("landed " + t), t);
            const both = await Promise.all([
                fetch(base + "/a").then((r) => r.text()).then(landed),
                lookup.call({}).then(landed),
            ]);
            console.log(both.join("+"), JSON.stringify(budget.remaining()));
            await writeFile("OUT", both.join(""));
            console.error(await readFile("OUT", "utf8"));
            process.exitCode = 3;
            "#
        .replace("PORT", &port.to_string())
        .replace("OUT", &out.display().to_string().replace('\\', "/"));
        std::fs::write(&main, script).unwrap();

        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        runtime.set_budget(
            crate::budget::Limits {
                nano: Some(400_000_000),
                tokens: None,
            },
            Some(home.clone()),
        );
        assert_eq!(runtime.run_file(&main).unwrap(), 3);
        stub.join().unwrap();
        let recorded: Vec<_> = journal_records(&home)
            .into_iter()
            .filter(|r| r["kind"] == "console")
            .collect();
        assert_eq!(recorded.len(), 8, "{recorded:#?}");
        std::fs::remove_file(&out).unwrap();

        // The stub is gone and the file deleted: everything comes from the
        // journal, and the transcript matches byte for byte.
        let summary = replay_latest(&home).unwrap();
        assert_eq!((summary.console, summary.exit_code), (8, 3));
        assert!(!out.exists(), "the replay wrote a file");
    }

    #[test]
    fn test_replay_stops_at_a_divergence_a_changed_source_or_serve() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let main = root.path().join("main.js");
        let record = |source: &str| {
            std::fs::write(&main, source).unwrap();
            let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
            runtime.set_journal_home(Some(home.clone()));
            assert_eq!(runtime.run_file(&main).unwrap(), 0);
        };

        // The clock is read live, so a script that prints it diverges there.
        record(
            "console.log('steady');\n\
             await new Promise((resolve) => setTimeout(resolve, 3));\n\
             console.log(Date.now());\n",
        );
        let seq = journal_records(&home)
            .iter()
            .filter(|r| r["kind"] == "console")
            .nth(1)
            .unwrap()["seq"]
            .as_u64()
            .unwrap();
        match replay_latest(&home) {
            Err(VaernError::ReplayDivergence {
                seq: at, expected, ..
            }) => {
                assert_eq!(at, seq);
                assert!(expected.starts_with("stdout \"1"), "{expected}");
            }
            other => panic!("{other:?}"),
        }

        // A request that differs from the recorded one stops the replay at
        // that effect, before its recorded outcome is handed back.
        let stamp = root.path().join("stamp.txt").display().to_string();
        record(&format!(
            "import {{ writeFile }} from 'vaern:fs';\n\
             await new Promise((resolve) => setTimeout(resolve, 3));\n\
             await writeFile({stamp:?}, String(Date.now()));\n"
        ));
        let write = journal_records(&home)
            .into_iter()
            .find(|r| r["kind"] == "fs")
            .unwrap();
        match replay_latest(&home) {
            Err(VaernError::ReplayDivergence { seq, actual, .. }) => {
                assert_eq!(Some(seq), write["seq"].as_u64());
                assert!(actual.starts_with("fs "), "{actual}");
            }
            other => panic!("{other:?}"),
        }

        // So does an outcome that differs, with the transcript intact.
        std::fs::write(
            &main,
            "await new Promise((resolve) => setTimeout(resolve, 3));\n\
             throw new Error('at ' + Date.now());\n",
        )
        .unwrap();
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        assert!(runtime.run_file(&main).is_err());
        match replay_latest(&home) {
            Err(VaernError::ReplayDivergence {
                expected, actual, ..
            }) => {
                assert!(expected.starts_with("exit code 1 with error"), "{expected}");
                assert_ne!(expected, actual);
            }
            other => panic!("{other:?}"),
        }

        record("console.log('a');\n");
        std::fs::write(&main, "console.log('b');\n").unwrap();
        assert!(matches!(
            replay_latest(&home),
            Err(VaernError::ReplaySourceChanged { .. })
        ));

        record(
            "import { serve } from 'vaern:http';\n\
             const s = serve(() => new Response('x'), { port: 0 });\n\
             await s.close();\n",
        );
        match replay_latest(&home) {
            Err(VaernError::Replay { code, .. }) => assert_eq!(code, "UNSUPPORTED"),
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    fn link_dir(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn link_dir(target: &Path, link: &Path) {
        std::os::windows::fs::symlink_dir(target, link).unwrap();
    }

    #[test]
    fn test_a_script_cannot_reach_its_journals_or_ledgers_through_vaern_fs() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let caps = crate::budget::Limits {
            nano: Some(2_000_000_000),
            tokens: None,
        };
        let run = |name: &str, script: &str| {
            let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
            runtime.set_journal_home(Some(home.clone()));
            runtime.set_budget(caps, Some(home.clone()));
            let outcome = runtime.eval(name, script);
            assert_eq!(outcome.unwrap(), 0, "{name}");
            journal_records(&home)
        };
        let spent = |records: &[serde_json::Value]| {
            let open = records.iter().find(|r| r["kind"] == "ledger.open").unwrap();
            open["spent_nano_usd"].as_u64().unwrap()
        };

        // One run spends from a named budget.
        let spend = model_stub(8985)
            + r#"
            import { model, budget } from "vaern:ai";
            budget({ name: "nightly", usd: 1 });
            await model({ baseUrl: base + "/ok/v1", model: "m",
                          price: { inputPerMTok: 1, outputPerMTok: 1 } })
                .chat({ messages: [{ role: "user", content: "x" }], maxTokens: 8 });
            await stub.close();
            "#;
        assert_eq!(spent(&run("spend.js", &spend)), 0);

        // The next one tries to erase that spending and its own journal:
        // directly, through a link into the home, and through `..`.
        let link = root.path().join("link");
        link_dir(&home, &link);
        let slashed = |path: &Path| path.display().to_string().replace('\\', "/");
        let attack = r#"
            import { readFile, writeFile, appendFile } from "vaern:fs";
            const ledger = "HOME/budgets/nightly.jsonl";
            const tries = [
                () => writeFile(ledger, ""),
                () => writeFile(ledger, "", { flag: "wx" }),
                () => appendFile(ledger, "x"),
                () => readFile(ledger, "utf8"),
                () => writeFile("HOME/budgets/nightly.lock", ""),
                () => writeFile("HOME/runs/latest", "forged"),
                () => writeFile("HOME/new.txt", ""),
                () => writeFile("LINK/budgets/nightly.jsonl", ""),
                () => writeFile("ROOT/sub/../.vaern/budgets/nightly.jsonl", ""),
            ];
            const seen = [];
            for (const attempt of tries) {
                seen.push(await attempt().then(() => "done", (e) => e.name + " " + e.code));
            }
            if (seen.some((s) => s !== "FsError EPERM")) throw new Error(seen.join(", "));
            const refused = await writeFile(ledger, "").catch((e) => e);
            if (!refused.message.includes("not script-writable")
                || !refused.message.includes("nightly.jsonl")
                || !refused.message.includes(".vaern'")) throw refused;
            // A path beside the home is still the script's to use.
            await writeFile("ROOT/.vaern-notes.txt", "kept");
            console.log(await readFile("ROOT/.vaern-notes.txt", "utf8"));
            "#
        .replace("HOME", &slashed(&home))
        .replace("LINK", &slashed(&link))
        .replace("ROOT", &slashed(root.path()));
        std::fs::create_dir(root.path().join("sub")).unwrap();
        let records = run("attack.js", &attack);
        let last_console = records.iter().rfind(|r| r["kind"] == "console").unwrap();
        assert_eq!(last_console["text"], "kept\n");
        assert!(!home.join("new.txt").exists());

        // The ledger still holds what the first run spent.
        let reopen = r#"
            import { budget } from "vaern:ai";
            budget({ name: "nightly", usd: 1 });
            "#;
        assert!(spent(&run("reopen.js", reopen)) > 0);
    }

    #[test]
    fn test_failed_run_is_sealed_with_its_error() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vaern");
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        runtime.set_journal_home(Some(home.clone()));
        let err = runtime
            .eval("boom.js", "throw new Error('boom');")
            .unwrap_err();
        let records = journal_records(&home);
        let end = records.last().unwrap();
        assert_eq!(end["kind"], "run.end");
        assert_eq!(end["exit_code"], 1);
        assert_eq!(end["error"], err.to_string());
    }

    #[test]
    fn test_journal_is_off_unless_a_home_is_set() {
        let root = tempfile::tempdir().unwrap();
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        assert_eq!(runtime.eval("quiet.js", "console.log('x');").unwrap(), 0);
        assert!(runtime.journal.borrow().is_none());
        assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    }

    #[test]
    fn test_parse_errors_outside_the_project_do_not_quote_the_file() {
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join("secret");
        let project = root.path().join("project");
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(secret.join("ini.js"), "[default]\nkey=TOKEN\n").unwrap();
        std::fs::write(secret.join("ini.ts"), "[default]\nkey=TOKEN\n").unwrap();
        std::fs::write(project.join("own.js"), "[default]\n").unwrap();
        let main = project.join("main.js");
        std::fs::write(
            &main,
            r#"
            const seen = [];
            for (const f of ["../secret/ini.js", "../secret/ini.ts", "./own.js"]) {
                try { await import(f); } catch (e) { seen.push(String(e.message)); }
            }
            const [js, ts, own] = seen;
            const hidden = (m) => !m.includes("default") && !m.includes("TOKEN")
                && m.includes("not shown for files outside the project");
            process.exitCode =
                (hidden(js) && js.includes("line 1, column") ? 0 : 1) |
                (hidden(ts) && ts.includes("line 1, column") ? 0 : 2) |
                (own.includes("default") ? 0 : 4);
            if (process.exitCode) console.error(seen.join("\n"));
            "#,
        )
        .unwrap();
        // The working directory (the crate) does not contain the temp tree,
        // so only the entry script's directory is the project.
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        assert_eq!(runtime.run_file(&main).unwrap(), 0);
    }

    #[test]
    fn test_byte_order_mark_does_not_shift_reported_locations() {
        let root = tempfile::tempdir().unwrap();
        let line = "const s: string = 'x'; throw new Error('boom');\n";
        for name in ["plain.ts", "plain.js"] {
            let source = if name.ends_with(".js") {
                line.replace(": string", "")
            } else {
                line.to_string()
            };
            let plain = root.path().join(name);
            std::fs::write(&plain, &source).unwrap();
            let marked_name = format!("bom-{name}");
            let marked = root.path().join(&marked_name);
            std::fs::write(&marked, format!("\u{FEFF}{source}")).unwrap();

            let location = |path: &Path, file: &str| {
                let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
                let text = runtime.run_file(path).unwrap_err().to_string();
                // The last mention is the stack frame, which carries the location.
                let start = text.rfind(file).unwrap_or_else(|| panic!("{text}"));
                text[start + file.len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == ':')
                    .collect::<String>()
            };
            assert_eq!(
                location(&marked, &marked_name),
                location(&plain, name),
                "{name}"
            );
        }
    }

    #[test]
    fn test_ambient_declarations_run() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let code = "declare enum Level { Low, High }\n\
                    declare namespace Lib { const version: string; }\n\
                    declare global { interface Window { tag: string } }\n\
                    const level: number = 2;\n\
                    process.exitCode = level === 2 ? 0 : 1;\n";
        assert_eq!(runtime.eval("ambient.ts", code).unwrap(), 0);
    }

    // The engine reports byte columns, so blanked type text must keep its
    // byte length for `.ts` columns to agree with `.js` ones.
    #[test]
    fn test_columns_count_bytes_in_js_and_in_blanked_ts_types() {
        let column = |name: &str, code: &str| {
            let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
            let text = runtime.eval(name, code).unwrap_err().to_string();
            let start = text.rfind(name).unwrap_or_else(|| panic!("{text}")) + name.len();
            text[start..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ':')
                .collect::<String>()
        };
        // '文' is three bytes, the same as 'abc'.
        let js_wide = column("wide.js", "const s = '文'; throw new Error('x');\n");
        let js_ascii = column("ascii.js", "const s = 'abc'; throw new Error('x');\n");
        let ts_wide = column(
            "wide.ts",
            "const s: Array<文> = []; throw new Error('x');\n",
        );
        let ts_ascii = column(
            "ascii.ts",
            "const s: Array<abc> = []; throw new Error('x');\n",
        );
        assert_eq!(js_wide, js_ascii);
        assert_eq!(ts_wide, ts_ascii);
    }

    #[test]
    fn test_parse_error_reports_name_message_and_location() {
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let err = runtime
            .eval("bad.js", "const a = 1;\nconst b = a +;\n")
            .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("SyntaxError: unexpected token in expression"),
            "{text}"
        );
        assert!(text.contains("bad.js:2:"), "{text}");
    }

    /// Serves one HTTP response whose headers alone take about 320 KiB.
    fn wide_reply() -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, Read};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            reader.read_exact(&mut vec![0; length]).unwrap();
            let mut head =
                String::from("HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n");
            for i in 0..80 {
                head.push_str(&format!("x-pad-{i}: {}\r\n", "h".repeat(4000)));
            }
            head.push_str("\r\nok");
            reader.get_mut().write_all(head.as_bytes()).unwrap();
        });
        (port, server)
    }

    /// A script that fills the heap, ending with blocks of `block` bytes,
    /// frees `spare` of those, awaits `call`, and exits 0 only if that
    /// promise rejected.
    fn with_full_heap(imports: &str, block: usize, spare: usize, call: &str) -> String {
        format!(
            r#"
            {imports}
            process.exitCode = 3;
            const hold = [];
            const fill = (size) => {{
                try {{ for (;;) hold.push(new ArrayBuffer(size)); }} catch {{}}
            }};
            fill(8 * 1024 * 1024);
            fill({block});
            hold.length = Math.max(0, hold.length - {spare});
            let outcome;
            try {{
                await {call};
                outcome = "resolved";
            }} catch (err) {{
                outcome = "rejected";
            }}
            hold.length = 0;
            process.exitCode = outcome === "rejected" ? 0 : 4;
        "#
        )
    }

    #[test]
    fn test_fetch_result_that_does_not_fit_the_heap_rejects() {
        // The body is two bytes. The headers are built in the heap, where
        // 128 KiB is left for 320 KiB of them.
        let (port, server) = wide_reply();
        let code = with_full_heap(
            "",
            64 * 1024,
            2,
            &format!(r#"fetch("http://127.0.0.1:{port}/")"#),
        );
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let result = runtime.eval("fetch_oom.js", &code);
        server.join().unwrap();
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_model_result_that_does_not_fit_the_heap_rejects() {
        // The reply is valid JSON whose text is larger than the heap has left.
        let text = "a".repeat(24 * 1024 * 1024);
        let body = format!(
            r#"{{"choices":[{{"message":{{"content":"{text}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":1,"completion_tokens":1}}}}"#
        );
        let (port, server) = canned_http(vec![vec![(
            "/v1/chat/completions",
            "application/json",
            body,
        )]]);
        let code = with_full_heap(
            r#"import { model } from "vaern:ai";"#,
            8 * 1024 * 1024,
            2,
            &format!(
                r#"model({{ baseUrl: "http://127.0.0.1:{port}/v1", model: "m" }})
                    .chat({{ messages: [{{ role: "user", content: "x" }}], maxTokens: 8 }})"#
            ),
        );
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let result = runtime.eval("model_oom.js", &code);
        server.join().unwrap();
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_request_that_cannot_reach_its_handler_is_answered_with_500() {
        use std::io::Read;
        // The script fills the heap in the turn that starts the server, so
        // the request arrives while there is no room to hand it over.
        let client = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut stream = loop {
                match std::net::TcpStream::connect("127.0.0.1:8984") {
                    Ok(stream) => break stream,
                    Err(err) if started.elapsed() > Duration::from_secs(10) => panic!("{err}"),
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            };
            let body = vec![b'b'; 512 * 1024];
            let head = format!(
                "POST /in HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).unwrap();
            reply
        });
        let code = r#"
            import { serve } from "vaern:http";
            let handled = 0;
            const server = serve(() => (handled++, new Response("ok")), { port: 8984, hostname: "127.0.0.1" });
            const hold = [];
            const fill = (size) => { try { for (;;) hold.push(new ArrayBuffer(size)); } catch {} };
            fill(8 * 1024 * 1024);
            fill(64 * 1024);
            hold.length -= 4;
            await new Promise((resolve) => setTimeout(resolve, 1500));
            hold.length = 0;
            await server.close();
            process.exitCode = handled === 0 ? 0 : 4;
        "#;
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let result = runtime.eval("serve_oom.js", code);
        assert_eq!(result.unwrap(), 0);
        let reply = client.join().unwrap();
        assert!(reply.starts_with("HTTP/1.1 500 "), "{reply}");
    }

    #[test]
    fn test_fs_result_that_does_not_fit_the_heap_rejects() {
        let dir = std::env::temp_dir().join(format!("vaern-rt-oom-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Under the read cap, but larger than what is left once the heap is
        // filled, so only building the string can fail.
        let file = dir.join("big.txt");
        std::fs::write(&file, vec![b'a'; 48 * 1024 * 1024]).unwrap();
        let file_js = file.display().to_string().replace('\\', "/");
        let code = format!(
            r#"
            import {{ readFile }} from "vaern:fs";
            process.exitCode = 3;
            const hold = [];
            try {{ for (;;) hold.push(new ArrayBuffer(8 * 1024 * 1024)); }} catch {{}}
            hold.length = Math.max(0, hold.length - 2);
            let outcome;
            try {{
                outcome = "resolved " + (await readFile("{file_js}", "utf8")).length;
            }} catch (err) {{
                outcome = "rejected";
            }}
            hold.length = 0;
            process.exitCode = outcome === "rejected" ? 0 : 4;
        "#
        );
        let mut runtime = QuickJsRuntime::new(RuntimeOptions::default()).unwrap();
        let result = runtime.eval("oom.js", &code);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(result.unwrap(), 0);
    }
}
