use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{
    Arc, Mutex, MutexGuard, Once, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
    Weak,
};
use std::thread;
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast as ast;
use tsrs_core::collections::OrderedMap;
use tsrs_core::json::{self, JsonMap, Value};
use tsrs_core::stringutil::equal_fold;
use tsrs_core::{CompilerOptions, Locale, P, TextRange, alloc_str, goslices, tspath};
use tsrs_diagnostics::Category;
use tsrs_ipc::jsonrpc::{self, ID, ResponseError};
use tsrs_ipc::{self as ipc, Closer, Conn, Handler, Message, Protocol, ReadWriteCloser};
use tsrs_spanmap::SpanMap;
use tsrs_tsoptions::gojson::compiler_options_to_go_json;
use tsrs_tsoptions::{Mapper, OptionPathSegment, is_supported_virtual_extension};

use crate::MapperTimings;
use crate::host::{
    DiagnosticDirectiveError, DiagnosticDirectiveErrorKind, Error, Host, InitializeError,
    InitializeErrorKind, InvalidVirtualExtensionError, MappedResult, OperationTiming,
    OptionDiagnostic, Project, ProjectError, ProjectErrorKind, ProjectSpec, Request, Timings,
    TransformErrorKind, TransformResultFiles, new_transform_error, quote,
};

// hostimpl.go:30
const INITIALIZE_TIMEOUT_SECONDS: i32 = 5;

// hostimpl.go:32
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(INITIALIZE_TIMEOUT_SECONDS as u64);

// Content mapper protocol method names.
// hostimpl.go:35
pub const METHOD_INITIALIZE: &str = "initialize";
pub const METHOD_OPEN_PROJECT: &str = "openProject";
pub const METHOD_CLOSE_PROJECT: &str = "closeProject";
pub const METHOD_TRANSFORM: &str = "transform";

// Go `json.Marshal` and `json.Unmarshal` (encoding/json/v2) of the protocol types below, following their struct tags:
// members in struct order, an `omitempty` member left out when it would be null, "", {} or [], and a nil slice
// written as []; when decoding, member names match exactly, unknown members are ignored, a JSON null leaves the zero
// value, and a value of the wrong kind is an error naming the Go type and the JSON pointer. A Go `json.Value` member
// is an `Option<Value>`, None when it is empty (absent). The values are the tsrs_core::json trees the connection
// carries; decoding takes them by value so that the transform's text is moved, not copied.
pub trait ProtocolJson: Sized {
    // Go `json.Marshal(v)`.
    fn marshal_json(&self) -> Value;
    // Go `json.Unmarshal(value, &v)`; `pointer` is the value's JSON pointer within the document, for errors.
    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String>;
}

// Go `json.Unmarshal(raw, &v)` of a message's params or result, which is empty (None) when the member was absent.
pub fn unmarshal<T: ProtocolJson>(raw: Option<Value>) -> Result<T, String> {
    match raw {
        Some(value) => T::unmarshal_json(value, ""),
        None => Err("unexpected EOF".to_string()),
    }
}

// InitializeParams is the parameter object for the initialize request.
// hostimpl.go:43
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct InitializeParams {
    // Locale is the BCP 47 locale to use for mapper-authored diagnostic messages, when configured.
    pub locale: String,
    // PositionEncodings lists the coordinate spaces the host accepts.
    pub position_encodings: Vec<PositionEncoding>,
}

impl ProtocolJson for InitializeParams {
    fn marshal_json(&self) -> Value {
        let mut o = JsonMap::new();
        if !self.locale.is_empty() {
            o.insert("locale".to_string(), Value::String(self.locale.clone()));
        }
        let encodings = self
            .position_encodings
            .iter()
            .map(|encoding| Value::String(encoding.0.to_string()))
            .collect();
        o.insert("positionEncodings".to_string(), Value::Array(encodings));
        Value::Object(o)
    }

    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let mut v = InitializeParams::default();
        for (name, member) in unmarshal_struct(value, "contentmapper.InitializeParams", pointer)?
            .into_iter()
            .flatten()
        {
            let pointer = &member_pointer(pointer, &name);
            match name.as_str() {
                "locale" => v.locale = unmarshal_string(member, pointer)?,
                "positionEncodings" => {
                    v.position_encodings = unmarshal_slice(
                        member,
                        "[]contentmapper.PositionEncoding",
                        pointer,
                        |element, pointer| {
                            Ok(PositionEncoding(Cow::Owned(unmarshal_string(element, pointer)?)))
                        },
                    )?;
                }
                _ => {}
            }
        }
        Ok(v)
    }
}

// InitializeResult is the mapper's response to the initialize request.
// hostimpl.go:51
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct InitializeResult {
    // PositionEncoding selects the coordinate space for all mappings and diagnostics.
    pub position_encoding: PositionEncoding,
    // DiagnosticSource is the prefix used for every mapper-authored diagnostic code.
    pub diagnostic_source: String,
}

impl ProtocolJson for InitializeResult {
    fn marshal_json(&self) -> Value {
        let mut o = JsonMap::new();
        o.insert(
            "positionEncoding".to_string(),
            Value::String(self.position_encoding.0.to_string()),
        );
        o.insert("diagnosticSource".to_string(), Value::String(self.diagnostic_source.clone()));
        Value::Object(o)
    }

    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let mut v = InitializeResult::default();
        for (name, member) in unmarshal_struct(value, "contentmapper.InitializeResult", pointer)?
            .into_iter()
            .flatten()
        {
            let pointer = &member_pointer(pointer, &name);
            match name.as_str() {
                "positionEncoding" => {
                    v.position_encoding =
                        PositionEncoding(Cow::Owned(unmarshal_string(member, pointer)?));
                }
                "diagnosticSource" => v.diagnostic_source = unmarshal_string(member, pointer)?,
                _ => {}
            }
        }
        Ok(v)
    }
}

// OpenProjectParams is the parameter object for the openProject request.
// hostimpl.go:59
#[derive(Clone, PartialEq, Debug, Default)]
pub struct OpenProjectParams {
    // ConfigFileName is the absolute project configuration file name, or empty when there is none.
    pub config_file_name: String,
    // ProjectHandle is an opaque, process-local handle assigned by the host.
    pub project_handle: String,
    // Options is the mapper entry's options from the project's contentMappers configuration.
    pub options: Option<Value>,
    // CompilerOptions contains the project's effective compiler options.
    pub compiler_options: Option<Value>,
}

impl ProtocolJson for OpenProjectParams {
    fn marshal_json(&self) -> Value {
        let mut o = JsonMap::new();
        o.insert("configFileName".to_string(), Value::String(self.config_file_name.clone()));
        o.insert("projectHandle".to_string(), Value::String(self.project_handle.clone()));
        if let Some(options) = self.options.as_ref().filter(|options| !is_empty_json(options)) {
            o.insert("options".to_string(), options.clone());
        }
        o.insert("compilerOptions".to_string(), raw_json(self.compiler_options.as_ref()));
        Value::Object(o)
    }

    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let mut v = OpenProjectParams::default();
        for (name, member) in unmarshal_struct(value, "contentmapper.OpenProjectParams", pointer)?
            .into_iter()
            .flatten()
        {
            let pointer = &member_pointer(pointer, &name);
            match name.as_str() {
                "configFileName" => v.config_file_name = unmarshal_string(member, pointer)?,
                "projectHandle" => v.project_handle = unmarshal_string(member, pointer)?,
                "options" => v.options = Some(member),
                "compilerOptions" => v.compiler_options = Some(member),
                _ => {}
            }
        }
        Ok(v)
    }
}

// OpenProjectResult is the mapper's response to an openProject request. ConfigIdentity and WatchedFiles
// may only be returned by mappers that declare dynamicConfig.
// hostimpl.go:72
#[derive(Clone, PartialEq, Debug, Default)]
pub struct OpenProjectResult {
    // ConfigIdentity is a stable fingerprint of all dynamic configuration that can affect transforms.
    pub config_identity: String,
    // WatchedFiles are absolute files whose changes may alter ConfigIdentity or transform output.
    pub watched_files: Vec<String>,
    // OptionDiagnostics report invalid mapper options. Paths are relative to the mapper entry's options object.
    pub option_diagnostics: Vec<OptionDiagnosticResult>,
}

impl ProtocolJson for OpenProjectResult {
    fn marshal_json(&self) -> Value {
        let mut o = JsonMap::new();
        o.insert("configIdentity".to_string(), Value::String(self.config_identity.clone()));
        if !self.watched_files.is_empty() {
            let files = self.watched_files.iter().map(|file| Value::String(file.clone())).collect();
            o.insert("watchedFiles".to_string(), Value::Array(files));
        }
        if !self.option_diagnostics.is_empty() {
            let diagnostics =
                self.option_diagnostics.iter().map(ProtocolJson::marshal_json).collect();
            o.insert("optionDiagnostics".to_string(), Value::Array(diagnostics));
        }
        Value::Object(o)
    }

    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let mut v = OpenProjectResult::default();
        for (name, member) in unmarshal_struct(value, "contentmapper.OpenProjectResult", pointer)?
            .into_iter()
            .flatten()
        {
            let pointer = &member_pointer(pointer, &name);
            match name.as_str() {
                "configIdentity" => v.config_identity = unmarshal_string(member, pointer)?,
                "watchedFiles" => {
                    v.watched_files =
                        unmarshal_slice(member, "[]string", pointer, unmarshal_string)?;
                }
                "optionDiagnostics" => {
                    v.option_diagnostics = unmarshal_slice(
                        member,
                        "[]contentmapper.OptionDiagnosticResult",
                        pointer,
                        OptionDiagnosticResult::unmarshal_json,
                    )?;
                }
                _ => {}
            }
        }
        Ok(v)
    }
}

// hostimpl.go:81
#[derive(Clone, PartialEq, Debug, Default)]
pub struct OptionDiagnosticResult {
    pub path: Vec<Value>,
    pub message_text: String,
    pub code: i32,
}

impl ProtocolJson for OptionDiagnosticResult {
    fn marshal_json(&self) -> Value {
        let mut o = JsonMap::new();
        o.insert("path".to_string(), Value::Array(self.path.clone()));
        o.insert("messageText".to_string(), Value::String(self.message_text.clone()));
        o.insert("code".to_string(), Value::Number(f64::from(self.code)));
        Value::Object(o)
    }

    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let mut v = OptionDiagnosticResult::default();
        for (name, member) in
            unmarshal_struct(value, "contentmapper.OptionDiagnosticResult", pointer)?
                .into_iter()
                .flatten()
        {
            let pointer = &member_pointer(pointer, &name);
            match name.as_str() {
                "path" => {
                    v.path = unmarshal_slice(member, "[]jsontext.Value", pointer, |element, _| {
                        Ok(element)
                    })?;
                }
                "messageText" => v.message_text = unmarshal_string(member, pointer)?,
                "code" => v.code = unmarshal_int32(&member, "int32", pointer)?,
                _ => {}
            }
        }
        Ok(v)
    }
}

// CloseProjectParams is the parameter object for the closeProject request.
// hostimpl.go:88
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CloseProjectParams {
    pub project_handle: String,
}

