//! Defines the structured error types for the Vaern core runtime.
//!
//! This module provides actionable, context-rich error reporting for
//! TypeScript parsing, module resolution, runtime evaluation, and environment operations.
//! Every user-facing variant reports what failed, the source location (file:line:col),
//! and a recommended remediation action.

use thiserror::Error;

/// Core error enum representing all failure modes in the Vaern runtime.
#[derive(Debug, Error)]
pub enum VaernError {
    /// Failure accessing or reading a file on disk.
    #[error(
        "failed to read source file '{path}': {source}. Check file permissions and verify the path exists."
    )]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// Syntax error encountered while parsing JavaScript or TypeScript source.
    #[error(
        "syntax error in '{file}:{line}:{col}': {message}. Correct the syntax at the indicated location."
    )]
    Syntax {
        file: String,
        line: usize,
        col: usize,
        message: String,
    },

    /// TypeScript language construct that is intentionally rejected in M1a.
    #[error(
        "unsupported TypeScript feature '{feature}' in '{file}:{line}:{col}': {message}. Feature is not supported in M1a; rewrite using standard ES2022 syntax."
    )]
    UnsupportedFeature {
        file: String,
        line: usize,
        col: usize,
        feature: String,
        message: String,
    },

    /// Failure resolving an ES-module import. `tried` lists the paths and
    /// package.json fields consulted, in order.
    #[error(
        "module resolution failed for '{specifier}' imported by '{referrer}': {reason}.{}\n{action}",
        tried_list(.tried)
    )]
    ModuleResolution {
        specifier: String,
        referrer: String,
        reason: String,
        tried: Vec<String>,
        action: String,
    },

    /// Uncaught JavaScript runtime exception during execution.
    #[error(
        "runtime exception in '{file}': {message}\nInspect the call stack and handle the error."
    )]
    Execution { file: String, message: String },

    /// Engine initialization or configuration failure.
    #[error("engine initialization failed: {reason}. Check host system resource limits.")]
    EngineInit { reason: String },

    /// HTTP client fetch failure during execution.
    #[error("fetch failed at stage '{stage}' for '{url}': {message}. {action}")]
    Fetch {
        stage: String,
        url: String,
        message: String,
        action: String,
    },

    /// HTTP server failure during binding or execution.
    #[error("server failed on '{hostname}:{port}': {message}. {action}")]
    Server {
        hostname: String,
        port: u16,
        message: String,
        action: String,
    },

    /// `vaern:fs` failure. `stage` is `resolve`, `open`, `read` or `write`;
    /// `code` is a POSIX-style name such as `ENOENT`.
    #[error("fs {stage} failed for '{path}' ({code}): {message}. {action}")]
    Fs {
        stage: String,
        code: String,
        path: String,
        message: String,
        action: String,
    },

    /// Module source over the size the loader reads into the heap.
    #[error(
        "source file '{path}' is larger than the {limit_mib} MiB module size limit. Split the module, or read large data at run time with vaern:fs."
    )]
    SourceTooLarge { path: String, limit_mib: u64 },

    /// A streamed HTTP response chunk could not be handed to the connection.
    #[error("response stream for server request {req_id} failed: {message}. {action}")]
    ServerStream {
        req_id: u64,
        message: String,
        action: String,
    },
}

fn tried_list(tried: &[String]) -> String {
    tried.iter().map(|t| format!("\n  tried: {t}")).collect()
}

/// Convenience result type alias for Vaern core operations.
pub type Result<T> = std::result::Result<T, VaernError>;
