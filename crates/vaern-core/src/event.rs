//! Messages from Tokio tasks to the JavaScript thread's event loop.
//!
//! Fetch and file completions, streamed chunks that waited for buffer space,
//! incoming server requests and server shutdowns all arrive on one channel. The event loop blocks on that channel, with the
//! nearest timer deadline as its timeout, so an idle runtime sleeps until
//! there is work instead of polling for it.

use crate::error::Result;
use crate::fetch::FetchResponse;
use crate::fs::FsOutput;
use crate::server::ServerIncomingRequest;

/// Outcome of a fetch started from JavaScript.
pub struct FetchCompletion {
    pub req_id: u32,
    pub result: Result<FetchResponse>,
}

/// Outcome of a `vaern:fs` operation started from JavaScript.
pub struct FsCompletion {
    pub req_id: u32,
    pub result: Result<FsOutput>,
}

/// Outcome of a `vaern:ai` model call, with the call it answers.
pub struct ModelCompletion {
    pub req_id: u32,
    pub call: crate::ai::PreparedCall,
    pub result: Result<crate::ai::ModelReply>,
}

/// Outcome of a streamed response chunk that had to wait for buffer space.
pub struct StreamChunkSent {
    pub req_id: u64,
    pub result: Result<()>,
}

/// Work for the JavaScript thread produced outside it.
pub enum LoopEvent {
    FetchDone(FetchCompletion),
    FsDone(FsCompletion),
    ModelDone(Box<ModelCompletion>),
    /// A text fragment of a streamed model call, by request id. Sent from
    /// the same task as its `ModelDone`, so every delta arrives before it.
    ModelDelta(u32, String),
    StreamChunkSent(StreamChunkSent),
    ServerRequest(ServerIncomingRequest),
    ServerClosed(u32),
}

/// Sending half of the event loop channel.
pub type EventSender = std::sync::mpsc::Sender<LoopEvent>;

/// Receiving half of the event loop channel, owned by the JavaScript thread.
pub type EventReceiver = std::sync::mpsc::Receiver<LoopEvent>;