impl ProtocolJson for CloseProjectParams {
    fn marshal_json(&self) -> Value {
        let mut o = JsonMap::new();
        o.insert("projectHandle".to_string(), Value::String(self.project_handle.clone()));
        Value::Object(o)
    }

    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let mut v = CloseProjectParams::default();
        for (name, member) in unmarshal_struct(value, "contentmapper.CloseProjectParams", pointer)?
            .into_iter()
            .flatten()
        {
            if name == "projectHandle" {
                v.project_handle = unmarshal_string(member, &member_pointer(pointer, &name))?;
            }
        }
        Ok(v)
    }
}

// PositionEncoding is the coordinate space a mapper uses for mappings and diagnostics.
// hostimpl.go:93 (Go `type PositionEncoding string`: a decoded value can be any string, which handshake rejects.)
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct PositionEncoding(pub Cow<'static, str>);

impl PositionEncoding {
    // hostimpl.go:96
    pub const UTF8: PositionEncoding = PositionEncoding(Cow::Borrowed("utf-8"));
    pub const UTF16: PositionEncoding = PositionEncoding(Cow::Borrowed("utf-16"));
}

impl fmt::Display for PositionEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// TransformParams is the parameter object for the transform request.
// hostimpl.go:101
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TransformParams {
    // FileName is the absolute name of the content-mapped source file being transformed.
    pub file_name: String,
    // Content is the content-mapped source file's text.
    pub content: String,
    // ProjectHandle identifies the mapper project configuration opened for this transform.
    pub project_handle: String,
}

impl ProtocolJson for TransformParams {
    fn marshal_json(&self) -> Value {
        let mut o = JsonMap::new();
        o.insert("fileName".to_string(), Value::String(self.file_name.clone()));
        o.insert("content".to_string(), Value::String(self.content.clone()));
        o.insert("projectHandle".to_string(), Value::String(self.project_handle.clone()));
        Value::Object(o)
    }

    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let mut v = TransformParams::default();
        for (name, member) in
            unmarshal_struct(value, "contentmapper.TransformParams", pointer)?.into_iter().flatten()
        {
            let pointer = &member_pointer(pointer, &name);
            match name.as_str() {
                "fileName" => v.file_name = unmarshal_string(member, pointer)?,
                "content" => v.content = unmarshal_string(member, pointer)?,
                "projectHandle" => v.project_handle = unmarshal_string(member, pointer)?,
                _ => {}
            }
        }
        Ok(v)
    }
}

// MappedOutput is virtual source text and its mapping to an original input.
// hostimpl.go:111
#[derive(Clone, PartialEq, Debug, Default)]
pub struct MappedOutput {
    // Text is the virtual JavaScript or TypeScript source text.
    pub text: String,
    // Extension determines the virtual source file's syntax.
    pub extension: String,
    // Mappings is the span map's tuple-array JSON (see spanmap.Marshal), expressed in the selected
    // position encoding. Absent or empty means the output is fully synthesized.
    pub mappings: Option<Value>,
    // DiagnosticDirectives describe framework directives that suppress TypeScript diagnostics in
    // virtual ranges and optionally report an error when no diagnostic is produced.
    pub diagnostic_directives: Option<DiagnosticDirectives>,
}

impl MappedOutput {
    // The members of a MappedOutput, which TransformResult and SupplementalOutput embed (Go inlines an embedded
    // struct's fields into the parent object).
    fn marshal_members(&self, o: &mut JsonMap) {
        o.insert("text".to_string(), Value::String(self.text.clone()));
        o.insert("extension".to_string(), Value::String(self.extension.clone()));
        if let Some(mappings) = self.mappings.as_ref().filter(|mappings| !is_empty_json(mappings)) {
            o.insert("mappings".to_string(), mappings.clone());
        }
        if let Some(diagnostic_directives) = &self.diagnostic_directives {
            o.insert("diagnosticDirectives".to_string(), diagnostic_directives.marshal_json());
        }
    }

    // Decodes `member` if it is one of MappedOutput's; gives it back otherwise.
    fn unmarshal_member(
        &mut self,
        name: &str,
        member: Value,
        pointer: &str,
    ) -> Result<Option<Value>, String> {
        match name {
            "text" => self.text = unmarshal_string(member, pointer)?,
            "extension" => self.extension = unmarshal_string(member, pointer)?,
            "mappings" => self.mappings = Some(member),
            "diagnosticDirectives" => {
                self.diagnostic_directives = match member {
                    Value::Null => None,
                    member => Some(DiagnosticDirectives::unmarshal_json(member, pointer)?),
                };
            }
            _ => return Ok(Some(member)),
        }
        Ok(None)
    }
}

impl ProtocolJson for MappedOutput {
    fn marshal_json(&self) -> Value {
        let mut o = JsonMap::new();
        self.marshal_members(&mut o);
        Value::Object(o)
    }

    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let mut v = MappedOutput::default();
        for (name, member) in
            unmarshal_struct(value, "contentmapper.MappedOutput", pointer)?.into_iter().flatten()
        {
            let member_pointer = member_pointer(pointer, &name);
            v.unmarshal_member(&name, member, &member_pointer)?;
        }
        Ok(v)
    }
}

// DiagnosticDirectivePolicy is the numeric policy stored in a mapped diagnostic directive tuple.
// hostimpl.go:125 (Go `type DiagnosticDirectivePolicy uint8`: a decoded tuple can carry any uint8, which
// normalizeDiagnosticDirectives rejects. Named values are associated constants, like `tsrs_spanmap::Kind`.)
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct DiagnosticDirectivePolicy(pub u8);

impl DiagnosticDirectivePolicy {
    // hostimpl.go:128
    pub const IGNORE: DiagnosticDirectivePolicy = DiagnosticDirectivePolicy(0);
    pub const EXPECT: DiagnosticDirectivePolicy = DiagnosticDirectivePolicy(1);
}

impl fmt::Display for DiagnosticDirectivePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// hostimpl.go:132
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct UnusedExpectDirectiveDiagnostic {
    pub code: i32,
    pub message_text: String,
}

impl ProtocolJson for UnusedExpectDirectiveDiagnostic {
    fn marshal_json(&self) -> Value {
        let mut o = JsonMap::new();
        o.insert("code".to_string(), Value::Number(f64::from(self.code)));
        o.insert("messageText".to_string(), Value::String(self.message_text.clone()));
        Value::Object(o)
    }

    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let mut v = UnusedExpectDirectiveDiagnostic::default();
        for (name, member) in
            unmarshal_struct(value, "contentmapper.UnusedExpectDirectiveDiagnostic", pointer)?
                .into_iter()
                .flatten()
        {
            let pointer = &member_pointer(pointer, &name);
            match name.as_str() {
                "code" => v.code = unmarshal_int32(&member, "int32", pointer)?,
                "messageText" => v.message_text = unmarshal_string(member, pointer)?,
                _ => {}
            }
        }
        Ok(v)
    }
}

// DiagnosticDirectives shares unused-expect diagnostics across compact directive tuples.
// hostimpl.go:138
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct DiagnosticDirectives {
    pub unused_expect_directive_diagnostics: Vec<UnusedExpectDirectiveDiagnostic>,
    pub directives: Vec<MappedDiagnosticDirective>,
}

impl ProtocolJson for DiagnosticDirectives {
    fn marshal_json(&self) -> Value {
        let mut o = JsonMap::new();
        let unused = self
            .unused_expect_directive_diagnostics
            .iter()
            .map(ProtocolJson::marshal_json)
            .collect();
        o.insert("unusedExpectDirectiveDiagnostics".to_string(), Value::Array(unused));
        let directives = self.directives.iter().map(ProtocolJson::marshal_json).collect();
        o.insert("directives".to_string(), Value::Array(directives));
        Value::Object(o)
    }

    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let mut v = DiagnosticDirectives::default();
        for (name, member) in
            unmarshal_struct(value, "contentmapper.DiagnosticDirectives", pointer)?
                .into_iter()
                .flatten()
        {
            let pointer = &member_pointer(pointer, &name);
            match name.as_str() {
                "unusedExpectDirectiveDiagnostics" => {
                    v.unused_expect_directive_diagnostics = unmarshal_slice(
                        member,
                        "[]contentmapper.UnusedExpectDirectiveDiagnostic",
                        pointer,
                        UnusedExpectDirectiveDiagnostic::unmarshal_json,
                    )?;
                }
                "directives" => {
                    v.directives = unmarshal_slice(
                        member,
                        "[]contentmapper.MappedDiagnosticDirective",
                        pointer,
                        MappedDiagnosticDirective::unmarshal_json,
                    )?;
                }
                _ => {}
            }
        }
        Ok(v)
    }
}

// MappedDiagnosticDirective is encoded as
// [originalStart, originalLength, virtualStart, virtualEnd, policy, unusedExpectDirectiveIndex?].
// An omitted index selects the only unused-expect diagnostic and is invalid when there is not exactly one.
// hostimpl.go:146 (Go `int` positions are i64.)
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct MappedDiagnosticDirective {
    pub original_start: i64,
    pub original_length: i64,
    pub virtual_start: i64,
    pub virtual_end: i64,
    pub policy: DiagnosticDirectivePolicy,
    pub unused_expect_directive_index: Option<i64>,
}

impl ProtocolJson for MappedDiagnosticDirective {
    // hostimpl.go:160 MarshalJSONTo
    fn marshal_json(&self) -> Value {
        let mut tuple = vec![
            int_json(self.original_start),
            int_json(self.original_length),
            int_json(self.virtual_start),
            int_json(self.virtual_end),
            Value::Number(f64::from(self.policy.0)),
        ];
        if let Some(index) = self.unused_expect_directive_index {
            tuple.push(int_json(index));
        }
        Value::Array(tuple)
    }

    // hostimpl.go:168 UnmarshalJSONFrom (json/v2 prefixes the method's error with the value it was decoding.)
    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let semantic_error = |err: String| {
            format!(
                "{}: {err}",
                type_error(json_kind(&value), "contentmapper.MappedDiagnosticDirective", pointer)
            )
        };
        let tuple: &[Value] = match &value {
            Value::Array(tuple) => tuple,
            Value::Null => &[],
            value => {
                return Err(semantic_error(type_error(json_kind(value), "[]jsontext.Value", "")));
            }
        };
        if tuple.len() != 5 && tuple.len() != 6 {
            return Err(semantic_error(format!(
                "diagnostic directive tuple must contain 5 or 6 elements, got {}",
                tuple.len()
            )));
        }
        let element = |i: usize| {
            unmarshal_int(&tuple[i], "int", i64::MIN, i64::MAX, "").map_err(|err| {
                semantic_error(format!("invalid diagnostic directive tuple element {i}: {err}"))
            })
        };
        let mut d = MappedDiagnosticDirective {
            original_start: element(0)?,
            original_length: element(1)?,
            virtual_start: element(2)?,
            virtual_end: element(3)?,
            ..Default::default()
        };
        d.policy = DiagnosticDirectivePolicy(
            unmarshal_int(
                &tuple[4],
                "contentmapper.DiagnosticDirectivePolicy",
                0,
                i64::from(u8::MAX),
                "",
            )
            .map_err(|err| {
                semantic_error(format!("invalid diagnostic directive tuple element 4: {err}"))
            })? as u8,
        );
        if tuple.len() == 6 {
            d.unused_expect_directive_index = Some(element(5)?);
        }
        Ok(d)
    }
}

// hostimpl.go:193
#[derive(Clone, PartialEq, Debug, Default)]
pub struct SupplementalOutput {
    pub mapped_output: MappedOutput,
}

