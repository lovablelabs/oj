use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tsrs_ast as ast;
use tsrs_core::collections::OrderedMap;
use tsrs_core::{CompilerOptions, Locale, P};
use tsrs_ipc as ipc;
use tsrs_spanmap::{MappingError, SpanMap};
use tsrs_tsoptions::{Mapper, OptionPathSegment};

use crate::hostimpl::{DiagnosticDirectivePolicy, PositionEncoding};

// TransformErrorKind identifies the stage at which a content mapper transform failed.
// host.go:14
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum TransformErrorKind {
    #[default]
    Unknown,
    Initialize,
    Project,
    Request,
    Response,
    Mappings,
}

// TransformError reports a failure while preparing, requesting, or decoding a transform.
// host.go:26
#[derive(Clone, Debug)]
pub struct TransformError {
    pub kind: TransformErrorKind,
    err: Option<Box<Error>>,
}

// NewTransformError creates a transform error for the given stage and underlying error.
// host.go:32 (None is Go's nil error.)
pub fn new_transform_error(kind: TransformErrorKind, err: Option<Error>) -> TransformError {
    TransformError { kind, err: err.map(Box::new) }
}

impl TransformError {
    // host.go:36
    fn error(&self) -> String {
        match &self.err {
            Some(err) => format!("content mapper transform failed: {err}"),
            // fmt's %v of a nil error.
            None => "content mapper transform failed: <nil>".to_string(),
        }
    }

    // host.go:40
    fn unwrap(&self) -> Option<&Error> {
        self.err.as_deref()
    }
}

// DiagnosticDirectiveErrorKind identifies why a diagnostic directive was rejected.
// host.go:43
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum DiagnosticDirectiveErrorKind {
    #[default]
    InvalidRange,
    InvalidPolicy,
    ExpectMissingUnusedDiagnostic,
    InvalidUnusedDiagnosticIndex,
    Overlap,
}

// DiagnosticDirectiveError reports an invalid diagnostic directive in a transform response.
// host.go:54 (`None` for a directive of the canonical output.)
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct DiagnosticDirectiveError {
    pub kind: DiagnosticDirectiveErrorKind,
    pub index: usize,
    pub supplemental_index: Option<usize>,
    pub policy: DiagnosticDirectivePolicy,
}

impl DiagnosticDirectiveError {
    // host.go:61
    fn error(&self) -> String {
        format!("invalid content mapper diagnostic directive {}", self.index)
    }
}

// InvalidVirtualExtensionError reports an unsupported or missing virtual extension on a mapped output.
// host.go:66
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct InvalidVirtualExtensionError {
    pub extension: String,
}

impl InvalidVirtualExtensionError {
    // host.go:70
    fn error(&self) -> String {
        format!("invalid virtual extension {}", quote(&self.extension))
    }
}

// ProjectErrorKind identifies why a mapper's openProject response was rejected.
// host.go:75
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum ProjectErrorKind {
    #[default]
    MalformedResponse,
    MissingConfigIdentity,
    NonAbsoluteWatchedFile,
    UnexpectedConfigIdentity,
    UnexpectedWatchedFiles,
}

// ProjectError reports an invalid mapper openProject response.
// host.go:86
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ProjectError {
    pub kind: ProjectErrorKind,
}

impl ProjectError {
    // host.go:90 (Go's default case, "content mapper returned an invalid project response", is for a kind outside
    // the named constants, which the Rust enum cannot hold.)
    fn error(self) -> String {
        match self.kind {
            ProjectErrorKind::MalformedResponse => {
                "content mapper returned a malformed project response"
            }
            ProjectErrorKind::MissingConfigIdentity => {
                "content mapper did not return configIdentity for dynamic configuration"
            }
            ProjectErrorKind::NonAbsoluteWatchedFile => {
                "content mapper returned a non-absolute path in watchedFiles"
            }
            ProjectErrorKind::UnexpectedConfigIdentity => {
                "content mapper returned configIdentity without declaring dynamicConfig"
            }
            ProjectErrorKind::UnexpectedWatchedFiles => {
                "content mapper returned watchedFiles without declaring dynamicConfig"
            }
        }
        .to_string()
    }
}

