// Go's `error` values created or inspected by the protocol layer. Go code tests them with
// `errors.Is(err, lsproto.ErrorCodeX)`, `errors.Is(err, io.EOF)`, `errors.AsType[lsproto.ErrorCode](err)`;
// the port keeps the message text and the list of wrapped sentinels (`tags`, outermost first).

use std::fmt;

use crate::ErrorCode;
use crate::json::JsonError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorTag {
    Code(ErrorCode),
    // io.EOF / io.ErrUnexpectedEOF
    #[expect(
        clippy::upper_case_acronyms,
        reason = "The name matches the upstream Go or protocol vocabulary."
    )]
    EOF,
    UnexpectedEOF,
    // jsonrpc.ErrInvalidHeader / ErrInvalidContentLength / ErrNoContentLength / ErrInvalidJSONRPCVersion
    InvalidHeader,
    InvalidContentLength,
    NoContentLength,
    InvalidJSONRPCVersion,
    // context.Canceled / context.DeadlineExceeded (Go returns `ctx.Err()` as an error).
    ContextCanceled,
    ContextDeadlineExceeded,
    // A package-level sentinel error value or error type (`errors.Is(err, pkg.ErrFoo)`,
    // `errors.AsType[pkg.fooError](err)`), named by its Go identifier.
    Sentinel(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::error_impl_error,
    reason = "the public name mirrors Go's package-level error value used throughout the protocol port"
)]
pub struct Error {
    pub message: String,
    pub tags: Vec<ErrorTag>,
}

impl Error {
    // Go `errors.New(message)` / `fmt.Errorf` without `%w`.
    pub fn new(message: impl Into<String>) -> Error {
        Error { message: message.into(), tags: Vec::new() }
    }

    // A sentinel error.
    pub fn tagged(tag: ErrorTag, message: impl Into<String>) -> Error {
        Error { message: message.into(), tags: vec![tag] }
    }

    // Go `fmt.Errorf("%w: %w", code, err)`.
    pub fn wrap_code(code: ErrorCode, err: impl Into<Error>) -> Error {
        let err = err.into();
        let mut tags = vec![ErrorTag::Code(code)];
        tags.extend(err.tags);
        Error { message: format!("{}: {}", code, err.message), tags }
    }

    // Go `fmt.Errorf(prefix + "%w", err)`.
    pub fn wrap(prefix: &str, err: impl Into<Error>) -> Error {
        let err = err.into();
        Error { message: format!("{}{}", prefix, err.message), tags: err.tags }
    }

    // Go `errors.Is(err, sentinel)`.
    pub fn is(&self, tag: ErrorTag) -> bool {
        self.tags.contains(&tag)
    }

    // Go `errors.Is(err, lsproto.ErrorCodeX)`.
    pub fn is_code(&self, code: ErrorCode) -> bool {
        self.is(ErrorTag::Code(code))
    }

    // Go `errors.Is(err, io.EOF)`.
    pub fn is_eof(&self) -> bool {
        self.is(ErrorTag::EOF)
    }

    // Go `errors.AsType[lsproto.ErrorCode](err)`.
    pub fn as_error_code(&self) -> Option<ErrorCode> {
        self.tags.iter().find_map(|t| match t {
            ErrorTag::Code(code) => Some(*code),
            _ => None,
        })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<ErrorCode> for Error {
    fn from(code: ErrorCode) -> Error {
        Error { message: code.string(), tags: vec![ErrorTag::Code(code)] }
    }
}

impl From<tsrs_core::context::ContextError> for Error {
    fn from(err: tsrs_core::context::ContextError) -> Error {
        let tag = match err {
            tsrs_core::context::ContextError::Canceled => ErrorTag::ContextCanceled,
            tsrs_core::context::ContextError::DeadlineExceeded => ErrorTag::ContextDeadlineExceeded,
        };
        Error { message: err.to_string(), tags: vec![tag] }
    }
}

impl From<JsonError> for Error {
    fn from(err: JsonError) -> Error {
        Error {
            message: err.to_string(),
            tags: err.codes.iter().map(|c| ErrorTag::Code(*c)).collect(),
        }
    }
}