impl ProtocolJson for SupplementalOutput {
    fn marshal_json(&self) -> Value {
        self.mapped_output.marshal_json()
    }

    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let mut v = SupplementalOutput::default();
        for (name, member) in unmarshal_struct(value, "contentmapper.SupplementalOutput", pointer)?
            .into_iter()
            .flatten()
        {
            let member_pointer = member_pointer(pointer, &name);
            v.mapped_output.unmarshal_member(&name, member, &member_pointer)?;
        }
        Ok(v)
    }
}

// TransformResult is the canonical output for one input file.
// hostimpl.go:198
#[derive(Clone, PartialEq, Debug, Default)]
pub struct TransformResult {
    pub mapped_output: MappedOutput,
    // Diagnostics are mapper-authored errors expressed in original-source coordinates.
    pub diagnostics: Vec<Diagnostic>,
    // Supplemental contains additional unnamed compiler inputs associated with this source file.
    pub supplemental: Vec<SupplementalOutput>,
}

impl ProtocolJson for TransformResult {
    fn marshal_json(&self) -> Value {
        let mut o = JsonMap::new();
        self.mapped_output.marshal_members(&mut o);
        if !self.diagnostics.is_empty() {
            o.insert(
                "diagnostics".to_string(),
                Value::Array(self.diagnostics.iter().map(ProtocolJson::marshal_json).collect()),
            );
        }
        if !self.supplemental.is_empty() {
            o.insert(
                "supplemental".to_string(),
                Value::Array(self.supplemental.iter().map(ProtocolJson::marshal_json).collect()),
            );
        }
        Value::Object(o)
    }

    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let mut v = TransformResult::default();
        for (name, member) in
            unmarshal_struct(value, "contentmapper.TransformResult", pointer)?.into_iter().flatten()
        {
            let pointer = &member_pointer(pointer, &name);
            let Some(member) = v.mapped_output.unmarshal_member(&name, member, pointer)? else {
                continue;
            };
            match name.as_str() {
                "diagnostics" => {
                    v.diagnostics = unmarshal_slice(
                        member,
                        "[]contentmapper.Diagnostic",
                        pointer,
                        Diagnostic::unmarshal_json,
                    )?;
                }
                "supplemental" => {
                    v.supplemental = unmarshal_slice(
                        member,
                        "[]contentmapper.SupplementalOutput",
                        pointer,
                        SupplementalOutput::unmarshal_json,
                    )?;
                }
                _ => {}
            }
        }
        Ok(v)
    }
}

// Diagnostic is an error reported by a mapper in original-source coordinates.
// hostimpl.go:207
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct Diagnostic {
    // MessageText is the diagnostic message.
    pub message_text: String,
    // Start and Length locate the diagnostic in the original content using the selected position encoding.
    pub start: i64,
    pub length: i64,
    pub code: i32,
}

impl ProtocolJson for Diagnostic {
    fn marshal_json(&self) -> Value {
        let mut o = JsonMap::new();
        o.insert("messageText".to_string(), Value::String(self.message_text.clone()));
        o.insert("start".to_string(), int_json(self.start));
        o.insert("length".to_string(), int_json(self.length));
        o.insert("code".to_string(), Value::Number(f64::from(self.code)));
        Value::Object(o)
    }

    fn unmarshal_json(value: Value, pointer: &str) -> Result<Self, String> {
        let mut v = Diagnostic::default();
        for (name, member) in
            unmarshal_struct(value, "contentmapper.Diagnostic", pointer)?.into_iter().flatten()
        {
            let pointer = &member_pointer(pointer, &name);
            match name.as_str() {
                "messageText" => v.message_text = unmarshal_string(member, pointer)?,
                "start" => v.start = unmarshal_int(&member, "int", i64::MIN, i64::MAX, pointer)?,
                "length" => v.length = unmarshal_int(&member, "int", i64::MIN, i64::MAX, pointer)?,
                "code" => v.code = unmarshal_int32(&member, "int32", pointer)?,
                _ => {}
            }
        }
        Ok(v)
    }
}

// dialFunc establishes a running connection to a mapper. In production it spawns the mapper's process;
// tests substitute an in-memory connection. It returns the connection and a closer that tears it down.
// hostimpl.go:218 (Go also passes the host's context, which the port does not have.)
type DialFunc = dyn Fn(
        &Mapper,
        &Locale,
    ) -> Result<(Arc<dyn Conn>, Arc<dyn Closer>, PositionEncoding, String), Error>
    + Send
    + Sync;

// host manages one child process per mapper identity. It is the production implementation of Host.
// hostimpl.go:221 (Go's ctx, cancel and stop are not ported: the port has no context. The owner closes the host, and
// dropping the last reference closes it too, where Go closes it when its context ends.)
pub struct HostImpl {
    // The host's own Arc, which project leases and Acquire's release function hold, as Go's do a *host.
    this: Weak<HostImpl>,
    dial: Box<DialFunc>,
    timing: Arc<TimingCollector>,

    // Go's lifecycleMu and the diagnosticLocale it guards. SetLocale and Close hold it for writing; every method
    // that may talk to a mapper holds it for reading, so processes are never replaced or torn down under one.
    lifecycle_mu: RwLock<Locale>,

    // Go's mu and the state it guards. Lock order: lifecycle_mu, then mu.
    mu: Mutex<HostLocked>,
}

// The fields of Go's host that mu guards. Close sets the maps to Go's nil (None). Go's maps are iterated in random
// order; these keep insertion order.
struct HostLocked {
    conns: Option<OrderedMap<String, MapperConn>>,
    projects: Option<OrderedMap<String, ProjectEntry>>,
    project_leases: Option<FxHashMap<String, Arc<ProjectLease>>>,
    next_project_id: u64,
}

// hostimpl.go:238
struct ProjectEntry {
    mapper: &'static Mapper,
    spec: ProjectSpec,
    project_handle: String,
    opened: bool,
    config_identity: String,
    watched_files: Vec<String>,
    option_diagnostics: Vec<OptionDiagnostic>,
}

// hostimpl.go:248
#[derive(Default)]
struct MapperConn {
    conn: Option<Arc<dyn Conn>>,
    closer: Option<Arc<dyn Closer>>,
    // err, when non-nil, records that this mapper failed to start; it is cached so we do not repeatedly
    // try (and fail) to spawn a broken mapper.
    err: Option<Error>,
    position_encoding: PositionEncoding,
    diagnostic_source: String,
    // refs is the number of active Acquire calls retaining this identity.
    refs: usize,
}

// hostimpl.go:260
#[derive(Default)]
struct OperationTimingState {
    count: AtomicU64,
    // Nanoseconds (Go's time.Duration).
    duration: AtomicU64,
}

impl OperationTimingState {
    // hostimpl.go:265
    fn record(&self, start: Instant) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.duration.fetch_add(
            u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::SeqCst,
        );
    }

    // hostimpl.go:270
    fn snapshot(&self) -> OperationTiming {
        OperationTiming {
            count: self.count.load(Ordering::SeqCst),
            duration: Duration::from_nanos(self.duration.load(Ordering::SeqCst)),
        }
    }
}

// hostimpl.go:274
struct TimingCollector {
    mu: Mutex<TimingCollectorLocked>,
}

// The fields of Go's timingCollector that its mu guards.
struct TimingCollectorLocked {
    mappers: OrderedMap<String, Arc<MapperTimingCollector>>,
    active_requests: u64,
    request_wait_start: Instant,
    request_wait_elapsed: Duration,
}

// hostimpl.go:282 (The owner is a weak reference: the owner's map holds the mapper collectors.)
#[derive(Default)]
struct MapperTimingCollector {
    spawn: OperationTimingState,
    initialize: OperationTimingState,
    open_project: OperationTimingState,
    close_project: OperationTimingState,
    transform: OperationTimingState,
    owner: Weak<TimingCollector>,
}

impl TimingCollector {
    fn new() -> Arc<TimingCollector> {
        Arc::new(TimingCollector {
            mu: Mutex::new(TimingCollectorLocked {
                mappers: OrderedMap::default(),
                active_requests: 0,
                request_wait_start: Instant::now(),
                request_wait_elapsed: Duration::ZERO,
            }),
        })
    }

    // hostimpl.go:291
    fn mapper(self: &Arc<Self>, identity: &str) -> Arc<MapperTimingCollector> {
        let mut t = lock(&self.mu);
        if let Some(timing) = t.mappers.get(identity) {
            return Arc::clone(timing);
        }
        let timing =
            Arc::new(MapperTimingCollector { owner: Arc::downgrade(self), ..Default::default() });
        t.mappers.insert(identity.to_string(), Arc::clone(&timing));
        timing
    }

    // hostimpl.go:302
    fn snapshot(&self) -> Timings {
        let (request_wait, mappers) = {
            let t = lock(&self.mu);
            let mut request_wait = t.request_wait_elapsed;
            if t.active_requests != 0 {
                request_wait += t.request_wait_start.elapsed();
            }
            (request_wait, t.mappers.clone())
        };
        let mut result = Timings {
            mappers: OrderedMap::with_capacity_and_hasher(mappers.len(), rustc_hash::FxBuildHasher),
            request_wait,
        };
        result.mappers.extend(mappers.into_iter().map(|(identity, timing)| {
            (
                identity,
                MapperTimings {
                    spawn: timing.spawn.snapshot(),
                    initialize: timing.initialize.snapshot(),
                    open_project: timing.open_project.snapshot(),
                    close_project: timing.close_project.snapshot(),
                    transform: timing.transform.snapshot(),
                },
            )
        }));
        result
    }
}

impl MapperTimingCollector {
    // hostimpl.go:324
    fn start_request(&self) -> Instant {
        if let Some(owner) = self.owner.upgrade() {
            let mut t = lock(&owner.mu);
            if t.active_requests == 0 {
                t.request_wait_start = Instant::now();
            }
            t.active_requests += 1;
        }
        Instant::now()
    }

    // hostimpl.go:334
    fn finish_request(&self, operation: &OperationTimingState, start: Instant) {
        operation.record(start);
        if let Some(owner) = self.owner.upgrade() {
            let mut t = lock(&owner.mu);
            t.active_requests -= 1;
            if t.active_requests == 0 {
                let elapsed = t.request_wait_start.elapsed();
                t.request_wait_elapsed += elapsed;
            }
        }
    }
}

// Spawner starts a child process, returning its stdio as an io.ReadWriteCloser (Read is the
// process's stdout, Write is its stdin) whose Close tears the process down. This seam keeps os/exec out
// of this package: production hosts spawn a real process, tests supply an in-process pipe.
// hostimpl.go:349 (The stream is tsrs_ipc's ReadWriteCloser. Go passes io.Discard as stderr when the host does not
// log; that is None here, so that a spawner can connect the child's stderr to the null device instead of copying it.
// The error is the text Go's err.Error() gives.)
pub trait Spawner: Send + Sync {
    fn spawn(
        &self,
        command: &[String],
        dir: &str,
        stderr: Option<Box<dyn Write + Send>>,
    ) -> Result<ReadWriteCloser, String>;
}

// hostimpl.go:353 processExitState is tsrs_ipc's `Closer::exit_code`, which every stream's closer answers (None for
// a stream that is not a process).

// SpawnerFunc adapts a spawn function to the Spawner interface.
// hostimpl.go:358
pub struct SpawnerFunc<F>(pub F);