// InitializeErrorKind identifies why a mapper's initialize response was rejected.
// host.go:108
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum InitializeErrorKind {
    #[default]
    ProcessStart,
    ProcessExit,
    NoResponse,
    InvalidResponse,
    Request,
    PositionEncoding,
    EmptyDiagnosticSource,
    ReservedDiagnosticSource,
}

// InitializeError reports an invalid or unsupported mapper initialize response.
// host.go:122
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct InitializeError {
    pub kind: InitializeErrorKind,
    pub mapper_name: String,
    pub command: String,
    pub detail: String,
    pub exit_code: i32,
    pub timeout_seconds: i32,
    pub position_encoding: PositionEncoding,
    pub diagnostic_source: String,
}

// SupplementalFileCollisionError reports a compiler-assigned supplemental filename that already exists.
// host.go:134
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct SupplementalFileCollisionError {
    pub file_name: String,
}

impl SupplementalFileCollisionError {
    // host.go:138
    fn error(&self) -> String {
        format!("content mapper supplemental output file {} already exists", quote(&self.file_name))
    }
}

impl InitializeError {
    // host.go:142 (Go's default case, "content mapper initialization failed", is for a kind outside the named
    // constants, which the Rust enum cannot hold.)
    fn error(&self) -> String {
        match self.kind {
            InitializeErrorKind::ProcessStart => {
                format!(
                    "could not start content mapper command {}: {}",
                    quote(&self.command),
                    self.detail
                )
            }
            InitializeErrorKind::ProcessExit => {
                format!(
                    "content mapper process exited before initialization with code {}",
                    self.exit_code
                )
            }
            InitializeErrorKind::NoResponse => {
                "content mapper did not respond to the initialize request".to_string()
            }
            InitializeErrorKind::InvalidResponse => {
                format!("content mapper returned an invalid initialize response: {}", self.detail)
            }
            InitializeErrorKind::Request => {
                format!("content mapper initialize request failed: {}", self.detail)
            }
            InitializeErrorKind::PositionEncoding => {
                format!("unsupported position encoding {}", quote(&self.position_encoding.0))
            }
            InitializeErrorKind::EmptyDiagnosticSource => {
                "diagnostic source must not be empty".to_string()
            }
            InitializeErrorKind::ReservedDiagnosticSource => {
                format!(
                    "diagnostic source {} is reserved by TypeScript",
                    quote(&self.diagnostic_source)
                )
            }
        }
    }
}

// Result is the outcome of transforming a content-mapped source file into virtual TypeScript.
// host.go:166 (Go's `Result`, renamed so that it does not shadow std's Result where the crate is glob-imported.)
#[derive(Clone, Debug, Default)]
pub struct TransformResultFiles {
    // Text is the virtual TypeScript source text that is parsed into the program.
    pub text: String,
    // VirtualExtension determines how Text is parsed.
    pub virtual_extension: String,
    // Diagnostics are syntax errors in the original content.
    pub diagnostics: Vec<P<ast::Diagnostic>>,
    // Mappings maps positions in Text back to the original content, so that diagnostics the compiler
    // produces against the virtual text can be reported at their original locations. A successful
    // transform must return a non-nil map; an empty map describes fully synthesized output.
    pub mappings: Option<P<SpanMap>>,
    // DiagnosticDirectives control TypeScript diagnostics produced in virtual ranges.
    pub diagnostic_directives: Vec<ast::MappedDiagnosticDirective>,
    // Supplemental contains additional unnamed outputs associated with the canonical result.
    pub supplemental: Vec<MappedResult>,
}

// MappedResult is one virtual source file and its mapping to the original input.
// host.go:184
#[derive(Clone, Debug, Default)]
pub struct MappedResult {
    pub text: String,
    pub virtual_extension: String,
    pub mappings: Option<P<SpanMap>>,
    pub diagnostic_directives: Vec<ast::MappedDiagnosticDirective>,
}

// Request carries the inputs for transforming one content-mapped source file.
// host.go:192 (Go's strings share the caller's text; so do these borrows.)
#[derive(Clone, Copy, Debug, Default)]
pub struct Request<'a> {
    // FileName is the content-mapped source file being transformed.
    pub file_name: &'a str,
    // Content is the content-mapped source file's text.
    pub content: &'a str,
}

