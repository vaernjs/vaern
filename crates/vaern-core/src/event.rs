//! Messages from Tokio tasks to the JavaScript thread's event loop.
//!
//! Fetch completions, incoming server requests and server shutdowns all arrive
//! on one channel. The event loop blocks on that channel, with the nearest
//! timer deadline as its timeout, so an idle runtime sleeps until there is
//! work instead of polling for it.

use crate::error::Result;
use crate::fetch::FetchResponse;
use crate::server::ServerIncomingRequest;

/// Outcome of a fetch started from JavaScript.
pub struct FetchCompletion {
    pub req_id: u32,
    pub result: Result<FetchResponse>,
}

/// Work for the JavaScript thread produced outside it.
pub enum LoopEvent {
    FetchDone(FetchCompletion),
    ServerRequest(ServerIncomingRequest),
    ServerClosed(u32),
}

/// Sending half of the event loop channel.
pub type EventSender = std::sync::mpsc::Sender<LoopEvent>;

/// Receiving half of the event loop channel, owned by the JavaScript thread.
pub type EventReceiver = std::sync::mpsc::Receiver<LoopEvent>;
