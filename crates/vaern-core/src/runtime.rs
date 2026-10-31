//! QuickJS-ng execution runtime and async event-loop driver.
//!
//! This module owns the initialization of the QuickJS engine, registration
//! of host globals (`console`, `process`, timers, `queueMicrotask`), module loading,
//! and driving the event loop to completion using Tokio.

use crate::error::{Result, VaernError};
use crate::event::{EventReceiver, EventSender, FetchCompletion, LoopEvent};
use crate::module_loader::{FsModuleLoader, ModuleLoader};
use crate::server::{HttpServer, ServerIncomingRequest, ServerResponse, ServerResponseBody};
use crate::transpile::{SwcTranspiler, Transpiler};
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::{
    Array, ArrayBuffer, Context, Ctx, Function, Module, Object, Runtime as QjsRuntime, Value,
};
use std::cell::RefCell;
use std::collections::HashMap;
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
    events_tx: EventSender,
    active_servers: usize,
    server_handles: HashMap<u32, Arc<HttpServer>>,
    server_resp_senders: HashMap<u64, tokio::sync::oneshot::Sender<ServerResponse>>,
    server_stream_senders:
        HashMap<u64, tokio::sync::mpsc::Sender<std::result::Result<bytes::Bytes, std::io::Error>>>,
    /// First unhandled promise rejection or uncaught timer exception. The event
    /// loop turns it into an error so a throwing script cannot exit with status 0.
    unhandled_error: Option<String>,
    /// Rejected promises with no handler yet, keyed by promise identity, in
    /// rejection order. A handler attached later removes only its own entry.
    pending_rejections: Vec<(u64, String)>,
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
}

impl Resolver for VaernModuleResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        if name == "vaern:http" {
            return Ok("vaern:http".to_string());
        }
        let referrer_path = if base.is_empty() {
            self.base_dir.clone()
        } else {
            PathBuf::from(base)
        };
        let loader = FsModuleLoader;
        let resolved = loader
            .resolve(name, &referrer_path)
            .map_err(|err| rquickjs::Error::new_loading_message(name, format!("{err}")))?;
        Ok(resolved.to_string_lossy().replace('\\', "/"))
    }
}