impl<F> Spawner for SpawnerFunc<F>
where
    F: Fn(&[String], &str, Option<Box<dyn Write + Send>>) -> Result<ReadWriteCloser, String>
        + Send
        + Sync,
{
    // hostimpl.go:360
    fn spawn(
        &self,
        command: &[String],
        dir: &str,
        stderr: Option<Box<dyn Write + Send>>,
    ) -> Result<ReadWriteCloser, String> {
        (self.0)(command, dir, stderr)
    }
}

// Logger receives content mapper protocol and process output as complete log lines.
// hostimpl.go:365
pub type Logger = Arc<dyn Fn(&str) + Send + Sync>;

// HostOptions configures optional content mapper process logging.
// hostimpl.go:368
#[derive(Clone, Default)]
pub struct HostOptions {
    pub logger: Option<Logger>,
}

// hostimpl.go:372
struct LoggingProtocol {
    protocol: Box<dyn Protocol>,
    mapper_name: String,
    logger: Logger,
}

impl LoggingProtocol {
    // hostimpl.go:378 (Go marshals the message here; the callers pass the result of marshaling it.)
    fn log(&self, direction: &str, message: Result<String, ipc::Error>) {
        match message {
            Err(err) => (self.logger)(&format!(
                "[content mapper: {}] {direction}: <failed to serialize: {err}>",
                self.mapper_name
            )),
            Ok(data) => (self.logger)(&format!(
                "[content mapper: {}] {direction}: {data}",
                self.mapper_name
            )),
        }
    }
}

impl Protocol for LoggingProtocol {
    // hostimpl.go:387
    fn read_message(&self) -> Result<Message, ipc::Error> {
        let message = self.protocol.read_message();
        if let Ok(message) = &message {
            self.log("receive", message.marshal());
        }
        message
    }

    // hostimpl.go:395
    fn write_request(
        &self,
        id: &ID,
        method: &str,
        params: Option<&Value>,
    ) -> Result<(), ipc::Error> {
        self.log("send", jsonrpc::marshal_request_message(Some(id), method, params));
        self.protocol.write_request(id, method, params)
    }

    // hostimpl.go:401
    fn write_notification(&self, method: &str, params: Option<&Value>) -> Result<(), ipc::Error> {
        self.log("send", jsonrpc::marshal_request_message(None, method, params));
        self.protocol.write_notification(method, params)
    }

    // hostimpl.go:407 (Go logs a nil result as an absent member, which the port's Value::Null cannot tell apart
    // from an explicit null; the host only writes responses to the connection's own timing requests.)
    fn write_response(&self, id: &ID, result: &Value) -> Result<(), ipc::Error> {
        self.log("send", jsonrpc::marshal_response_message(Some(id), Some(result), None));
        self.protocol.write_response(id, result)
    }

    // hostimpl.go:413
    fn write_error(&self, id: &ID, response_error: &ResponseError) -> Result<(), ipc::Error> {
        self.log("send", jsonrpc::marshal_response_message(Some(id), None, Some(response_error)));
        self.protocol.write_error(id, response_error)
    }
}

// hostimpl.go:419 (Go's string of pending bytes is bytes here; a line is logged lossily if it is not UTF-8.)
struct StderrLogger {
    mapper_name: String,
    logger: Logger,
    mu: Mutex<Vec<u8>>,
}

impl StderrLogger {
    // hostimpl.go:426
    #[expect(
        clippy::unnecessary_wraps,
        reason = "The logger adapter shares the fallible Write interface used by the content mapper host."
    )]
    fn write(&self, data: &[u8]) -> io::Result<usize> {
        let mut pending = lock(&self.mu);
        pending.extend_from_slice(data);
        while let Some(index) = pending.iter().position(|&b| b == b'\n') {
            let line = pending[..index].strip_suffix(b"\r").unwrap_or(&pending[..index]);
            self.log(&tsrs_core::utf8::from_utf8_lossy(line));
            pending.drain(..=index);
        }
        Ok(data.len())
    }

    // hostimpl.go:441
    fn flush(&self) {
        let mut pending = lock(&self.mu);
        if !pending.is_empty() {
            let line = pending.strip_suffix(b"\r").unwrap_or(&pending);
            self.log(&tsrs_core::utf8::from_utf8_lossy(line));
            pending.clear();
        }
    }

    // hostimpl.go:450
    fn log(&self, message: &str) {
        (self.logger)(&format!("[content mapper: {}] stderr: {message}", self.mapper_name));
    }
}

// The io.Writer Go hands the spawner, which is the *stderrLogger itself.
struct StderrLoggerWriter(Arc<StderrLogger>);

impl Write for StderrLoggerWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.write(data)
    }

    // Go's io.Writer has no Flush; the logger writes complete lines as they arrive.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// hostimpl.go:454 (Go embeds the io.ReadWriteCloser; only Close is wrapped, and the reader and writer pass through.
// No exit_code: the embedded io.ReadWriteCloser's method set has no ExitCode, so Go's process-exit probe sees none
// through a logged process.)
struct LoggedProcess {
    closer: Arc<dyn Closer>,
    stderr: Arc<StderrLogger>,
}

impl Closer for LoggedProcess {
    // hostimpl.go:459
    fn close(&self) -> io::Result<()> {
        let err = self.closer.close();
        self.stderr.flush();
        err
    }
}

// hostimpl.go:465 (Go's sync.Once and the error it records.)
struct CloseOnceReadWriteCloser {
    closer: Arc<dyn Closer>,
    once: OnceLock<Option<(io::ErrorKind, String)>>,
}

impl Closer for CloseOnceReadWriteCloser {
    // hostimpl.go:471
    fn close(&self) -> io::Result<()> {
        let err = self
            .once
            .get_or_init(|| self.closer.close().err().map(|err| (err.kind(), err.to_string())));
        match err {
            None => Ok(()),
            Some((kind, text)) => Err(io::Error::new(*kind, text.clone())),
        }
    }

    // hostimpl.go:476
    fn exit_code(&self) -> Option<i32> {
        self.closer.exit_code()
    }
}

// NewHost creates a Host that spawns each mapper's process via the given spawner and drives it over a
// JSON-RPC connection. The host's lifetime is bound to ctx: cancelling it (e.g. the CLI's signal context
// on SIGINT, or a build/watch session ending) tears every mapper process down, so owners of a session
// context need not close the host explicitly. Close does the same synchronously.
// hostimpl.go:487 (No context: the host is closed by Close, or when it is dropped.)
pub fn new_host(spawner: Arc<dyn Spawner>, diagnostic_locale: Locale) -> Arc<dyn Host> {
    new_host_with_options(spawner, diagnostic_locale, HostOptions::default())
}

// NewHostWithOptions creates a Host with optional protocol and process logging.
// hostimpl.go:492
pub fn new_host_with_options(
    spawner: Arc<dyn Spawner>,
    diagnostic_locale: Locale,
    options: HostOptions,
) -> Arc<dyn Host> {
    let logger = options.logger;
    let timing = TimingCollector::new();
    let dial_timing = Arc::clone(&timing);
    let dial = move |mapper: &Mapper, diagnostic_locale: &Locale| {
        if mapper.manifest.exec.is_empty() {
            return Err(Error::Other(format!(
                "content mapper {} declares no command to run",
                quote(&mapper.definition.package)
            )));
        }
        let mapper_timing = dial_timing.mapper(&mapper.identity());
        let diagnostic_name = mapper.diagnostic_name().to_string();
        let spawn_start = Instant::now();
        let mut stderr: Option<Box<dyn Write + Send>> = None;
        let mut stderr_log: Option<Arc<StderrLogger>> = None;
        if let Some(logger) = &logger {
            let log = Arc::new(StderrLogger {
                mapper_name: diagnostic_name.clone(),
                logger: Arc::clone(logger),
                mu: Mutex::new(Vec::new()),
            });
            stderr = Some(Box::new(StderrLoggerWriter(Arc::clone(&log))));
            stderr_log = Some(log);
        }
        let spawned = spawner.spawn(&mapper.manifest.exec, &mapper.package_directory, stderr);
        mapper_timing.spawn.record(spawn_start);
        let ReadWriteCloser { reader, writer, closer } = match spawned {
            Ok(rwc) => rwc,
            Err(err) => {
                return Err(Error::from(InitializeError {
                    kind: InitializeErrorKind::ProcessStart,
                    mapper_name: diagnostic_name,
                    command: mapper.manifest.exec[0].clone(),
                    detail: err,
                    ..Default::default()
                }));
            }
        };
        let mut closer = closer;
        if let Some(stderr_log) = stderr_log {
            closer = Arc::new(LoggedProcess { closer, stderr: stderr_log });
        }
        let rwc: Arc<dyn Closer> =
            Arc::new(CloseOnceReadWriteCloser { closer, once: OnceLock::new() });
        let mut protocol: Box<dyn Protocol> = Box::new(ipc::new_jsonrpc_protocol(reader, writer));
        if let Some(logger) = &logger {
            protocol = Box::new(LoggingProtocol {
                protocol,
                mapper_name: diagnostic_name.clone(),
                logger: Arc::clone(logger),
            });
        }
        let conn = ipc::new_async_conn_with_protocol(
            Some(Arc::clone(&rwc)),
            protocol,
            Arc::new(RejectHandler),
        );
        // Go's `go func() { _ = conn.Run(ctx); _ = rwc.Close() }()`. The thread is never joined: closing the stream
        // ends the read loop, except while a grandchild of the mapper keeps its stdout open (process.rs), and then
        // the thread waits for that grandchild instead of the host.
        let started = {
            let conn = Arc::clone(&conn);
            let rwc = Arc::clone(&rwc);
            thread::Builder::new().name(CONN_THREAD.to_string()).spawn(move || {
                let _ = conn.run();
                let _ = rwc.close();
            })
        };
        if let Err(err) = started {
            // Go starts goroutines unconditionally; a thread can fail to start, which leaves the request unsent.
            let _ = rwc.close();
            return Err(Error::from(InitializeError {
                kind: InitializeErrorKind::Request,
                mapper_name: diagnostic_name,
                detail: format!("could not start the connection thread: {err}"),
                ..Default::default()
            }));
        }
        // Go's `context.WithTimeout(ctx, initializeTimeout)`.
        let initialize_deadline = Instant::now() + INITIALIZE_TIMEOUT;
        let initialize_start = mapper_timing.start_request();
        let handshake_result = handshake(&*conn, initialize_deadline, diagnostic_locale);
        mapper_timing.finish_request(&mapper_timing.initialize, initialize_start);
        let initialize_ctx_err = Instant::now() >= initialize_deadline;
        let err = match handshake_result {
            Ok((position_encoding, diagnostic_source)) => {
                let conn: Arc<dyn Conn> = conn;
                return Ok((conn, rwc, position_encoding, diagnostic_source));
            }
            Err(err) => err,
        };
        let exit_code = rwc.exit_code();
        let _ = rwc.close();
        if let Error::Initialize(mut initialize_error) = err {
            initialize_error.mapper_name = diagnostic_name;
            return Err(Error::Initialize(initialize_error));
        }
        if let Some(exit_code) = exit_code {
            return Err(Error::from(InitializeError {
                kind: InitializeErrorKind::ProcessExit,
                mapper_name: diagnostic_name,
                exit_code,
                ..Default::default()
            }));
        }
        // Go also tests for context.Canceled, which only the host context's cancellation produces.
        if initialize_ctx_err || err.is(ipc::ErrorTag::DeadlineExceeded) {
            return Err(Error::from(InitializeError {
                kind: InitializeErrorKind::NoResponse,
                mapper_name: diagnostic_name,
                timeout_seconds: INITIALIZE_TIMEOUT_SECONDS,
                ..Default::default()
            }));
        }
        Err(Error::from(InitializeError {
            kind: InitializeErrorKind::Request,
            mapper_name: diagnostic_name,
            detail: err.error(),
            ..Default::default()
        }))
    };
    new_with_dial(diagnostic_locale, timing, Box::new(dial))
}