// ProjectSpec describes the project configuration visible to its content mappers.
// host.go:200 (Go keys project leases by the `*Mapper` and `*core.CompilerOptions` pointers: the mappers are
// references into a ParsedCommandLine's arena-allocated list, `ParsedCommandLine::content_mappers`, and the options
// are the command line's `P<CompilerOptions>`, so their addresses are the same stable identities.)
#[derive(Clone, Debug, Default)]
pub struct ProjectSpec {
    // ConfigFileName is the absolute project configuration file name, or empty for a project without one.
    pub config_file_name: String,
    // Mappers are the resolved content mapper entries configured for the project.
    pub mappers: Vec<&'static Mapper>,
    // CompilerOptions are the project's effective compiler options.
    pub compiler_options: Option<P<CompilerOptions>>,
}

// host.go:215
#[derive(Clone, Debug)]
pub struct OptionDiagnostic {
    pub mapper: &'static Mapper,
    pub path: Vec<OptionPathSegment>,
    pub source: String,
    pub code: i32,
    pub message_text: String,
}

// OperationTiming is the cumulative wall time and invocation count for one mapper operation.
// host.go:224
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct OperationTiming {
    pub count: u64,
    pub duration: Duration,
}

// MapperTimings is cumulative process and protocol activity for one resolved mapper identity.
// host.go:230
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct MapperTimings {
    pub spawn: OperationTiming,
    pub initialize: OperationTiming,
    pub open_project: OperationTiming,
    pub close_project: OperationTiming,
    pub transform: OperationTiming,
}

// Timings is a cumulative snapshot of content mapper process and protocol activity.
// host.go:239 (Go's map is iterated in random order; this one keeps the order mappers were first used in.)
#[derive(Clone, Debug, Default)]
pub struct Timings {
    pub mappers: OrderedMap<String, MapperTimings>,
    pub request_wait: Duration,
}

impl Timings {
    // Since returns the non-negative operation delta since previous.
}

// Project is the project-scoped view of a Host. It owns mapper configuration handles and provides the
// identities and watch dependencies needed for caching and incremental builds. Mapper projects are opened
// lazily when a transform is requested, or earlier when dynamic configuration is needed.
// host.go:273 (Implementations are shared between the file loader's parse threads: every method may be called
// from several threads at once.)
pub trait Project: Send + Sync {
    // Refresh closes opened mapper projects so they are reopened on the next transform or configuration identity query.
    // host.go:275
    fn refresh(&self) -> Result<(), Error>;
    // Identities returns sorted transform identities for all configured mappers. It returns an error if
    // dynamic project configuration cannot be opened or validated.
    // host.go:278 (Go returns them in the order of the project's mappers, not sorted, despite the comment.)
    fn identities(&self) -> Result<Vec<String>, Error>;
    // Identity returns the transform identity for mapper, or an empty string if mapper is not in this
    // project. It returns an error if dynamic project configuration cannot be opened or validated.
    // host.go:281 (`mapper` is found by address, as Go compares the *Mapper.)
    fn identity(&self, mapper: &Mapper) -> Result<String, Error>;
    // WatchedFiles returns the absolute files reported by mappers whose package.json declares dynamicConfig.
    // It returns an error if project configuration cannot be opened or validated.
    // host.go:284
    fn watched_files(&self) -> Result<Vec<String>, Error>;
    // Diagnostics returns option diagnostics cached by mapper projects that have already been opened.
    // host.go:286
    fn diagnostics(&self) -> Vec<OptionDiagnostic>;
    // Transform transforms one content-mapped source file using mapper in this project's configuration.
    // host.go:288
    fn transform(
        &self,
        mapper: &Mapper,
        request: Request<'_>,
    ) -> Result<TransformResultFiles, Error>;
    // Close releases this project reference and closes mapper project handles when no references remain.
    // host.go:290
    fn close(&self) -> Result<(), Error>;
}

