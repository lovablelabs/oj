// Contract between the checker lane and the core session (Go `snapshotData`, `checkerSetup` and the
// node-handle / source-file-descriptor helpers in tsc/internal/api/session.go).
//
// The checker module never owns snapshots, source-file leases, node index tables or the AST encoder;
// the core session implements `CheckerHost` and the checker handlers only borrow through it for the
// duration of one request.

use std::sync::Arc;

use tsrs_ast::{Node, SourceFile};
use tsrs_compiler::Program;
use tsrs_core::P;
use tsrs_core::context::Context;
use tsrs_core::json::Value;
use tsrs_project::Snapshot;

use super::registry::CheckerRegistry;

/// Go `ErrInvalidRequest` (params could not be decoded) vs `ErrClientError` (well-formed request that
/// refers to something invalid: unknown project, stale handle, wrong type kind, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckerErrorKind {
    InvalidRequest,
    Client,
    /// A dependency of the handler (codec node handles, source-file leases) is not available in this
    /// build; maps to core's `ErrorKind::Unsupported`, never to a fake success.
    Unsupported,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckerError {
    pub kind: CheckerErrorKind,
    pub message: String,
}

impl CheckerError {
    pub fn client(message: impl Into<String>) -> CheckerError {
        CheckerError { kind: CheckerErrorKind::Client, message: message.into() }
    }

    pub(crate) fn unsupported(message: impl Into<String>) -> CheckerError {
        CheckerError { kind: CheckerErrorKind::Unsupported, message: message.into() }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> CheckerError {
        CheckerError { kind: CheckerErrorKind::InvalidRequest, message: message.into() }
    }
}

impl std::fmt::Display for CheckerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            CheckerErrorKind::InvalidRequest => write!(f, "api: invalid request: {}", self.message),
            CheckerErrorKind::Client => write!(f, "api: client error: {}", self.message),
            CheckerErrorKind::Unsupported => write!(f, "api: unsupported: {}", self.message),
            CheckerErrorKind::Internal => f.write_str(&self.message),
        }
    }
}

pub type CheckerResult<T> = Result<T, CheckerError>;

/// A handler result. `EncodedNode` carries the binary AST encoding of a synthesized node
/// (typeToTypeNode / signatureToSignatureDeclaration); the transport sends it as raw binary (msgpack)
/// or as `{ "data": base64 }` (JSON-RPC), exactly like Go's `RawBinary` / `SourceFileResponse`.
#[derive(Clone, Debug, PartialEq)]
#[expect(
    clippy::too_long_first_doc_paragraph,
    reason = "Public for the integration tests; the comment keeps its upstream wording."
)]
pub enum CheckerResponse {
    Json(Value),
    EncodedNode(Vec<u8>),
}

/// A retained snapshot for the duration of one request. Core must keep the snapshot referenced (and its
/// programs, checkers and source files alive) until this scope is dropped; `release` runs on drop.
pub struct SnapshotScope {
    pub handle: u64,
    pub snapshot: Arc<Snapshot>,
    /// The snapshot's checker registries (Go `symbolRegistry` + `projectRegistries`). Core stores one
    /// per API snapshot handle and calls `CheckerRegistry::release` before dropping its last reference.
    pub registry: Arc<CheckerRegistry>,
    pub release: Option<Box<dyn FnOnce()>>,
}

impl Drop for SnapshotScope {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// A leased cached source file (Go `acquireCachedSourceFile` + `lease.Release`).
pub struct CachedFileScope {
    pub file: P<SourceFile>,
    pub release: Option<Box<dyn FnOnce()>>,
}

impl Drop for CachedFileScope {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Implemented by the core session.
pub trait CheckerHost {
    /// Request context (cancellation, locale). The checker module adds `CheckerLifetime::API`.
    fn context(&self) -> Context;

    /// Go `getSnapshotData`: an unknown or released handle is a client error.
    fn snapshot(&self, handle: u64) -> CheckerResult<SnapshotScope>;

    /// Go `acquireCachedSourceFile(descriptor)` for file-owned symbol references that carry no snapshot.
    fn acquire_cached_source_file(&self, descriptor: &Value) -> CheckerResult<CachedFileScope>;

    /// Go `newSourceFileDescriptor(file)` as its wire object
    /// `{ fileName, path, contentHash, parseOptionsKey, scriptKind, nodeId }`.
    fn source_file_descriptor(&self, file: P<SourceFile>) -> CheckerResult<Value>;

    /// `source_file_descriptor(file) == descriptor`, which a host may answer without building the descriptor.
    fn source_file_descriptor_matches(
        &self,
        file: P<SourceFile>,
        descriptor: &Value,
    ) -> CheckerResult<bool> {
        Ok(self.source_file_descriptor(file)? == *descriptor)
    }

    /// Go `sourceFileNodeID(file)` (used for `CompactSymbolReference.file`).
    fn source_file_node_id(&self, file: P<SourceFile>) -> CheckerResult<u64>;

    /// Go `nodeHandleFrom(node)`: `"<index>.<kind>.<path>"` using the encoder's node index table.
    fn node_handle(&self, node: P<Node>) -> CheckerResult<String>;

    /// Go `snapshotData.resolveNodeHandle(program, handle)`.
    fn resolve_node_handle(
        &self,
        program: &'static Program,
        handle: &str,
    ) -> CheckerResult<P<Node>>;

    /// Go `encoder.EncodeNode(node, nil)` for a synthesized node.
    fn encode_node(&self, node: P<Node>) -> CheckerResult<Vec<u8>>;

    /// Go `snapshotHost.CloneSnapshotWithAutoImports(ctx, base, file.ToURI(cwd), nil)`. The returned
    /// snapshot carries one reference owned by the caller (Go `defer preparedSnapshot.Deref()`).
    fn clone_snapshot_with_auto_imports(
        &self,
        base: &Snapshot,
        file_name: &str,
    ) -> CheckerResult<Arc<Snapshot>>;

    /// Go `if s.projectSession != nil { s.projectSession.TryAdoptSnapshotInBackground(base, prepared) }`: an
    /// LSP-attached session offers the snapshot it prepared for auto-imports to the language server.
    fn try_adopt_snapshot_in_background(&self, base: &Arc<Snapshot>, prepared: &Arc<Snapshot>);
}