struct VaernModuleLoader {
    transpiler: SwcTranspiler,
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
        let path = Path::new(name);
        let content = std::fs::read_to_string(path).map_err(|err| {
            rquickjs::Error::new_loading_message(name, format!("failed to read '{name}': {err}"))
        })?;
        let transpiled = if name.ends_with(".ts") {
            self.transpiler
                .transpile(name, &content)
                .map_err(|err| rquickjs::Error::new_loading_message(name, format!("{err}")))?
        } else {
            content
        };
        Module::declare(ctx.clone(), name, transpiled)
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
const HEAP_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// How long the pump waits for buffer space before treating the client as stalled.
const STREAM_CHUNK_SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Hands one response chunk to the connection, waiting for buffer space.
///
/// Waiting gives the pump backpressure: a burst larger than the buffer is
/// delivered in full instead of dropping chunks. The wait happens on the JS
/// thread through `block_on`, which is safe because the connection drains on
/// the Tokio worker threads, never on this one.
fn send_stream_chunk(
    handle: &tokio::runtime::Handle,
    tx: &tokio::sync::mpsc::Sender<std::result::Result<bytes::Bytes, std::io::Error>>,
    req_id: u64,
    chunk: bytes::Bytes,
    wait: Duration,
) -> Result<()> {
    // The timer must be created inside `block_on`, which enters the runtime
    // context; constructing it on the JS thread panics with no reactor.
    let sent = handle.block_on(async { tokio::time::timeout(wait, tx.send(Ok(chunk))).await });
    match sent {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(VaernError::ServerStream {
            req_id,
            message: "the client closed the connection before the body was complete".to_string(),
            action: "Stop writing to this response; the remaining chunks cannot be delivered"
                .to_string(),
        }),
        Err(_) => {
            // Abort the body rather than let the connection end cleanly, so the
            // client sees a failed transfer instead of a silently short one.
            let tx = tx.clone();
            handle.spawn(async move {
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
    active_servers_list: Arc<tokio::sync::Mutex<Vec<Arc<HttpServer>>>>,
    _tokio_rt: tokio::runtime::Runtime,
}

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
            events_tx,
            active_servers: 0,
            server_handles: HashMap::new(),
            server_resp_senders: HashMap::new(),
            server_stream_senders: HashMap::new(),
            unhandled_error: None,
            pending_rejections: Vec::new(),
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
            active_servers_list,
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
            let print_fn = Function::new(ctx.clone(), |target: i32, msg: String| {
                if target == 0 {
                    let mut out = std::io::stdout().lock();
                    let _ = writeln!(out, "{msg}");
                } else {
                    let mut err = std::io::stderr().lock();
                    let _ = writeln!(err, "{msg}");
                }
            })
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
            let exit_fn = Function::new(ctx.clone(), |code: i32| exit_process(code))
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

                    let options = crate::fetch::FetchOptions {
                        method,
                        headers,
                        body,
                        timeout,
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
                    state_fetch.borrow_mut().pending_fetches += 1;

                    tokio_handle_fetch.spawn(async move {
                        let result = crate::fetch::execute_fetch(&url, options, abort_rx).await;
                        let _ = tx.send(LoopEvent::FetchDone(FetchCompletion { req_id, result }));
                    });

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
            let server_start_fn = Function::new(
                ctx.clone(),
                move |server_id: u32, hostname: String, port: u16| -> rquickjs::Result<String> {
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
                move |req_id: f64, chunk_val: Value<'_>, is_done: bool| -> rquickjs::Result<()> {
                    let req_id_u64 = req_id as u64;
                    let mut chunk_bytes = Vec::new();
                    if let Some(arr) = chunk_val.as_array() {
                        for i in 0..arr.len() {
                            if let Ok(num) = arr.get::<u32>(i) {
                                chunk_bytes.push(num as u8);
                            }
                        }
                    }

                    // Cloned so no RefCell borrow is held while this thread waits.
                    let sender = state_stream_chunk.borrow().server_stream_senders.get(&req_id_u64).cloned();
                    if !chunk_bytes.is_empty()
                        && let Some(tx) = sender
                    {
                        let chunk = bytes::Bytes::from(chunk_bytes);
                        if let Err(err) = send_stream_chunk(&tokio_handle_chunk, &tx, req_id_u64, chunk, STREAM_CHUNK_SEND_TIMEOUT) {
                            state_stream_chunk.borrow_mut().server_stream_senders.remove(&req_id_u64);
                            return Err(rquickjs::Exception::throw_message(chunk_val.ctx(), &err.to_string()));
                        }
                    }

                    if is_done {
                        state_stream_chunk.borrow_mut().server_stream_senders.remove(&req_id_u64);
                    }

                    Ok(())
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

                function __formatArg(arg) {
                    if (typeof arg === 'string') return arg;
                    if (typeof arg === 'undefined') return 'undefined';
                    if (arg === null) return 'null';
                    if (typeof arg === 'symbol') return arg.toString();
                    if (typeof arg === 'bigint') return arg.toString() + 'n';
                    if (typeof arg === 'object') {
                        try {
                            return JSON.stringify(arg);
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

                // WHATWG TextEncoder & TextDecoder
                class TextEncoder {
                    encode(str = "") {
                        const s = String(str);
                        const utf8 = [];
                        for (let i = 0; i < s.length; i++) {
                            let charcode = s.charCodeAt(i);
                            if (charcode < 0x80) utf8.push(charcode);
                            else if (charcode < 0x800) {
                                utf8.push(0xc0 | (charcode >> 6), 0x80 | (charcode & 0x3f));
                            } else if (charcode < 0xd800 || charcode >= 0xe000) {
                                utf8.push(0xe0 | (charcode >> 12), 0x80 | ((charcode >> 6) & 0x3f), 0x80 | (charcode & 0x3f));
                            } else {
                                i++;
                                charcode = 0x10000 + (((charcode & 0x3ff) << 10) | (s.charCodeAt(i) & 0x3ff));
                                utf8.push(0xf0 | (charcode >> 18), 0x80 | ((charcode >> 12) & 0x3f), 0x80 | ((charcode >> 6) & 0x3f), 0x80 | (charcode & 0x3f));
                            }
                        }
                        return new Uint8Array(utf8);
                    }
                }
                globalThis.TextEncoder = TextEncoder;

                class TextDecoder {
                    decode(bytes) {
                        if (!bytes) return "";
                        const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
                        let str = "";
                        for (let i = 0; i < u8.length; i++) {
                            str += String.fromCharCode(u8[i]);
                        }
                        try {
                            return decodeURIComponent(escape(str));
                        } catch (_) {
                            return str;
                        }
                    }
                }
                globalThis.TextDecoder = TextDecoder;

                // WHATWG URL & URLSearchParams
                class URLSearchParams {
                    constructor(init = "") {
                        this._params = new Map();
                        if (typeof init === 'string') {
                            const str = init.startsWith('?') ? init.slice(1) : init;
                            if (str) {
                                for (const part of str.split('&')) {
                                    const [k, v] = part.split('=');
                                    if (k) this.set(decodeURIComponent(k), v ? decodeURIComponent(v) : "");
                                }
                            }
                        }
                    }
                    get(name) {
                        return this._params.has(name) ? this._params.get(name) : null;
                    }
                    set(name, value) {
                        this._params.set(name, String(value));
                    }
                    has(name) {
                        return this._params.has(name);
                    }
                }
                globalThis.URLSearchParams = URLSearchParams;

                class URL {
                    constructor(input, base) {
                        let full = String(input);
                        if (base && !full.includes("://")) {
                            const b = String(base).endsWith('/') ? String(base).slice(0, -1) : String(base);
                            const p = full.startsWith('/') ? full : '/' + full;
                            full = b + p;
                        }
                        const m = full.match(/^([a-zA-Z]+:)\/\/([^/:]+)(?::(\d+))?(\/[^?#]*)?(\?[^#]*)?(#.*)?$/);
                        if (!m) {
                            throw new TypeError(`Invalid URL: ${input}`);
                        }
                        this.protocol = m[1];
                        this.hostname = m[2];
                        this.port = m[3] || "";
                        this.host = this.port ? `${this.hostname}:${this.port}` : this.hostname;
                        this.pathname = m[4] || "/";
                        this.search = m[5] || "";
                        this.hash = m[6] || "";
                        this.href = `${this.protocol}//${this.host}${this.pathname}${this.search}${this.hash}`;
                        this.searchParams = new URLSearchParams(this.search);
                    }
                    toString() {
                        return this.href;
                    }
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
                            const reader = this.body.getReader();
                            const chunks = [];
                            while (true) {
                                const { done, value } = await reader.read();
                                if (done) break;
                                if (typeof value === 'string') {
                                    chunks.push(value);
                                } else if (value instanceof Uint8Array) {
                                    chunks.push(new TextDecoder().decode(value));
                                }
                            }
                            return chunks.join("");
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
                            const txt = await this.text();
                            return new TextEncoder().encode(txt).buffer;
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
                            const reader = this.body.getReader();
                            const chunks = [];
                            while (true) {
                                const { done, value } = await reader.read();
                                if (done) break;
                                if (typeof value === 'string') {
                                    chunks.push(value);
                                } else if (value instanceof Uint8Array) {
                                    chunks.push(new TextDecoder().decode(value));
                                }
                            }
                            return chunks.join("");
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
                            const txt = await this.text();
                            return new TextEncoder().encode(txt).buffer;
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

                // WHATWG fetch
                const __fetchCallbacks = new Map();
                let __nextFetchId = 1;

                globalThis.fetch = async (input, init = {}) => {
                    const url = typeof input === 'string' ? input : String(input);
                    const method = String(init.method || "GET").toUpperCase();

                    const headersList = [];
                    if (init.headers) {
                        const h = init.headers instanceof Headers ? init.headers : new Headers(init.headers);
                        h.forEach((val, key) => {
                            headersList.push([key, val]);
                        });
                    }

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
                    __fetchCallbacks.delete(info.reqId);

                    if (info.success) {
                        const headers = new Headers(info.headers);
                        const res = new Response(info.body, {
                            status: info.status,
                            statusText: info.statusText,
                            headers,
                            url: info.url
                        });
                        cb.resolve(res);
                    } else {
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

                            if (response._isStream || (response.body && typeof response.body.getReader === 'function')) {
                                __vaern_server_respond_stream_start(reqInfo.reqId, status, statusText, headersList);
                                const reader = response.body.getReader();
                                try {
                                    while (true) {
                                        const { done, value } = await reader.read();
                                        if (done) {
                                            __vaern_server_respond_stream_chunk(reqInfo.reqId, [], true);
                                            break;
                                        }
                                        let chunkBytes;
                                        if (typeof value === 'string') {
                                            chunkBytes = Array.from(new TextEncoder().encode(value));
                                        } else if (value instanceof Uint8Array) {
                                            chunkBytes = Array.from(value);
                                        } else {
                                            chunkBytes = Array.from(new TextEncoder().encode(String(value)));
                                        }
                                        __vaern_server_respond_stream_chunk(reqInfo.reqId, chunkBytes, false);
                                    }
                                } catch (streamErr) {
                                    __vaern_print(1, `[ERROR] Stream read error: ${streamErr}`);
                                    __vaern_server_respond_stream_chunk(reqInfo.reqId, [], true);
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
                            const errStr = (err && (err.stack || err.message)) || String(err);
                            __vaern_print(1, `[ERROR] ${reqInfo.method} ${reqInfo.uri}: ${errStr}`);
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

    fn dispatch_fetch_completion(&self, completion: FetchCompletion) {
        let _ = self.context.with(|ctx| -> rquickjs::Result<()> {
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
        });
    }

    fn dispatch_server_request(&self, req: ServerIncomingRequest) {
        let req_id = req.req_id;
        let resp_sender = req.resp_sender;
        self.state
            .borrow_mut()
            .server_resp_senders
            .insert(req_id, resp_sender);

        let _ = self.context.with(|ctx| -> rquickjs::Result<()> {
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

            dispatch_fn.call::<_, ()>((info,))?;
            Ok(())
        });
    }

    fn handle_event(&self, event: LoopEvent) {
        match event {
            LoopEvent::ServerRequest(req) => self.dispatch_server_request(req),
            LoopEvent::FetchDone(completion) => {
                if self.state.borrow().pending_fetches > 0 {
                    self.state.borrow_mut().pending_fetches -= 1;
                }
                self.dispatch_fetch_completion(completion);
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

            // Drain all pending microtasks
            while self.qjs_runtime.is_job_pending() {
                if let Err(err) = self.qjs_runtime.execute_pending_job() {
                    return Err(VaernError::Execution {
                        file: file.to_string(),
                        message: format!("unhandled microtask rejection: {err}"),
                    });
                }
            }

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

            // Inspect active timers, pending fetch status, and active servers
            let (
                expired,
                min_deadline,
                has_pending_fetches,
                has_active_servers,
                _has_active_timers,
            ) = {
                let s = self.state.borrow();
                let pending_fetches = s.pending_fetches > 0;
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
        let path_str = path.display().to_string();
        if !path.exists() {
            return Err(VaernError::Io {
                path: path_str,
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"),
            });
        }

        let source = std::fs::read_to_string(path).map_err(|err| VaernError::Io {
            path: path_str.clone(),
            source: err,
        })?;

        let transpiled = if path.extension().is_some_and(|ext| ext == "ts") {
            self.transpiler.transpile(&path_str, &source)?
        } else {
            source
        };

        let base_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();

        self.qjs_runtime.set_loader(
            VaernModuleResolver { base_dir },
            VaernModuleLoader {
                transpiler: self.transpiler.clone(),
            },
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
                if let Some(e) = exc.as_exception() {
                    e.stack()
                        .or_else(|| e.message())
                        .unwrap_or_else(|| err.to_string())
                } else if let Some(s) = exc.as_string() {
                    s.to_string().unwrap_or_else(|_| err.to_string())
                } else {
                    err.to_string()
                }
            });
            return Err(VaernError::Execution {
                file: path_str,
                message: msg,
            });
        }

        self.run_event_loop(&path_str)
    }

    fn eval(&mut self, filename: &str, source: &str) -> Result<i32> {
        let transpiled = if filename.ends_with(".ts") {
            self.transpiler.transpile(filename, source)?
        } else {
            source.to_string()
        };

        let base_dir = Path::new(".").to_path_buf();
        self.qjs_runtime.set_loader(
            VaernModuleResolver { base_dir },
            VaernModuleLoader {
                transpiler: self.transpiler.clone(),
            },
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
                if let Some(e) = exc.as_exception() {
                    e.stack()
                        .or_else(|| e.message())
                        .unwrap_or_else(|| err.to_string())
                } else if let Some(s) = exc.as_string() {
                    s.to_string().unwrap_or_else(|_| err.to_string())
                } else {
                    err.to_string()
                }
            });
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
}
