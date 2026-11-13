//! One TCP connection between a client and an LLM server, and the requests
//! seen on it that are still waiting for their response.

use crate::wire::thread_key_from_body;
use serde_json::Value;

#[derive(Default)]
pub(crate) struct Flow {
    pub(crate) request_buf: String,
    pub(crate) response_buf: String,
    pub(crate) requests: Vec<CapturedRequest>,
    /// Thread key parsed from the current HTTP request's headers, waiting to
    /// be attached to the body object once it is complete.
    pub(crate) pending_thread: Option<String>,
    /// User-Agent of the current HTTP request, waiting to be attached to the
    /// body object once it is complete.
    pub(crate) pending_user_agent: Option<String>,
    pub(crate) server: Option<String>,
}

/// Captured requests kept per flow; the oldest is dropped first.
const MAX_CAPTURED_REQUESTS: usize = 20;

impl Flow {
    pub(crate) fn capture_request(&mut self, body: Value) {
        let thread = self
            .pending_thread
            .take()
            .or_else(|| thread_key_from_body(&body));
        self.requests.push(CapturedRequest {
            body,
            thread,
            user_agent: self.pending_user_agent.take(),
        });
        if self.requests.len() > MAX_CAPTURED_REQUESTS {
            self.requests.remove(0);
        }
    }
}

/// A captured request body plus the explicit thread identity it declared, if
/// any (headers or body fields).
#[derive(Clone)]
pub(crate) struct CapturedRequest {
    pub(crate) body: Value,
    pub(crate) thread: Option<String>,
    pub(crate) user_agent: Option<String>,
}