// Host transforms otherwise unsupported file content into virtual TypeScript during program construction, by driving the
// configured content mappers. Create one with NewHost; Close tears down every mapper it spawned.
// host.go:295 (Go binds a host to a context.Context; the port has no context, so the owner calls `close`, and the
// host closes itself when it is dropped: hostimpl.rs.)
pub trait Host: Send + Sync {
    // Timings returns a cumulative snapshot of mapper process and protocol activity.
    // host.go:297
    fn timings(&self) -> Timings;
    // Project returns a retained project-scoped view for spec. Equivalent specs share underlying mapper
    // configuration state; the caller must close the returned Project.
    // host.go:300 (None is Go's nil Project, returned once the host is closed.)
    fn project(&self, spec: ProjectSpec) -> Option<Arc<dyn Project>>;
    // Acquire retains the processes for the given mapper identities until the returned lease is released.
    // Acquiring a mapper does not start its process; processes remain lazy until Transform is called.
    // host.go:303 (The release function may be called more than once; only the first call releases.)
    fn acquire(&self, mappers: &[&Mapper]) -> Box<dyn Fn() + Send + Sync>;
    // SetLocale updates the locale used to initialize mapper processes. Existing processes are stopped
    // and respawned lazily so subsequent transforms use the new locale.
    // host.go:306
    fn set_locale(&self, locale: Locale);
    // Transform maps a content-mapped source file to virtual TypeScript using the given content mapper
    // in a short-lived project with default compiler options.
    //
    // A non-nil error indicates the mapper itself failed to produce a result — for example the
    // host hit a broken pipe, a process crash, or could not deserialize the mapper's response.
    // host.go:312
    fn transform(
        &self,
        mapper: &'static Mapper,
        request: Request<'_>,
    ) -> Result<TransformResultFiles, Error>;
    // Close shuts down every mapper process the host spawned.
    // host.go:314
    fn close(&self) -> Result<(), Error>;
}

// contentmapper.go:26 ErrProjectUnavailable: the text of `Error::ProjectUnavailable`.
const ERR_PROJECT_UNAVAILABLE: &str = "content mapper project is unavailable";

// The Go `error` values of this package. Go's callers tell them apart with errors.AsType and errors.Is
// (compiler/fileloader.go:446-520, 592-640), which look through TransformError's Unwrap; the `as_*` methods and
// `is_project_unavailable` do the same. An error Go creates with errors.New, or with fmt.Errorf (whose `%w`
// operands here never are one of the typed errors), is `Other` with Go's text; an error from the mapper
// connection is `Ipc`, which keeps the ipc sentinels (`ipc::ErrorTag`) for errors.Is.
#[derive(Clone, Debug)]
#[expect(
    clippy::error_impl_error,
    reason = "the public name mirrors Go's package-level content-mapper error value"
)]
pub enum Error {
    // *TransformError
    Transform(TransformError),
    // *DiagnosticDirectiveError
    DiagnosticDirective(DiagnosticDirectiveError),
    // *InvalidVirtualExtensionError
    InvalidVirtualExtension(InvalidVirtualExtensionError),
    // *ProjectError
    Project(ProjectError),
    // *InitializeError (boxed: it is the largest of these errors, and every Result of the crate carries an Error).
    Initialize(Box<InitializeError>),
    // *SupplementalFileCollisionError
    SupplementalFileCollision(SupplementalFileCollisionError),
    // *spanmap.MappingError (ParseResult returns the span map's validation problem as it is).
    Mapping(MappingError),
    // ErrProjectUnavailable (contentmapper.go:26).
    ProjectUnavailable,
    // An error from the mapper connection (package ipc).
    Ipc(ipc::Error),
    // Any other error, by its text.
    Other(String),
}

impl Error {
    // Go `err.Error()`.
    pub fn error(&self) -> String {
        match self {
            Error::Transform(err) => err.error(),
            Error::DiagnosticDirective(err) => err.error(),
            Error::InvalidVirtualExtension(err) => err.error(),
            Error::Project(err) => err.error(),
            Error::Initialize(err) => err.error(),
            Error::SupplementalFileCollision(err) => err.error(),
            Error::Mapping(err) => err.error(),
            Error::ProjectUnavailable => ERR_PROJECT_UNAVAILABLE.to_string(),
            Error::Ipc(err) => err.to_string(),
            Error::Other(text) => text.clone(),
        }
    }

    // Go `errors.Unwrap(err)`: only a TransformError wraps another error.
    fn unwrap(&self) -> Option<&Error> {
        match self {
            Error::Transform(err) => err.unwrap(),
            _ => None,
        }
    }

