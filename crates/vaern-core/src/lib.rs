//! Core runtime engine, TypeScript transpiler, and execution abstractions for Vaern.
//!
//! This crate provides the foundational building blocks for M1a:
//! - [`Transpiler`]: Type-stripping trait that transforms TypeScript to pure JavaScript without type-checking.
//! - [`Runtime`]: Execution trait encapsulating the QuickJS JavaScript engine and event loop.
//! - [`ModuleLoader`]: Trait resolving and loading relative local ES modules from disk.

pub mod error;
pub mod event;
pub mod fetch;
pub mod module_loader;
pub mod runtime;
pub mod server;
#[cfg(any(test, feature = "test-tls"))]
pub mod test_tls;
pub mod transpile;

pub use error::{Result, VaernError};
pub use fetch::{FetchOptions, FetchResponse, execute_fetch};
pub use module_loader::ModuleLoader;
pub use runtime::{QuickJsRuntime, Runtime, RuntimeOptions};
pub use server::{HttpServer, ServerIncomingRequest, ServerResponse, ServerResponseBody};
pub use transpile::{TranspileOptions, Transpiler};