// The name of the thread that runs a mapper connection's read loop (Go's goroutine).
const CONN_THREAD: &str = "contentmapper-conn";

// hostimpl.go:555
fn new_with_dial(
    diagnostic_locale: Locale,
    timing: Arc<TimingCollector>,
    dial: Box<DialFunc>,
) -> Arc<HostImpl> {
    Arc::new_cyclic(|this| HostImpl {
        this: Weak::clone(this),
        dial,
        timing,
        lifecycle_mu: RwLock::new(diagnostic_locale),
        mu: Mutex::new(HostLocked {
            conns: Some(OrderedMap::default()),
            projects: Some(OrderedMap::default()),
            project_leases: Some(FxHashMap::default()),
            next_project_id: 0,
        }),
    })
}

impl Host for HostImpl {
    // hostimpl.go:562
    fn timings(&self) -> Timings {
        self.timing.snapshot()
    }

    // hostimpl.go:566
    fn set_locale(&self, diagnostic_locale: Locale) {
        let mut current = write(&self.lifecycle_mu);
        if current.string() == diagnostic_locale.string() {
            return;
        }
        *current = diagnostic_locale;

        let mut closers: Vec<Arc<dyn Closer>> = Vec::new();
        {
            let mut state = lock(&self.mu);
            let HostLocked { conns, projects, .. } = &mut *state;
            for entry in conns.iter_mut().flat_map(|conns| conns.values_mut()) {
                if let Some(closer) = entry.closer.take() {
                    closers.push(closer);
                }
                entry.conn = None;
                entry.err = None;
                entry.position_encoding = PositionEncoding::default();
                entry.diagnostic_source = String::new();
            }
            for project in projects.iter_mut().flat_map(|projects| projects.values_mut()) {
                project.opened = false;
            }
        }
        for closer in closers {
            let _ = closer.close();
        }
    }

    // hostimpl.go:595
    fn project(&self, spec: ProjectSpec) -> Option<Arc<dyn Project>> {
        let _lifecycle = read(&self.lifecycle_mu);

        let key = project_spec_key(&spec);
        let mut state = lock(&self.mu);
        let HostLocked { conns, projects, project_leases, next_project_id } = &mut *state;
        let (Some(conns), Some(projects), Some(project_leases)) = (conns, projects, project_leases)
        else {
            return None;
        };
        let lease_entry = match project_leases.entry(key) {
            Entry::Occupied(entry) => return Some(entry.get().retain_locked()),
            Entry::Vacant(entry) => entry,
        };
        let mut entries =
            OrderedMap::with_capacity_and_hasher(spec.mappers.len(), rustc_hash::FxBuildHasher);
        for &mapper in &spec.mappers {
            let entry_key = format!("{}:{}", mapper.identity(), next_project_id);
            *next_project_id += 1;
            let entry = ProjectEntry {
                mapper,
                spec: spec.clone(),
                project_handle: entry_key.clone(),
                opened: false,
                config_identity: String::new(),
                watched_files: Vec::new(),
                option_diagnostics: Vec::new(),
            };
            projects.insert(entry_key.clone(), entry);
            conns.entry(mapper.identity()).or_default().refs += 1;
            entries.insert(mapper_key(mapper), entry_key);
        }
        let lease = Arc::new(ProjectLease {
            host: self.this.upgrade().expect("a content mapper host is alive while it is used"),
            key: lease_entry.key().clone(),
            mappers: spec.mappers.clone(),
            entries,
            refs: AtomicUsize::new(1),
            once: Once::new(),
        });
        lease_entry.insert(Arc::clone(&lease));
        Some(lease)
    }

    // hostimpl.go:736
    fn acquire(&self, mappers: &[&Mapper]) -> Box<dyn Fn() + Send + Sync> {
        let mut seen = FxHashSet::default();
        let mut identities = Vec::with_capacity(mappers.len());
        {
            let mut state = lock(&self.mu);
            if let Some(conns) = &mut state.conns {
                for mapper in mappers {
                    let identity = mapper.identity();
                    if !seen.insert(identity.clone()) {
                        continue;
                    }
                    conns.entry(identity.clone()).or_default().refs += 1;
                    identities.push(identity);
                }
            }
        }
        // Go's sync.OnceFunc.
        let host = self.this.upgrade().expect("a content mapper host is alive while it is used");
        let once = Once::new();
        Box::new(move || once.call_once(|| host.release(&identities)))
    }

    // Transform sends the file's content to the mapper's process and decodes the transformed result.
    // hostimpl.go:761 (Go allocates a fresh CompilerOptions, whose address keys the short-lived project; this one is
    // an arena object, so it stays allocated.)
    fn transform(
        &self,
        mapper: &'static Mapper,
        request: Request<'_>,
    ) -> Result<TransformResultFiles, Error> {
        let project = self
            .project(ProjectSpec {
                mappers: vec![mapper],
                compiler_options: Some(P::new(tsrs_core::CompilerOptions::default())),
                ..Default::default()
            })
            // Go calls Transform and Close on the nil Project, which panics, once the host is closed.
            .expect("content mapper host is closed");
        let result = project.transform(mapper, request);
        let _ = project.close();
        result
    }

    // Close shuts down every mapper process. It is safe to call more than once and is invoked automatically
    // when the context passed to New is cancelled.
    // hostimpl.go:798
    fn close(&self) -> Result<(), Error> {
        let _lifecycle = write(&self.lifecycle_mu);
        let closers: Vec<Arc<dyn Closer>> = {
            let mut state = lock(&self.mu);
            let closers = state
                .conns
                .iter()
                .flat_map(|conns| conns.values())
                .filter_map(|mc| mc.closer.clone())
                .collect();
            state.conns = None;
            state.projects = None;
            state.project_leases = None;
            closers
        };
        let errs: Vec<String> = closers
            .iter()
            .filter_map(|closer| closer.close().err())
            .map(|err| err.to_string())
            .collect();
        // Go's errors.Join.
        if errs.is_empty() {
            return Ok(());
        }
        Err(Error::Other(errs.join("\n")))
    }
}

// Go binds the host to its context (hostimpl.go:558 `h.stop = context.AfterFunc(ctx, func() { _ = h.Close() })`);
// the port closes it when the last reference to it goes away.
impl Drop for HostImpl {
    fn drop(&mut self) {
        let _ = Host::close(self);
    }
}

// hostimpl.go:627 (Go formats the pointers with %p.)
fn project_spec_key(spec: &ProjectSpec) -> String {
    use std::fmt::Write as _;
    let mut key = String::new();
    match spec.compiler_options {
        Some(options) => {
            let _ = write!(key, "{}\x00{:p}", spec.config_file_name, options.get());
        }
        None => {
            let _ = write!(key, "{}\x000x0", spec.config_file_name);
        }
    }
    for &mapper in &spec.mappers {
        let _ = write!(key, "\x00{mapper:p}");
    }
    key
}

// hostimpl.go:636 (`hex.EncodeToString(hash.Bytes())` of an xxh3.Uint128 is the u128's 32 hex digits.)
fn combined_identity(
    mapper: &Mapper,
    config_identity: &str,
    compiler_options: Option<P<CompilerOptions>>,
) -> String {
    let transform_identity = mapper.transform_identity(compiler_options.map(P::get)).to_be_bytes();
    let identity = mapper.identity();
    let options = mapper.definition.options.as_bytes();
    let mut buf = Vec::with_capacity(
        identity.len() + options.len() + config_identity.len() + transform_identity.len() + 3,
    );
    buf.extend_from_slice(identity.as_bytes());
    buf.push(0);
    buf.extend_from_slice(options);
    buf.push(0);
    buf.extend_from_slice(config_identity.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&transform_identity);
    let hash = xxhash_rust::xxh3::xxh3_128(&buf);
    format!("{identity}:{hash:032x}")
}

// The key of Go's map[*Mapper]string: the mapper's address.
fn mapper_key(mapper: &Mapper) -> usize {
    std::ptr::from_ref(mapper).addr()
}

impl HostImpl {
    // hostimpl.go:650 (Go reaches h.conns through the receiver; the caller passes it, as it also holds a mutable
    // reference into h.projects. The lifecycle lock's locale goes to the dial.)
    fn open_project_locked(
        &self,
        conns: &mut Option<OrderedMap<String, MapperConn>>,
        diagnostic_locale: &Locale,
        entry: &mut ProjectEntry,
    ) -> Result<(), Error> {
        if entry.opened {
            return Ok(());
        }
        let (conn, _, diagnostic_source) =
            self.conn_for_locked(conns, diagnostic_locale, entry.mapper)?;
        let compiler_options = match entry.spec.compiler_options {
            Some(options) => compiler_options_to_go_json(&options),
            None => Value::Null,
        };
        // Go passes the raw options through; the connection would fail to marshal text that is not JSON.
        let options = match entry.mapper.definition.options.as_str() {
            "" => None,
            options => Some(json::unmarshal(options).map_err(Error::Other)?),
        };
        let params = OpenProjectParams {
            config_file_name: entry.spec.config_file_name.clone(),
            project_handle: entry.project_handle.clone(),
            options,
            compiler_options: Some(compiler_options),
        };
        let mapper_timing = self.timing.mapper(&entry.mapper.identity());
        let start = mapper_timing.start_request();
        let raw = conn.call(METHOD_OPEN_PROJECT, Some(&params.marshal_json()));
        mapper_timing.finish_request(&mapper_timing.open_project, start);
        let raw = raw?;
        let Ok(result) = unmarshal::<OpenProjectResult>(raw) else {
            return Err(ProjectError { kind: ProjectErrorKind::MalformedResponse }.into());
        };
        let dynamic_config = entry.mapper.manifest.dynamic_config;
        if dynamic_config && result.config_identity.is_empty() {
            return Err(ProjectError { kind: ProjectErrorKind::MissingConfigIdentity }.into());
        }
        if !dynamic_config && !result.config_identity.is_empty() {
            return Err(ProjectError { kind: ProjectErrorKind::UnexpectedConfigIdentity }.into());
        }
        if !dynamic_config && !result.watched_files.is_empty() {
            return Err(ProjectError { kind: ProjectErrorKind::UnexpectedWatchedFiles }.into());
        }
        entry.config_identity = result.config_identity;
        if result.watched_files.iter().any(|file_name| !tspath::path_is_absolute(file_name)) {
            return Err(ProjectError { kind: ProjectErrorKind::NonAbsoluteWatchedFile }.into());
        }
        entry.watched_files = result.watched_files;
        entry.option_diagnostics = Vec::with_capacity(result.option_diagnostics.len());
        for diagnostic in result.option_diagnostics {
            let mut path = Vec::with_capacity(diagnostic.path.len());
            path = diagnostic.path.iter().try_fold(path, |mut path, raw_segment| {
                // Go switches on the raw segment's json.Kind: '"' a property, '0' an index, anything else malformed.
                match raw_segment {
                    Value::String(property) => path.push(OptionPathSegment {
                        property: property.clone(),
                        ..Default::default()
                    }),
                    Value::Number(_) | Value::Integer(_) => {
                        let index = match unmarshal_int(raw_segment, "int", i64::MIN, i64::MAX, "")
                        {
                            Ok(index) if index >= 0 => index,
                            _ => {
                                return Err(ProjectError {
                                    kind: ProjectErrorKind::MalformedResponse,
                                }
                                .into());
                            }
                        };
                        path.push(OptionPathSegment {
                            index: index as usize,
                            is_index: true,
                            ..Default::default()
                        });
                    }
                    _ => {
                        return Err(
                            ProjectError { kind: ProjectErrorKind::MalformedResponse }.into()
                        );
                    }
                }
                Ok::<_, Error>(path)
            })?;
            entry.option_diagnostics.push(OptionDiagnostic {
                mapper: entry.mapper,
                path,
                source: diagnostic_source.clone(),
                code: diagnostic.code,
                message_text: diagnostic.message_text,
            });
        }
        entry.opened = true;
        Ok(())
    }