    // The errors `errors.AsType` and `errors.Is` visit: err, then what it wraps.
    fn chain(&self) -> impl Iterator<Item = &Error> {
        std::iter::successors(Some(self), |err| err.unwrap())
    }

    // Go `errors.AsType[*TransformError](err)`.
    pub fn as_transform_error(&self) -> Option<&TransformError> {
        self.chain().find_map(|err| match err {
            Error::Transform(err) => Some(err),
            _ => None,
        })
    }

    // Go `errors.AsType[*DiagnosticDirectiveError](err)`.
    pub fn as_diagnostic_directive_error(&self) -> Option<&DiagnosticDirectiveError> {
        self.chain().find_map(|err| match err {
            Error::DiagnosticDirective(err) => Some(err),
            _ => None,
        })
    }

    // Go `errors.AsType[*InvalidVirtualExtensionError](err)`.
    pub fn as_invalid_virtual_extension_error(&self) -> Option<&InvalidVirtualExtensionError> {
        self.chain().find_map(|err| match err {
            Error::InvalidVirtualExtension(err) => Some(err),
            _ => None,
        })
    }

    // Go `errors.AsType[*ProjectError](err)`.
    pub fn as_project_error(&self) -> Option<&ProjectError> {
        self.chain().find_map(|err| match err {
            Error::Project(err) => Some(err),
            _ => None,
        })
    }

    // Go `errors.AsType[*InitializeError](err)`.
    pub fn as_initialize_error(&self) -> Option<&InitializeError> {
        self.chain().find_map(|err| match err {
            Error::Initialize(err) => Some(&**err),
            _ => None,
        })
    }

    // Go `errors.AsType[*SupplementalFileCollisionError](err)`.
    pub fn as_supplemental_file_collision_error(&self) -> Option<&SupplementalFileCollisionError> {
        self.chain().find_map(|err| match err {
            Error::SupplementalFileCollision(err) => Some(err),
            _ => None,
        })
    }

    // Go `errors.AsType[*spanmap.MappingError](err)`.
    pub fn as_mapping_error(&self) -> Option<&MappingError> {
        self.chain().find_map(|err| match err {
            Error::Mapping(err) => Some(err),
            _ => None,
        })
    }

    // Go `errors.Is(err, ErrProjectUnavailable)`.

    // Go `errors.Is(err, sentinel)` for the sentinels of package ipc (io.EOF, ipc.ErrConnClosed,
    // context.DeadlineExceeded).
    pub(crate) fn is(&self, tag: ipc::ErrorTag) -> bool {
        self.chain().any(|err| matches!(err, Error::Ipc(err) if err.is(tag)))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.error())
    }
}

impl std::error::Error for Error {}

impl From<TransformError> for Error {
    fn from(err: TransformError) -> Error {
        Error::Transform(err)
    }
}

impl From<DiagnosticDirectiveError> for Error {
    fn from(err: DiagnosticDirectiveError) -> Error {
        Error::DiagnosticDirective(err)
    }
}

impl From<InvalidVirtualExtensionError> for Error {
    fn from(err: InvalidVirtualExtensionError) -> Error {
        Error::InvalidVirtualExtension(err)
    }
}

impl From<ProjectError> for Error {
    fn from(err: ProjectError) -> Error {
        Error::Project(err)
    }
}

impl From<InitializeError> for Error {
    fn from(err: InitializeError) -> Error {
        Error::Initialize(Box::new(err))
    }
}

impl From<SupplementalFileCollisionError> for Error {
    fn from(err: SupplementalFileCollisionError) -> Error {
        Error::SupplementalFileCollision(err)
    }
}

impl From<MappingError> for Error {
    fn from(err: MappingError) -> Error {
        Error::Mapping(err)
    }
}

impl From<ipc::Error> for Error {
    fn from(err: ipc::Error) -> Error {
        Error::Ipc(err)
    }
}

// Go `strconv.Quote`, which fmt's %q verb uses for a string.
pub(crate) fn quote(s: &str) -> String {
    tsrs_core::gocompat::quote(s.as_bytes(), tsrs_core::gocompat::QuoteStyle::Header)
}
