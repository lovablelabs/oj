// Node API checker lane (mfs-cx/node-api-checker): port of the checker / symbol / type / signature
// request handlers of the tsgo API (originally based on microsoft/TypeScript b85298b6, tsc/internal/api/session.go +
// proto.go). See checker/README.md for the contract, id ownership and the per-method coverage inventory.
//
// Core hook (docs/NODE_API.md, "checker hook"): `handle(session, method, params)` and
// `CheckerSnapshotState`. Internally the handlers run against the `CheckerHost` trait so they only
// borrow snapshots, node handles, source-file descriptors and the AST encoder for one request;
// `SessionHost` adapts core's `Session` to it.

pub mod dispatch;
mod handlers_ls;
mod handlers_symbols;
mod handlers_types;
pub mod host;
pub mod json;
pub mod lease;
mod params;
pub mod registry;
pub mod setup;
mod symbol_index;

use std::sync::Arc;

use tsrs_ast::{Node, SourceFile, Symbol};
use tsrs_compiler::Program;
use tsrs_core::P;
use tsrs_core::context::Context;
use tsrs_core::json::Value;
use tsrs_project::Snapshot;

pub use host::{
    CachedFileScope, CheckerError, CheckerErrorKind, CheckerHost, CheckerResponse, CheckerResult,
    SnapshotScope,
};
pub use registry::CheckerRegistry;

use crate::handler::{ApiError, ApiResult, ErrorKind, Response};
use crate::session::{Session, json_response};
use crate::wire::Obj;

/// Per-snapshot checker registries (Go `snapshotData.symbolRegistry` + `projectRegistries`). Core
/// creates it with `Default` when a snapshot is registered and drops it with the snapshot; dropping
/// releases every registered pointer.
#[derive(Default)]
#[expect(
    clippy::too_long_first_doc_paragraph,
    reason = "Public for the integration tests; the comment keeps its upstream wording."
)]
pub struct CheckerSnapshotState {
    pub registry: Arc<CheckerRegistry>,
}

impl Drop for CheckerSnapshotState {
    fn drop(&mut self) {
        self.registry.release();
    }
}

/// Core entry point. `None` only for methods this lane does not recognize.
pub(crate) fn handle(
    session: &Session,
    method: &str,
    params: &Value,
) -> Option<ApiResult<Response>> {
    let host = SessionHost { session };
    let result = dispatch::handle(&host, method, params)?;
    Some(result.map_err(to_api_error).and_then(|response| match response {
        CheckerResponse::Json(value) => json_response(&value),
        CheckerResponse::EncodedNode(bytes) => {
            if session.binary_responses() {
                Ok(Response::Binary(bytes))
            } else {
                json_response(&Obj::new().set("data", Value::String(base64_encode(&bytes))).build())
            }
        }
    }))
}

/// Go `newFileSymbolResponse`, for core handlers that return a binder symbol from an ordinary cached AST.
pub(crate) fn file_symbol_response(session: &Session, symbol: P<Symbol>) -> ApiResult<Value> {
    setup::file_symbol_response(&SessionHost { session }, symbol).map_err(to_api_error)
}

fn to_api_error(e: CheckerError) -> ApiError {
    match e.kind {
        CheckerErrorKind::InvalidRequest => ApiError::invalid_request(e.message),
        CheckerErrorKind::Client => ApiError::client(e.message),
        CheckerErrorKind::Unsupported => {
            ApiError { kind: ErrorKind::Unsupported, message: e.message }
        }
        CheckerErrorKind::Internal => ApiError::internal(e.message),
    }
}

fn from_api_error(e: ApiError) -> CheckerError {
    match e.kind {
        ErrorKind::InvalidRequest => CheckerError::invalid(e.message),
        ErrorKind::ClientError => CheckerError::client(e.message),
        ErrorKind::Unsupported => CheckerError::unsupported(e.message),
        ErrorKind::Internal => {
            CheckerError { kind: CheckerErrorKind::Internal, message: e.message }
        }
    }
}

/// Adapts core's `Session` to `CheckerHost`.
struct SessionHost<'a> {
    session: &'a Session,
}

impl CheckerHost for SessionHost<'_> {
    fn context(&self) -> Context {
        Context::background()
    }

    fn snapshot(&self, handle: u64) -> CheckerResult<SnapshotScope> {
        let sd = self.session.snapshot_data(handle).map_err(from_api_error)?;
        let snapshot = Arc::clone(&sd.snapshot);
        let registry = Arc::clone(&sd.checker_state.registry);
        // The scope pins `SnapshotData` (and with it the project snapshot) until the request ends.
        Ok(SnapshotScope { handle, snapshot, registry, release: Some(Box::new(move || drop(sd))) })
    }

    fn acquire_cached_source_file(&self, descriptor: &Value) -> CheckerResult<CachedFileScope> {
        let lease = self.session.acquire_cached_source_file(descriptor).map_err(from_api_error)?;
        Ok(CachedFileScope {
            file: lease.source_file(),
            release: Some(Box::new(move || lease.release())),
        })
    }

    fn source_file_descriptor(&self, file: P<SourceFile>) -> CheckerResult<Value> {
        Ok(crate::sourcefiles::source_file_descriptor(file))
    }

    fn source_file_descriptor_matches(
        &self,
        file: P<SourceFile>,
        descriptor: &Value,
    ) -> CheckerResult<bool> {
        Ok(crate::sourcefiles::source_file_descriptor_matches(file, descriptor))
    }

    fn source_file_node_id(&self, file: P<SourceFile>) -> CheckerResult<u64> {
        Ok(crate::sourcefiles::source_file_node_id(file))
    }

    fn node_handle(&self, node: P<Node>) -> CheckerResult<String> {
        self.session.node_handle_from(node).map_err(from_api_error)
    }

    fn resolve_node_handle(
        &self,
        program: &'static Program,
        handle: &str,
    ) -> CheckerResult<P<Node>> {
        self.session.resolve_node_handle(program, handle).map_err(from_api_error)
    }

    fn encode_node(&self, node: P<Node>) -> CheckerResult<Vec<u8>> {
        Session::encode_node(node).map_err(from_api_error)
    }

    fn clone_snapshot_with_auto_imports(
        &self,
        base: &Snapshot,
        file_name: &str,
    ) -> CheckerResult<Arc<Snapshot>> {
        let file_name = tsrs_core::tspath::get_normalized_absolute_path(
            file_name,
            self.session.current_directory(),
        );
        let uri = tsrs_ls::lsconv::file_name_to_document_uri(&file_name);
        Ok(self.session.snapshot_host.clone_snapshot_with_auto_imports(
            &self.context(),
            base,
            &uri,
            None,
        ))
    }

    fn try_adopt_snapshot_in_background(&self, base: &Arc<Snapshot>, prepared: &Arc<Snapshot>) {
        if let Some(project_session) = &self.session.project_session {
            project_session
                .try_adopt_snapshot_in_background(Arc::clone(base), Arc::clone(prepared));
        }
    }
}

/// Standard base64 with padding (Go `base64.StdEncoding`).
pub fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    out.extend(data.chunks(3).flat_map(|chunk| {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        [
            ALPHABET[((n >> 18) & 63) as usize] as char,
            ALPHABET[((n >> 12) & 63) as usize] as char,
            if chunk.len() > 1 { ALPHABET[((n >> 6) & 63) as usize] as char } else { '=' },
            if chunk.len() > 2 { ALPHABET[(n & 63) as usize] as char } else { '=' },
        ]
    }));
    out
}
