//! `vaern:mcp`: Model Context Protocol servers written in JavaScript and
//! served by the runtime, and the client that spawns and calls them.
//!
//! The runtime owns every pipe, the JSON-RPC framing, version negotiation
//! and the server process's life. Scripts see tool handles and server
//! declarations only, so an MCP call is an effect at a Rust bridge like
//! `fetch` or a model call, journalled and metered there.

pub mod protocol;