    // hostimpl.go:728
    fn close_project(
        &self,
        mapper: &Mapper,
        conn: &dyn Conn,
        project_handle: &str,
    ) -> Result<(), ipc::Error> {
        let mapper_timing = self.timing.mapper(&mapper.identity());
        let start = mapper_timing.start_request();
        let params = CloseProjectParams { project_handle: project_handle.to_string() };
        let result = conn.call(METHOD_CLOSE_PROJECT, Some(&params.marshal_json()));
        mapper_timing.finish_request(&mapper_timing.close_project, start);
        result.map(|_| ())
    }

    // hostimpl.go:770 (Called with the lifecycle lock held for reading; its locale goes to connFor's dial.)
    fn transform_locked(
        &self,
        diagnostic_locale: &Locale,
        mapper: &Mapper,
        request: Request<'_>,
        project_handle: &str,
    ) -> Result<TransformResultFiles, Error> {
        if project_handle.is_empty() {
            return Err(Error::Other("content mapper project handle is required".to_string()));
        }
        let (conn, position_encoding, diagnostic_source) = match self
            .conn_for(diagnostic_locale, mapper)
        {
            Ok(conn) => conn,
            Err(err) => {
                return Err(new_transform_error(TransformErrorKind::Initialize, Some(err)).into());
            }
        };
        let mapper_timing = self.timing.mapper(&mapper.identity());
        let start = mapper_timing.start_request();
        let params = TransformParams {
            file_name: request.file_name.to_string(),
            content: request.content.to_string(),
            project_handle: project_handle.to_string(),
        };
        let raw = conn.call(METHOD_TRANSFORM, Some(&params.marshal_json()));
        mapper_timing.finish_request(&mapper_timing.transform, start);
        let raw = match raw {
            Ok(raw) => raw,
            Err(err) => {
                return Err(new_transform_error(
                    TransformErrorKind::Request,
                    Some(Error::Ipc(err)),
                )
                .into());
            }
        };
        match decode_transform_result(raw, request.content, &position_encoding, &diagnostic_source)
        {
            Ok(decoded) => Ok(decoded),
            Err(err) => Err(new_transform_error(TransformErrorKind::Response, Some(err)).into()),
        }
    }

    // connFor returns the connection for a mapper's identity, spawning its process on first use. Mappers
    // sharing an identity share a single process.
    // hostimpl.go:825
    fn conn_for(
        &self,
        diagnostic_locale: &Locale,
        mapper: &Mapper,
    ) -> Result<(Arc<dyn Conn>, PositionEncoding, String), Error> {
        let mut state = lock(&self.mu);
        self.conn_for_locked(&mut state.conns, diagnostic_locale, mapper)
    }

    // hostimpl.go:831
    fn conn_for_locked(
        &self,
        conns: &mut Option<OrderedMap<String, MapperConn>>,
        diagnostic_locale: &Locale,
        mapper: &Mapper,
    ) -> Result<(Arc<dyn Conn>, PositionEncoding, String), Error> {
        let Some(conns) = conns else {
            return Err(Error::Other("content mapper host is closed".to_string()));
        };
        let entry = conns.entry(mapper.identity()).or_default();
        if let Some(err) = &entry.err {
            return Err(err.clone());
        }
        if let Some(conn) = &entry.conn {
            return Ok((
                Arc::clone(conn),
                entry.position_encoding.clone(),
                entry.diagnostic_source.clone(),
            ));
        }
        match (self.dial)(mapper, diagnostic_locale) {
            Ok((conn, closer, position_encoding, diagnostic_source)) => {
                entry.conn = Some(Arc::clone(&conn));
                entry.closer = Some(closer);
                entry.position_encoding.clone_from(&position_encoding);
                entry.diagnostic_source.clone_from(&diagnostic_source);
                Ok((conn, position_encoding, diagnostic_source))
            }
            Err(err) => {
                entry.err = Some(err.clone());
                Err(err)
            }
        }
    }

    // hostimpl.go:1067
    fn release(&self, identities: &[String]) {
        let mut closers: Vec<Arc<dyn Closer>> = Vec::new();
        {
            let mut state = lock(&self.mu);
            if let Some(conns) = &mut state.conns {
                for identity in identities {
                    let Some(entry) = conns.get_mut(identity) else {
                        continue;
                    };
                    entry.refs = entry
                        .refs
                        .checked_sub(1)
                        .expect("content mapper connection reference count below zero");
                    if entry.refs == 0
                        && let Some(closer) =
                            conns.shift_remove(identity).and_then(|entry| entry.closer)
                    {
                        closers.push(closer);
                    }
                }
            }
        }
        for closer in closers {
            let _ = closer.close();
        }
    }
}

// hostimpl.go:853 (refs is guarded by host.mu, as in Go; it is an atomic only so that the lease can be shared.)
struct ProjectLease {
    host: Arc<HostImpl>,
    key: String,
    mappers: Vec<&'static Mapper>,
    // Go's map[*Mapper]string, keyed by the mapper's address.
    entries: OrderedMap<usize, String>,
    refs: AtomicUsize,
    once: Once,
}

// hostimpl.go:862 (Go embeds the *projectLease; every method but Close delegates to it.)
struct RetainedProject {
    lease: Arc<ProjectLease>,
    once: Once,
}

impl ProjectLease {
    // hostimpl.go:867
    fn retain_locked(self: &Arc<Self>) -> Arc<dyn Project> {
        self.refs.fetch_add(1, Ordering::SeqCst);
        Arc::new(RetainedProject { lease: Arc::clone(self), once: Once::new() })
    }

    // Go's `p.entries[mapper]`: the empty string for a mapper outside the project, which names no project entry.
    fn entry_key(&self, mapper: &Mapper) -> &str {
        self.entries.get(&mapper_key(mapper)).map_or("", String::as_str)
    }

    // hostimpl.go:1029
    fn release(&self) -> Result<(), Error> {
        let mut result: Option<ipc::Error> = None;
        {
            let _lifecycle = read(&self.host.lifecycle_mu);
            let mut released_identities = Vec::new();
            {
                let mut state = lock(&self.host.mu);
                let refs = self
                    .refs
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |refs| refs.checked_sub(1))
                    .unwrap_or_else(|_| {
                        panic!("content mapper project reference count below zero")
                    })
                    - 1;
                if refs != 0 {
                    return Ok(());
                }
                let HostLocked { conns, projects, project_leases, .. } = &mut *state;
                if let Some(project_leases) = project_leases
                    && project_leases
                        .get(&self.key)
                        .is_some_and(|lease| std::ptr::eq(Arc::as_ptr(lease), self))
                {
                    project_leases.remove(&self.key);
                }
                for key in self.entries.values() {
                    let Some(entry) =
                        projects.as_mut().and_then(|projects| projects.shift_remove(key))
                    else {
                        continue;
                    };
                    let identity = entry.mapper.identity();
                    if entry.opened
                        && let Some(conn) = conns
                            .as_ref()
                            .and_then(|conns| conns.get(&identity))
                            .and_then(|entry| entry.conn.as_ref())
                        && let Err(err) =
                            self.host.close_project(entry.mapper, &**conn, &entry.project_handle)
                    {
                        result = Some(join(result, err));
                    }
                    released_identities.push(identity);
                }
            }
            self.host.release(&released_identities);
        }
        result.map_or(Ok(()), |err| Err(Error::Ipc(err)))
    }
}

impl Project for ProjectLease {
    // hostimpl.go:877
    fn refresh(&self) -> Result<(), Error> {
        let _lifecycle = read(&self.host.lifecycle_mu);
        let mut state = lock(&self.host.mu);
        let HostLocked { conns, projects, .. } = &mut *state;
        let Some(projects) = projects else {
            return Ok(());
        };
        let mut result: Option<ipc::Error> = None;
        for key in self.entries.values() {
            let Some(entry) = projects.get_mut(key) else {
                continue;
            };
            if !entry.opened {
                continue;
            }
            if let Some(conn) = conns
                .as_ref()
                .and_then(|conns| conns.get(&entry.mapper.identity()))
                .and_then(|entry| entry.conn.as_ref())
                && let Err(err) =
                    self.host.close_project(entry.mapper, &**conn, &entry.project_handle)
            {
                result = Some(join(result, err));
            }
            entry.opened = false;
        }
        result.map_or(Ok(()), |err| Err(Error::Ipc(err)))
    }

    // hostimpl.go:899
    fn identities(&self) -> Result<Vec<String>, Error> {
        let diagnostic_locale = read(&self.host.lifecycle_mu);
        let mut state = lock(&self.host.mu);
        let HostLocked { conns, projects, .. } = &mut *state;
        let Some(projects) = projects else {
            return Ok(Vec::new());
        };
        let mut identities = Vec::with_capacity(self.entries.len());
        for &mapper in &self.mappers {
            let Some(entry) = projects.get_mut(self.entry_key(mapper)) else {
                continue;
            };
            if mapper.manifest.dynamic_config {
                self.host.open_project_locked(conns, &diagnostic_locale, entry)?;
                identities.push(combined_identity(
                    mapper,
                    &entry.config_identity,
                    entry.spec.compiler_options,
                ));
            } else {
                let hash = mapper.transform_identity(entry.spec.compiler_options.map(P::get));
                identities.push(format!("{}:{hash:032x}", mapper.identity()));
            }
        }
        Ok(identities)
    }

    // hostimpl.go:927
    fn identity(&self, mapper: &Mapper) -> Result<String, Error> {
        let diagnostic_locale = read(&self.host.lifecycle_mu);
        let mut state = lock(&self.host.mu);
        let HostLocked { conns, projects, .. } = &mut *state;
        let Some(projects) = projects else {
            return Ok(String::new());
        };
        let Some(key) = self.entries.get(&mapper_key(mapper)) else {
            return Ok(String::new());
        };
        let Some(entry) = projects.get_mut(key) else {
            return Ok(String::new());
        };
        if mapper.manifest.dynamic_config {
            self.host.open_project_locked(conns, &diagnostic_locale, entry)?;
            return Ok(combined_identity(
                mapper,
                &entry.config_identity,
                entry.spec.compiler_options,
            ));
        }
        let hash = mapper.transform_identity(entry.spec.compiler_options.map(P::get));
        Ok(format!("{}:{hash:032x}", mapper.identity()))
    }

