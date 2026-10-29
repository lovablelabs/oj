// Adapters between the core session and the wire runtime (crates/tsrs_api_transport). Go's `api.Session`
// implements `ipc.Handler` and calls back through its `ipc.Conn`; the two crates keep their own handler
// types, so these adapt one to the other for every server that runs a session: `tsrs --api` and the
// language server's API sessions.

use std::sync::Arc;

use tsrs_api_transport as transport;

use crate::handler::{ApiError, ApiResult, ClientConn, Handler as _};
use crate::session::Session;

/// Serves a session's requests on a transport connection and closes the session when the connection is
/// dropped (contract: `crates/tsrs_api_transport/README.md`).
pub struct SessionHandler(pub Arc<Session>);

impl transport::Handler for SessionHandler {
    fn handle_request(
        &self,
        cx: &transport::RequestContext,
        method: &str,
        params: &[u8],
    ) -> Result<transport::Response, transport::ApiError> {
        // Go: errors are CodeInternalError with err.Error().
        match self.0.handle_nested_request(method, params, cx.depth) {
            // `echo` returns the client's bytes and `batchRequests` can embed an `echo` result: the transport
            // validates those as Go's json.Marshal does. Every other JSON result is `json::marshal` output.
            Ok(crate::Response::Json(s)) if matches!(method, "echo" | "batchRequests") => {
                Ok(transport::Response::Json(s.into_bytes()))
            }
            Ok(crate::Response::Json(s)) => Ok(transport::Response::ValidJson(s.into_bytes())),
            Ok(crate::Response::Binary(b)) => Ok(transport::Response::Binary(b)),
            Err(e) => Err(transport::ApiError::internal(e.to_string())),
        }
    }

    fn handle_notification(&self, _cx: &transport::RequestContext, method: &str, params: &[u8]) {
        let _ = self.0.handle_notification(method, params);
    }
}

impl Drop for SessionHandler {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The session's client connection (Go `Session.conn`) over a transport caller.
pub struct ClientConnAdapter(pub Arc<dyn transport::Caller>);

impl ClientConn for ClientConnAdapter {
    fn call(&self, method: &str, params: &str) -> ApiResult<String> {
        let raw = self
            .0
            .call(method, Some(params.as_bytes()))
            .map_err(|e| ApiError::internal(e.to_string()))?;
        tsrs_core::utf8::into_string(raw).map_err(|e| ApiError::internal(e.to_string()))
    }
}