    // hostimpl.go:953
    fn watched_files(&self) -> Result<Vec<String>, Error> {
        let diagnostic_locale = read(&self.host.lifecycle_mu);
        let mut state = lock(&self.host.mu);
        let HostLocked { conns, projects, .. } = &mut *state;
        let Some(projects) = projects else {
            return Ok(Vec::new());
        };
        let mut files = Vec::new();
        for key in self.entries.values() {
            let Some(entry) = projects.get_mut(key) else {
                continue;
            };
            if entry.mapper.manifest.dynamic_config {
                self.host.open_project_locked(conns, &diagnostic_locale, entry)?;
            }
            files.extend_from_slice(&entry.watched_files);
        }
        files.sort();
        files.dedup();
        Ok(files)
    }

    // hostimpl.go:978
    fn diagnostics(&self) -> Vec<OptionDiagnostic> {
        let _lifecycle = read(&self.host.lifecycle_mu);
        let state = lock(&self.host.mu);
        let Some(projects) = &state.projects else {
            return Vec::new();
        };
        let mut diagnostics = Vec::new();
        diagnostics.extend(
            self.mappers
                .iter()
                .filter_map(|&mapper| {
                    let entry = projects.get(self.entry_key(mapper))?;
                    if !entry.opened {
                        return None;
                    }
                    Some(entry.option_diagnostics.iter().cloned())
                })
                .flatten(),
        );
        diagnostics
    }

    // hostimpl.go:1000
    fn transform(
        &self,
        mapper: &Mapper,
        request: Request<'_>,
    ) -> Result<TransformResultFiles, Error> {
        let diagnostic_locale = read(&self.host.lifecycle_mu);
        let handle = {
            let mut state = lock(&self.host.mu);
            let HostLocked { conns, projects, .. } = &mut *state;
            let Some(entry) =
                projects.as_mut().and_then(|projects| projects.get_mut(self.entry_key(mapper)))
            else {
                return Err(Error::Other("content mapper project is closed".to_string()));
            };
            if let Err(err) = self.host.open_project_locked(conns, &diagnostic_locale, entry) {
                if err.as_initialize_error().is_some() {
                    return Err(
                        new_transform_error(TransformErrorKind::Initialize, Some(err)).into()
                    );
                }
                return Err(new_transform_error(TransformErrorKind::Project, Some(err)).into());
            }
            entry.project_handle.clone()
        };
        self.host.transform_locked(&diagnostic_locale, mapper, request, &handle)
    }

    // hostimpl.go:1021
    fn close(&self) -> Result<(), Error> {
        let mut result = Ok(());
        self.once.call_once(|| result = self.release());
        result
    }
}

impl Project for RetainedProject {
    fn refresh(&self) -> Result<(), Error> {
        self.lease.refresh()
    }

    fn identities(&self) -> Result<Vec<String>, Error> {
        self.lease.identities()
    }

    fn identity(&self, mapper: &Mapper) -> Result<String, Error> {
        self.lease.identity(mapper)
    }

    fn watched_files(&self) -> Result<Vec<String>, Error> {
        self.lease.watched_files()
    }

    fn diagnostics(&self) -> Vec<OptionDiagnostic> {
        self.lease.diagnostics()
    }

    fn transform(
        &self,
        mapper: &Mapper,
        request: Request<'_>,
    ) -> Result<TransformResultFiles, Error> {
        self.lease.transform(mapper, request)
    }

    // hostimpl.go:872
    fn close(&self) -> Result<(), Error> {
        let mut result = Ok(());
        self.once.call_once(|| result = self.lease.release());
        result
    }
}

// Go's errors.Join(result, err) of a nil or non-nil result and a non-nil err.
fn join(result: Option<ipc::Error>, err: ipc::Error) -> ipc::Error {
    match result {
        None => err,
        Some(result) => ipc::Error::join(&result, &err),
    }
}

// hostimpl.go:1091 (Go's ctx carries the initialize deadline; here the deadline is passed.)
fn handshake(
    conn: &dyn Conn,
    deadline: Instant,
    diagnostic_locale: &Locale,
) -> Result<(PositionEncoding, String), Error> {
    let params = InitializeParams {
        locale: diagnostic_locale.string(),
        position_encodings: vec![PositionEncoding::UTF8, PositionEncoding::UTF16],
    };
    let raw = conn.call_with_timeout(
        METHOD_INITIALIZE,
        Some(&params.marshal_json()),
        deadline.saturating_duration_since(Instant::now()),
    )?;
    let res = match unmarshal::<InitializeResult>(raw) {
        Ok(res) => res,
        Err(err) => {
            return Err(InitializeError {
                kind: InitializeErrorKind::InvalidResponse,
                detail: err,
                ..Default::default()
            }
            .into());
        }
    };
    if res.position_encoding != PositionEncoding::UTF8
        && res.position_encoding != PositionEncoding::UTF16
    {
        return Err(InitializeError {
            kind: InitializeErrorKind::PositionEncoding,
            position_encoding: res.position_encoding,
            ..Default::default()
        }
        .into());
    }
    if res.diagnostic_source.trim().is_empty() {
        return Err(InitializeError {
            kind: InitializeErrorKind::EmptyDiagnosticSource,
            ..Default::default()
        }
        .into());
    }
    let reserved = equal_fold(&res.diagnostic_source, "typescript")
        || equal_fold(&res.diagnostic_source, "tsc")
        || tspath::ALL_SUPPORTED_EXTENSIONS_WITH_JSON
            .iter()
            .flat_map(|extensions| extensions.iter())
            .any(|extension| {
                equal_fold(&res.diagnostic_source, extension.strip_prefix('.').unwrap_or(extension))
            });
    if reserved {
        return Err(InitializeError {
            kind: InitializeErrorKind::ReservedDiagnosticSource,
            diagnostic_source: res.diagnostic_source,
            ..Default::default()
        }
        .into());
    }
    Ok((res.position_encoding, res.diagnostic_source))
}

// hostimpl.go:1121
fn decode_transform_result(
    raw: Option<Value>,
    original_text: &str,
    position_encoding: &PositionEncoding,
    diagnostic_source: &str,
) -> Result<TransformResultFiles, Error> {
    let res: TransformResult = unmarshal(raw).map_err(Error::Other)?;
    let TransformResult { mapped_output, diagnostics, supplemental } = res;
    let (mapped, original_positions) =
        decode_mapped_output(mapped_output, original_text, position_encoding, diagnostic_source)?;
    let mut result = TransformResultFiles {
        text: mapped.text,
        virtual_extension: mapped.virtual_extension,
        mappings: mapped.mappings,
        diagnostic_directives: mapped.diagnostic_directives,
        ..Default::default()
    };
    result.supplemental = supplemental
        .into_iter()
        .enumerate()
        .map(|(supplemental_index, supplemental)| {
            match decode_mapped_output(
                supplemental.mapped_output,
                original_text,
                position_encoding,
                diagnostic_source,
            ) {
                Ok((mapped, _)) => Ok(mapped),
                Err(mut err) => {
                    if let Error::DiagnosticDirective(directive_error) = &mut err {
                        directive_error.supplemental_index = Some(supplemental_index);
                    }
                    Err(err)
                }
            }
        })
        .collect::<Result<_, _>>()?;
    for d in &diagnostics {
        if d.start < 0 || d.length < 0 || d.start > i64::MAX - d.length {
            return Err(Error::Other(format!(
                "invalid content mapper diagnostic range [{}, {})",
                d.start,
                d.start.wrapping_add(d.length)
            )));
        }
        let start = original_positions.normalize(d.start).map_err(|err| {
            Error::Other(format!("invalid content mapper diagnostic start: {err}"))
        })?;
        let end = original_positions
            .normalize(d.start + d.length)
            .map_err(|err| Error::Other(format!("invalid content mapper diagnostic end: {err}")))?;
        result.diagnostics.push(ast::new_external_diagnostic(
            None,
            TextRange::new(start, end),
            diagnostic_source,
            Category::Error,
            d.code,
            &d.message_text,
        ));
    }
    Ok(result)
}

// hostimpl.go:1170
fn decode_mapped_output<'a>(
    output: MappedOutput,
    original_text: &'a str,
    position_encoding: &PositionEncoding,
    diagnostic_source: &str,
) -> Result<(MappedResult, PositionNormalizer<'a>), Error> {
    if !is_supported_virtual_extension(&output.extension) {
        return Err(InvalidVirtualExtensionError { extension: output.extension }.into());
    }
    let virtual_positions =
        new_position_normalizer(&output.text, position_encoding).map_err(Error::Other)?;
    let original_positions =
        new_position_normalizer(original_text, position_encoding).map_err(Error::Other)?;
    // A successful transform always carries a span map. Absent or empty mappings describe fully
    // synthesized output (no segment corresponds to the original), so decode to an empty map rather than
    // nil, which would mean "not content-mapped".
    let mappings = match &output.mappings {
        Some(raw) => {
            // Go hands spanmap.Unmarshal the member's raw bytes.
            let raw = json::marshal(raw).map_err(Error::Other)?;
            let mappings = tsrs_spanmap::unmarshal(raw.as_bytes()).map_err(Error::Other)?;
            normalize_mappings(&mappings, &virtual_positions, &original_positions)?
        }
        None => tsrs_spanmap::new(&[]),
    };
    let diagnostic_directives = normalize_diagnostic_directives(
        output.diagnostic_directives.as_ref(),
        &virtual_positions,
        &original_positions,
        diagnostic_source,
    )?;
    let result = MappedResult {
        text: output.text,
        virtual_extension: output.extension,
        mappings: Some(mappings),
        diagnostic_directives,
    };
    Ok((result, original_positions))
}

// hostimpl.go:1208 (The directives' strings are arena text, like the rest of the source file that will hold them.)
fn normalize_diagnostic_directives(
    diagnostic_directives: Option<&DiagnosticDirectives>,
    virtual_positions: &PositionNormalizer<'_>,
    original_positions: &PositionNormalizer<'_>,
    diagnostic_source: &str,
) -> Result<Vec<ast::MappedDiagnosticDirective>, Error> {
    let Some(diagnostic_directives) = diagnostic_directives else {
        return Ok(Vec::new());
    };
    let directives = &diagnostic_directives.directives;
    let unused_diagnostics = &diagnostic_directives.unused_expect_directive_diagnostics;
    let source = if directives.is_empty() { "" } else { alloc_str(diagnostic_source) };
    let mut result = vec![ast::MappedDiagnosticDirective::default(); directives.len()];
    for (i, directive) in directives.iter().enumerate() {
        let directive_error = |kind: DiagnosticDirectiveErrorKind| -> Error {
            DiagnosticDirectiveError {
                kind,
                index: i,
                supplemental_index: None,
                ..Default::default()
            }
            .into()
        };
        let mut normalized = ast::MappedDiagnosticDirective { source, ..Default::default() };
        match directive.policy {
            DiagnosticDirectivePolicy::IGNORE => {
                normalized.policy = ast::MappedDiagnosticDirectivePolicy::Ignore;
            }
            DiagnosticDirectivePolicy::EXPECT => {
                let mut unused_diagnostic_index = 0;
                if let Some(index) = directive.unused_expect_directive_index {
                    unused_diagnostic_index = index;
                } else if unused_diagnostics.len() != 1 {
                    return Err(directive_error(
                        DiagnosticDirectiveErrorKind::ExpectMissingUnusedDiagnostic,
                    ));
                }
                if unused_diagnostic_index < 0
                    || unused_diagnostic_index >= unused_diagnostics.len() as i64
                {
                    return Err(directive_error(
                        DiagnosticDirectiveErrorKind::InvalidUnusedDiagnosticIndex,
                    ));
                }
                let unused_diagnostic = &unused_diagnostics[unused_diagnostic_index as usize];
                normalized.policy = ast::MappedDiagnosticDirectivePolicy::Expect;
                normalized.unused_code = unused_diagnostic.code;
                normalized.unused_message_text = alloc_str(&unused_diagnostic.message_text);
            }
            policy => {
                return Err(DiagnosticDirectiveError {
                    kind: DiagnosticDirectiveErrorKind::InvalidPolicy,
                    index: i,
                    supplemental_index: None,
                    policy,
                }
                .into());
            }
        }
        if directive.virtual_start < 0 || directive.virtual_end < directive.virtual_start {
            return Err(directive_error(DiagnosticDirectiveErrorKind::InvalidRange));
        }
        let Ok(virtual_start) = virtual_positions.normalize(directive.virtual_start) else {
            return Err(directive_error(DiagnosticDirectiveErrorKind::InvalidRange));
        };
        let Ok(virtual_end) = virtual_positions.normalize(directive.virtual_end) else {
            return Err(directive_error(DiagnosticDirectiveErrorKind::InvalidRange));
        };
        normalized.virtual_range = TextRange::new(virtual_start, virtual_end);
        let mut valid_original_range = directive.original_start >= 0
            && directive.original_length >= 0
            && directive.original_start <= i64::MAX - directive.original_length;
        if valid_original_range {
            let original_start = original_positions.normalize(directive.original_start);
            let original_end =
                original_positions.normalize(directive.original_start + directive.original_length);
            match (original_start, original_end) {
                (Ok(original_start), Ok(original_end)) => {
                    normalized.original_range = TextRange::new(original_start, original_end);
                }
                _ => valid_original_range = false,
            }
        }
        if normalized.policy == ast::MappedDiagnosticDirectivePolicy::Expect
            && !valid_original_range
        {
            return Err(directive_error(DiagnosticDirectiveErrorKind::InvalidRange));
        }
        result[i] = normalized;
    }
    // Go's indexedDirective, sorted with Go's (unstable) slices.SortFunc so that equal starts keep Go's order.
    let mut sorted: Vec<(ast::MappedDiagnosticDirective, usize)> =
        result.iter().copied().zip(0..).collect();
    goslices::sort_func(&mut sorted, |a, b| a.0.virtual_range.pos().cmp(&b.0.virtual_range.pos()));
    if let Some(pair) =
        sorted.windows(2).find(|pair| pair[1].0.virtual_range.pos() < pair[0].0.virtual_range.end())
    {
        return Err(DiagnosticDirectiveError {
            kind: DiagnosticDirectiveErrorKind::Overlap,
            index: pair[1].1,
            supplemental_index: None,
            ..Default::default()
        }
        .into());
    }
    Ok(result)
}

// hostimpl.go:1291
fn normalize_mappings(
    mappings: &SpanMap,
    virtual_positions: &PositionNormalizer<'_>,
    original_positions: &PositionNormalizer<'_>,
) -> Result<P<SpanMap>, Error> {
    let mut segments = mappings.segments();
    for (i, segment) in segments.iter_mut().enumerate() {
        segment.virtual_start =
            virtual_positions.normalize_text_pos(segment.virtual_start).map_err(|err| {
                Error::Other(format!("invalid content mapper mapping {i} virtual start: {err}"))
            })?;
        segment.virtual_end =
            virtual_positions.normalize_text_pos(segment.virtual_end).map_err(|err| {
                Error::Other(format!("invalid content mapper mapping {i} virtual end: {err}"))
            })?;
        segment.original_start =
            original_positions.normalize_text_pos(segment.original_start).map_err(|err| {
                Error::Other(format!("invalid content mapper mapping {i} original start: {err}"))
            })?;
        segment.original_end =
            original_positions.normalize_text_pos(segment.original_end).map_err(|err| {
                Error::Other(format!("invalid content mapper mapping {i} original end: {err}"))
            })?;
    }
    Ok(tsrs_spanmap::new(&segments))
}

// hostimpl.go:1316
struct PositionNormalizer<'a> {
    text: &'a str,
    encoding: PositionEncoding,
    position_map: Option<ast::PositionMap>,
    length: i64,
}

// hostimpl.go:1323
fn new_position_normalizer<'a>(
    text: &'a str,
    encoding: &PositionEncoding,
) -> Result<PositionNormalizer<'a>, String> {
    let mut normalizer =
        PositionNormalizer { text, encoding: encoding.clone(), position_map: None, length: 0 };
    if *encoding == PositionEncoding::UTF8 {
        normalizer.length = text.len() as i64;
    } else if *encoding == PositionEncoding::UTF16 {
        let position_map = ast::compute_position_map(text);
        normalizer.length = i64::from(
            position_map
                .utf8_to_utf16(u32::try_from(text.len()).expect("source length exceeds u32")),
        );
        normalizer.position_map = Some(position_map);
    } else {
        return Err(format!("unsupported position encoding {}", quote(&encoding.0)));
    }
    Ok(normalizer)
}

impl PositionNormalizer<'_> {
    // hostimpl.go:1337
    fn normalize_text_pos(&self, position: u32) -> Result<u32, String> {
        self.normalize(i64::from(position))
    }

    // hostimpl.go:1342 (Go returns an int; a position that passes the length check fits a TextPos.)
    fn normalize(&self, position: i64) -> Result<u32, String> {
        if position < 0 {
            return Err(format!("position {position} is negative"));
        }
        if position > self.length {
            return Err(format!(
                "position {position} exceeds {} length {}",
                self.encoding, self.length
            ));
        }
        let normalized_position =
            u32::try_from(position).expect("validated source position exceeds u32");
        let byte_position = match &self.position_map {
            // PositionEncodingUTF16
            Some(position_map) => position_map.utf16_to_utf8(normalized_position),
            // PositionEncodingUTF8
            None => normalized_position,
        };
        // utf8.RuneStart
        if (byte_position as usize) < self.text.len()
            && self.text.as_bytes()[byte_position as usize] & 0xC0 == 0x80
        {
            return Err(format!("position {position} splits a Unicode code point"));
        }
        Ok(byte_position)
    }
}

// rejectHandler rejects any request initiated by the mapper. The content mapper protocol is currently
// parent-driven only; a request from the child is a protocol violation.
// hostimpl.go:1364
struct RejectHandler;

impl Handler for RejectHandler {
    // hostimpl.go:1366
    fn handle_request(&self, method: &str, _params: Option<&Value>) -> Result<Value, ipc::Error> {
        Err(ipc::Error::new(format!("content mapper sent an unexpected request: {method}")))
    }

    // hostimpl.go:1370
    fn handle_notification(
        &self,
        _method: &str,
        _params: Option<&Value>,
    ) -> Result<(), ipc::Error> {
        Ok(())
    }
}

// Go's sync.Mutex and sync.RWMutex have no poisoning: a thread that panics while holding one (Go's recovered
// panics) leaves the data usable.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}

// --- json/v2 decoding and encoding helpers for ProtocolJson ---

// A Go json.Value member's value: JSON null for an empty (None) value, as jsontext.Value.MarshalJSON writes it.
fn raw_json(value: Option<&Value>) -> Value {
    value.cloned().unwrap_or(Value::Null)
}

// Whether json/v2's `omitempty` leaves out a member with this value: null, "", {} or [].
fn is_empty_json(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(members) => members.is_empty(),
        Value::Bool(_) | Value::Number(_) | Value::Integer(_) => false,
    }
}

// A Go int, exactly: f64 cannot hold every i64.
fn int_json(n: i64) -> Value {
    if n.unsigned_abs() <= 1 << 53 { Value::Number(n as f64) } else { Value::Integer(n) }
}

fn member_pointer(pointer: &str, name: &str) -> String {
    format!("{pointer}/{name}")
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::String(_) => "string",
        Value::Number(_) | Value::Integer(_) => "number",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// json/v2's SemanticError text: `json: cannot unmarshal JSON <kind> into Go <type>[ within "<pointer>"]`.
fn type_error(kind: &str, go_type: &str, pointer: &str) -> String {
    if pointer.is_empty() {
        return format!("json: cannot unmarshal JSON {kind} into Go {go_type}");
    }
    format!("json: cannot unmarshal JSON {kind} into Go {go_type} within {}", quote(pointer))
}

// The members of an object to decode into a Go struct; None for null (the zero value).
fn unmarshal_struct(value: Value, go_type: &str, pointer: &str) -> Result<Option<JsonMap>, String> {
    match value {
        Value::Object(members) => Ok(Some(members)),
        Value::Null => Ok(None),
        value => Err(type_error(json_kind(&value), go_type, pointer)),
    }
}

// A Go slice decoded element by element; empty for null (Go's nil slice).
fn unmarshal_slice<T>(
    value: Value,
    go_type: &str,
    pointer: &str,
    mut unmarshal_element: impl FnMut(Value, &str) -> Result<T, String>,
) -> Result<Vec<T>, String> {
    match value {
        Value::Array(elements) => elements
            .into_iter()
            .enumerate()
            .map(|(i, element)| unmarshal_element(element, &format!("{pointer}/{i}")))
            .collect(),
        Value::Null => Ok(Vec::new()),
        value => Err(type_error(json_kind(&value), go_type, pointer)),
    }
}

// A Go string; "" for null.
fn unmarshal_string(value: Value, pointer: &str) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s),
        Value::Null => Ok(String::new()),
        value => Err(type_error(json_kind(&value), "string", pointer)),
    }
}

// A Go integer of the given range; 0 for null. Go also rejects an integral number written with a fraction or an
// exponent (1.0, 1e3), which the parsed value cannot tell apart from an integer literal.
fn unmarshal_int(
    value: &Value,
    go_type: &str,
    min: i64,
    max: i64,
    pointer: &str,
) -> Result<i64, String> {
    let n = match *value {
        Value::Null => return Ok(0),
        Value::Integer(n) => Some(n),
        Value::Number(n) if n.fract() == 0.0 && n >= min as f64 && n <= max as f64 => {
            Some(n as i64)
        }
        Value::Number(_) => None,
        _ => return Err(type_error(json_kind(value), go_type, pointer)),
    };
    match n {
        Some(n) if n >= min && n <= max => Ok(n),
        _ => {
            let text = json::marshal(value).unwrap_or_default();
            let reason = if matches!(value, Value::Number(n) if n.fract() != 0.0) {
                "invalid syntax"
            } else {
                "value out of range"
            };
            Err(format!("{}: {reason}", type_error(&format!("number {text}"), go_type, pointer)))
        }
    }
}

fn unmarshal_int32(value: &Value, go_type: &str, pointer: &str) -> Result<i32, String> {
    unmarshal_int(value, go_type, i64::from(i32::MIN), i64::from(i32::MAX), pointer)
        .map(|n| n as i32)
}
