// Port of execute/incremental/buildInfo.go.
//
// Go marshals these types with encoding/json/v2 (`omitzero` fields, custom MarshalJSON for the tuple shapes).
// Rust builds the same `json::Value` tree by hand, field by field in Go's declaration order, so that
// `json::marshal` produces Go's exact bytes in Go-compatible history. Unmarshaling follows the Go
// `UnmarshalJSON` fallbacks; opt-in modes add a performance-policy marker and canonical-cache proof.

use rustc_hash::FxHashMap;
use tsrs_ast::RepopulateDiagnosticKind;
use tsrs_core::collections::OrderedMap;
use tsrs_core::json::{JsonMap, Value};
use tsrs_core::tspath::{self, ComparePathsOptions, Path};
use tsrs_core::{CompilerOptions, ModuleKind, ResolutionMode};
use tsrs_diagnostics::Category;
use tsrs_tsoptions::{self as tsoptions, CompilerOptionsValue, ParsedCommandLine};

use crate::snapshot::{EmitSignature, FileEmitKind, FileInfo, get_pending_emit_kind_with_options};

pub type BuildInfoFileId = u32;
pub type BuildInfoFileIdListId = u32;

// buildInfoRoot is
// - for incremental program buildinfo
//   - start and end of FileId for consecutive fileIds to be included as root
//   - start - single fileId that is root
//
// - for non incremental program buildinfo
//   - string that is the root file name
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildInfoRoot {
    pub start: BuildInfoFileId,
    pub end: BuildInfoFileId,
    pub non_incremental: String, // Root of a non incremental program
}

// buildInfo.go:36
impl BuildInfoRoot {
    pub fn marshal_json(&self) -> Value {
        if self.start != 0 {
            if self.end != 0 {
                Value::Array(vec![num(self.start), num(self.end)])
            } else {
                num(self.start)
            }
        } else {
            Value::String(self.non_incremental.clone())
        }
    }

    // buildInfo.go:48
    fn unmarshal_json(data: &Value) -> Result<BuildInfoRoot, String> {
        if let Some(start_and_end) = as_uint_tuple::<2>(data) {
            return Ok(BuildInfoRoot {
                start: start_and_end[0],
                end: start_and_end[1],
                ..Default::default()
            });
        }
        if let Some(start) = as_uint(data) {
            return Ok(BuildInfoRoot { start, ..Default::default() });
        }
        if let Value::String(name) = data {
            return Ok(BuildInfoRoot { non_incremental: name.clone(), ..Default::default() });
        }
        Err(format!("invalid BuildInfoRoot: {}", show(data)))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildInfoFileInfoNoSignature {
    version: String,
    no_signature: bool,
    affects_global_scope: bool,
    implied_node_format: ResolutionMode,
}

//	 Signature is
//		 - undefined if FileInfo.version === FileInfo.signature
//		 - string actual signature
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildInfoFileInfoWithSignature {
    version: String,
    signature: String,
    affects_global_scope: bool,
    implied_node_format: ResolutionMode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildInfoFileInfo {
    signature: String,
    no_signature: Option<BuildInfoFileInfoNoSignature>,
    file_info: Option<BuildInfoFileInfoWithSignature>,
}

// buildInfo.go:98
pub fn new_build_info_file_info(file_info: &FileInfo) -> BuildInfoFileInfo {
    if file_info.version == file_info.signature {
        if !file_info.affects_global_scope && file_info.implied_node_format == ModuleKind::COMMON_JS
        {
            return BuildInfoFileInfo {
                signature: file_info.signature.clone(),
                ..Default::default()
            };
        }
    } else if file_info.signature.is_empty() {
        return BuildInfoFileInfo {
            no_signature: Some(BuildInfoFileInfoNoSignature {
                version: file_info.version.clone(),
                no_signature: true,
                affects_global_scope: file_info.affects_global_scope,
                implied_node_format: file_info.implied_node_format,
            }),
            ..Default::default()
        };
    }
    BuildInfoFileInfo {
        file_info: Some(BuildInfoFileInfoWithSignature {
            version: file_info.version.clone(),
            signature: if file_info.signature == file_info.version {
                String::new()
            } else {
                file_info.signature.clone()
            },
            affects_global_scope: file_info.affects_global_scope,
            implied_node_format: file_info.implied_node_format,
        }),
        ..Default::default()
    }
}

impl BuildInfoFileInfo {
    // buildInfo.go:121
    pub fn get_file_info(&self) -> FileInfo {
        if !self.signature.is_empty() {
            return FileInfo {
                version: self.signature.clone(),
                signature: self.signature.clone(),
                affects_global_scope: false,
                implied_node_format: ModuleKind::COMMON_JS,
            };
        }
        if let Some(no_signature) = &self.no_signature {
            return FileInfo {
                version: no_signature.version.clone(),
                signature: String::new(),
                affects_global_scope: no_signature.affects_global_scope,
                implied_node_format: no_signature.implied_node_format,
            };
        }
        let file_info = self.file_info.as_ref().unwrap();
        FileInfo {
            version: file_info.version.clone(),
            signature: if file_info.signature.is_empty() {
                file_info.version.clone()
            } else {
                file_info.signature.clone()
            },
            affects_global_scope: file_info.affects_global_scope,
            implied_node_format: file_info.implied_node_format,
        }
    }

    // buildInfo.go:148
    pub fn has_signature(&self) -> bool {
        !self.signature.is_empty()
    }

    // buildInfo.go:152
    pub fn marshal_json(&self) -> Value {
        if !self.signature.is_empty() {
            return Value::String(self.signature.clone());
        }
        if let Some(no_signature) = &self.no_signature {
            let mut o = Obj::default();
            o.str("version", &no_signature.version);
            o.bool("noSignature", no_signature.no_signature);
            o.bool("affectsGlobalScope", no_signature.affects_global_scope);
            o.int("impliedNodeFormat", no_signature.implied_node_format.value());
            return o.done();
        }
        let file_info = self.file_info.as_ref().unwrap();
        let mut o = Obj::default();
        o.str("version", &file_info.version);
        o.str("signature", &file_info.signature);
        o.bool("affectsGlobalScope", file_info.affects_global_scope);
        o.int("impliedNodeFormat", file_info.implied_node_format.value());
        o.done()
    }

    // buildInfo.go:162
    fn unmarshal_json(data: &Value) -> Result<BuildInfoFileInfo, String> {
        if let Value::String(v_signature) = data {
            return Ok(BuildInfoFileInfo { signature: v_signature.clone(), ..Default::default() });
        }
        let no_signature = (|| -> Result<BuildInfoFileInfoNoSignature, String> {
            let o = as_object(data)?;
            Ok(BuildInfoFileInfoNoSignature {
                version: get_string(o, "version")?,
                no_signature: get_bool(o, "noSignature")?,
                affects_global_scope: get_bool(o, "affectsGlobalScope")?,
                implied_node_format: module_kind_from_i32(get_int(o, "impliedNodeFormat")?),
            })
        })();
        match no_signature {
            Ok(no_signature) if no_signature.no_signature => {
                Ok(BuildInfoFileInfo { no_signature: Some(no_signature), ..Default::default() })
            }
            _ => {
                let file_info = (|| -> Result<BuildInfoFileInfoWithSignature, String> {
                    let o = as_object(data)?;
                    Ok(BuildInfoFileInfoWithSignature {
                        version: get_string(o, "version")?,
                        signature: get_string(o, "signature")?,
                        affects_global_scope: get_bool(o, "affectsGlobalScope")?,
                        implied_node_format: module_kind_from_i32(get_int(o, "impliedNodeFormat")?),
                    })
                })()
                .map_err(|_| format!("invalid BuildInfoFileInfo: {}", show(data)))?;
                Ok(BuildInfoFileInfo { file_info: Some(file_info), ..Default::default() })
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildInfoReferenceMapEntry {
    pub file_id: BuildInfoFileId,
    pub file_id_list_id: BuildInfoFileIdListId,
}

impl BuildInfoReferenceMapEntry {
    // buildInfo.go:186
    fn marshal_json(&self) -> Value {
        Value::Array(vec![num(self.file_id), num(self.file_id_list_id)])
    }

    // buildInfo.go:190
    fn unmarshal_json(data: &Value) -> Result<BuildInfoReferenceMapEntry, String> {
        let v = as_uint_tuple::<2>(data)
            .ok_or_else(|| format!("invalid BuildInfoReferenceMapEntry: {}", show(data)))?;
        Ok(BuildInfoReferenceMapEntry { file_id: v[0], file_id_list_id: v[1] })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BuildInfoDiagnostic {
    // BuildInfoFileId if it is for a File thats other than its stored for
    pub file: BuildInfoFileId,
    pub no_file: bool,
    pub pos: u32,
    pub end: u32,
    pub code: i32,
    pub category: Category,
    pub source: String,
    pub message_text: String,
    pub message_key: String,
    pub message_args: Vec<String>,
    pub message_chain: Vec<BuildInfoDiagnostic>,
    pub related_information: Vec<BuildInfoDiagnostic>,
    pub reports_unnecessary: bool,
    pub reports_deprecated: bool,
    pub skipped_on_no_emit: bool,
    pub repopulate_info: Option<BuildInfoRepopulateInfo>,
}

impl Default for BuildInfoDiagnostic {
    fn default() -> Self {
        BuildInfoDiagnostic {
            file: 0,
            no_file: false,
            pos: 0,
            end: 0,
            code: 0,
            category: Category::Warning,
            source: String::new(),
            message_text: String::new(),
            message_key: String::new(),
            message_args: Vec::new(),
            message_chain: Vec::new(),
            related_information: Vec::new(),
            reports_unnecessary: false,
            reports_deprecated: false,
            skipped_on_no_emit: false,
            repopulate_info: None,
        }
    }
}

impl BuildInfoDiagnostic {
    fn marshal_json(&self) -> Value {
        let mut o = Obj::default();
        o.uint("file", self.file);
        o.bool("noFile", self.no_file);
        o.uint("pos", self.pos);
        o.uint("end", self.end);
        o.int("code", self.code);
        o.int("category", category_to_i32(self.category));
        o.str("source", &self.source);
        o.str("messageText", &self.message_text);
        o.str("messageKey", &self.message_key);
        if !self.message_args.is_empty() {
            o.set(
                "messageArgs",
                Value::Array(self.message_args.iter().map(|a| Value::String(a.clone())).collect()),
            );
        }
        if !self.message_chain.is_empty() {
            o.set(
                "messageChain",
                Value::Array(
                    self.message_chain.iter().map(BuildInfoDiagnostic::marshal_json).collect(),
                ),
            );
        }
        if !self.related_information.is_empty() {
            o.set(
                "relatedInformation",
                Value::Array(
                    self.related_information
                        .iter()
                        .map(BuildInfoDiagnostic::marshal_json)
                        .collect(),
                ),
            );
        }
        o.bool("reportsUnnecessary", self.reports_unnecessary);
        o.bool("reportsDeprecated", self.reports_deprecated);
        o.bool("skippedOnNoEmit", self.skipped_on_no_emit);
        if let Some(info) = &self.repopulate_info {
            o.set("repopulateInfo", info.marshal_json());
        }
        o.done()
    }

    fn unmarshal_json(data: &Value) -> Result<BuildInfoDiagnostic, String> {
        let o = as_object(data)?;
        Ok(BuildInfoDiagnostic {
            file: get_uint(o, "file")?,
            no_file: get_bool(o, "noFile")?,
            pos: get_uint(o, "pos")?,
            end: get_uint(o, "end")?,
            code: get_int(o, "code")?,
            category: category_from_i32(get_int(o, "category")?),
            source: get_string(o, "source")?,
            message_text: get_string(o, "messageText")?,
            message_key: get_string(o, "messageKey")?,
            message_args: get_string_array(o, "messageArgs")?,
            message_chain: get_array(o, "messageChain", BuildInfoDiagnostic::unmarshal_json)?,
            related_information: get_array(
                o,
                "relatedInformation",
                BuildInfoDiagnostic::unmarshal_json,
            )?,
            reports_unnecessary: get_bool(o, "reportsUnnecessary")?,
            reports_deprecated: get_bool(o, "reportsDeprecated")?,
            skipped_on_no_emit: get_bool(o, "skippedOnNoEmit")?,
            repopulate_info: match o.get("repopulateInfo") {
                None | Some(Value::Null) => None,
                Some(v) => Some(BuildInfoRepopulateInfo::unmarshal_json(v)?),
            },
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildInfoRepopulateInfo {
    pub kind: RepopulateDiagnosticKind,
    pub module_reference: String,
    pub mode: ResolutionMode,
    pub package_name: String,
}

impl BuildInfoRepopulateInfo {
    fn marshal_json(&self) -> Value {
        let mut o = Obj::default();
        o.set("kind", num(self.kind as i32));
        o.str("moduleReference", &self.module_reference);
        o.int("mode", self.mode.value());
        o.str("packageName", &self.package_name);
        o.done()
    }

    fn unmarshal_json(data: &Value) -> Result<BuildInfoRepopulateInfo, String> {
        let o = as_object(data)?;
        let kind = match get_int(o, "kind")? {
            1 => RepopulateDiagnosticKind::ModeMismatch,
            2 => RepopulateDiagnosticKind::ModuleNotFound,
            k => return Err(format!("invalid RepopulateDiagnosticKind: {k}")),
        };
        Ok(BuildInfoRepopulateInfo {
            kind,
            module_reference: get_string(o, "moduleReference")?,
            mode: module_kind_from_i32(get_int(o, "mode")?),
            package_name: get_string(o, "packageName")?,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BuildInfoDiagnosticsOfFile {
    pub file_id: BuildInfoFileId,
    pub diagnostics: Vec<BuildInfoDiagnostic>,
}

impl BuildInfoDiagnosticsOfFile {
    // buildInfo.go:234
    fn marshal_json(&self) -> Value {
        // Go marshals a nil []*BuildInfoDiagnostic inside []any as null.
        Value::Array(vec![
            num(self.file_id),
            Value::Array(self.diagnostics.iter().map(BuildInfoDiagnostic::marshal_json).collect()),
        ])
    }

    // buildInfo.go:241
    fn unmarshal_json(data: &Value) -> Result<BuildInfoDiagnosticsOfFile, String> {
        let Value::Array(file_id_and_diagnostics) = data else {
            return Err(format!("invalid BuildInfoDiagnosticsOfFile: {}", show(data)));
        };
        if file_id_and_diagnostics.len() != 2 {
            return Err(format!(
                "invalid BuildInfoDiagnosticsOfFile: expected 2 elements, got {}",
                file_id_and_diagnostics.len()
            ));
        }
        let file_id = as_uint(&file_id_and_diagnostics[0])
            .ok_or_else(|| "invalid fileId in BuildInfoDiagnosticsOfFile".to_string())?;
        let diagnostics = match &file_id_and_diagnostics[1] {
            Value::Null => Vec::new(),
            Value::Array(items) => {
                items.iter().map(BuildInfoDiagnostic::unmarshal_json).collect::<Result<_, _>>()?
            }
            _ => return Err("invalid diagnostics in BuildInfoDiagnosticsOfFile".to_string()),
        };
        Ok(BuildInfoDiagnosticsOfFile { file_id, diagnostics })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BuildInfoSemanticDiagnostic {
    pub file_id: BuildInfoFileId, // File is not in changedSet and still doesnt have cached diagnostics
    pub diagnostics: Option<BuildInfoDiagnosticsOfFile>, // Diagnostics for file
}

impl BuildInfoSemanticDiagnostic {
    // buildInfo.go:271
    fn marshal_json(&self) -> Value {
        if self.file_id != 0 {
            return num(self.file_id);
        }
        match &self.diagnostics {
            Some(d) => d.marshal_json(),
            None => Value::Null,
        }
    }

    // buildInfo.go:278
    fn unmarshal_json(data: &Value) -> Result<BuildInfoSemanticDiagnostic, String> {
        if let Some(file_id) = as_uint(data) {
            return Ok(BuildInfoSemanticDiagnostic { file_id, diagnostics: None });
        }
        let diagnostics = BuildInfoDiagnosticsOfFile::unmarshal_json(data)
            .map_err(|_| format!("invalid BuildInfoSemanticDiagnostic: {}", show(data)))?;
        Ok(BuildInfoSemanticDiagnostic { file_id: 0, diagnostics: Some(diagnostics) })
    }
}

// fileId if pending emit is same as what compilerOptions suggest
// [fileId] if pending emit is only dts file emit
// [fileId, emitKind] if any other type emit is pending
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildInfoFilePendingEmit {
    pub file_id: BuildInfoFileId,
    pub emit_kind: FileEmitKind,
}

impl BuildInfoFilePendingEmit {
    // buildInfo.go:304
    pub fn marshal_json(&self) -> Value {
        if self.emit_kind == FileEmitKind::None {
            return num(self.file_id);
        }
        if self.emit_kind == FileEmitKind::Dts {
            return Value::Array(vec![num(self.file_id)]);
        }
        Value::Array(vec![num(self.file_id), num(self.emit_kind.bits())])
    }

    // buildInfo.go:316
    fn unmarshal_json(data: &Value) -> Result<BuildInfoFilePendingEmit, String> {
        if let Some(file_id) = as_uint(data) {
            return Ok(BuildInfoFilePendingEmit { file_id, emit_kind: FileEmitKind::None });
        }
        let int_tuple = as_uint_list(data)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("invalid BuildInfoFilePendingEmit: {}", show(data)))?;
        match int_tuple.len() {
            1 => {
                Ok(BuildInfoFilePendingEmit { file_id: int_tuple[0], emit_kind: FileEmitKind::Dts })
            }
            2 => Ok(BuildInfoFilePendingEmit {
                file_id: int_tuple[0],
                emit_kind: FileEmitKind::from_bits_retain(int_tuple[1]),
            }),
            n => {
                Err(format!("invalid BuildInfoFilePendingEmit: expected 1 or 2 integers, got {n}"))
            }
        }
    }
}

// [fileId, signature] if different from file's signature
// fileId if file wasnt emitted
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildInfoEmitSignature {
    pub file_id: BuildInfoFileId,
    pub signature: String, // Signature if it is different from file's Signature
    pub differs_only_in_dts_map: bool, // true if signature is different only in dtsMap value
    pub differs_in_options: bool, // true if signature is different in options used to emit file
}

impl BuildInfoEmitSignature {
    // buildInfo.go:353
    pub(crate) fn no_emit_signature(&self) -> bool {
        self.signature.is_empty() && !self.differs_only_in_dts_map && !self.differs_in_options
    }

    // buildInfo.go:357
    pub(crate) fn to_emit_signature(
        &self,
        path: &Path,
        emit_signatures: &FxHashMap<Path, EmitSignature>,
    ) -> EmitSignature {
        let mut signature = String::new();
        let mut signature_with_different_options = None;
        if self.differs_only_in_dts_map {
            let info = emit_signatures.get(path).unwrap();
            signature_with_different_options = Some(vec![info.signature.clone()]);
        } else if self.differs_in_options {
            signature_with_different_options = Some(vec![self.signature.clone()]);
        } else {
            signature.clone_from(&self.signature);
        }
        EmitSignature { signature, signature_with_different_options }
    }

    // buildInfo.go:377
    pub fn marshal_json(&self) -> Value {
        if self.no_emit_signature() {
            return num(self.file_id);
        }
        let signature = if self.differs_only_in_dts_map {
            Value::Array(Vec::new())
        } else if self.differs_in_options {
            Value::Array(vec![Value::String(self.signature.clone())])
        } else {
            Value::String(self.signature.clone())
        };
        Value::Array(vec![num(self.file_id), signature])
    }

    // buildInfo.go:395
    fn unmarshal_json(data: &Value) -> Result<BuildInfoEmitSignature, String> {
        if let Some(file_id) = as_uint(data) {
            return Ok(BuildInfoEmitSignature { file_id, ..Default::default() });
        }
        let Value::Array(file_id_and_signature) = data else {
            return Err(format!("invalid BuildInfoEmitSignature: {}", show(data)));
        };
        if file_id_and_signature.len() != 2 {
            return Err(format!(
                "invalid BuildInfoEmitSignature: expected 2 elements, got {}",
                file_id_and_signature.len()
            ));
        }
        let file_id = as_uint(&file_id_and_signature[0])
            .ok_or_else(|| "invalid fileId in BuildInfoEmitSignature".to_string())?;
        let mut signature = String::new();
        let mut differs_only_in_dts_map = false;
        let mut differs_in_options = false;
        match &file_id_and_signature[1] {
            Value::String(s) => signature.clone_from(s),
            Value::Array(signature_list) => match signature_list.len() {
                0 => differs_only_in_dts_map = true,
                1 => {
                    let Value::String(sig) = &signature_list[0] else {
                        return Err("invalid signature in BuildInfoEmitSignature: expected string"
                            .to_string());
                    };
                    signature.clone_from(sig);
                    differs_in_options = true;
                }
                n => {
                    return Err(format!(
                        "invalid signature in BuildInfoEmitSignature: expected string or []string with 0 or 1 element, got {n} elements"
                    ));
                }
            },
            _ => {
                return Err(
                    "invalid signature in BuildInfoEmitSignature: expected string or []string"
                        .to_string(),
                );
            }
        }
        Ok(BuildInfoEmitSignature {
            file_id,
            signature,
            differs_only_in_dts_map,
            differs_in_options,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildInfoResolvedRoot {
    pub resolved: BuildInfoFileId,
    pub root: BuildInfoFileId,
}

impl BuildInfoResolvedRoot {
    // buildInfo.go:453
    fn marshal_json(&self) -> Value {
        Value::Array(vec![num(self.resolved), num(self.root)])
    }

    // buildInfo.go:457
    fn unmarshal_json(data: &Value) -> Result<BuildInfoResolvedRoot, String> {
        let v = as_uint_tuple::<2>(data)
            .ok_or_else(|| format!("invalid BuildInfoResolvedRoot: {}", show(data)))?;
        Ok(BuildInfoResolvedRoot { resolved: v[0], root: v[1] })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BuildInfo {
    pub version: String,

    // Common between incremental and tsc -b buildinfo for non incremental programs
    pub errors: bool,
    pub check_pending: bool,
    pub root: Vec<BuildInfoRoot>,
    pub package_jsons: Option<Vec<String>>,
    pub missing_package_jsons: Option<Vec<String>>,
    pub content_mapper_identities: Option<Vec<String>>,
    // Fast-history cache proof; Go-compatible history leaves it empty and omits it.
    pub tsrs_canonical_check_signature: String,
    // Omitted with TypeScript defaults. Also covers non-incremental build caches and lazy-member overrides.
    pub tsrs_performance: String,

    // IncrementalProgram info
    pub file_names: Vec<String>,
    pub file_infos: Vec<BuildInfoFileInfo>,
    // Go marshals a non-nil empty `FileInfos` (a program without files) as `[]`; omitzero drops only nil.
    pub file_infos_non_nil: bool,
    pub file_ids_list: Vec<Vec<BuildInfoFileId>>,
    pub options: Option<JsonMap>,
    pub referenced_map: Vec<BuildInfoReferenceMapEntry>,
    pub semantic_diagnostics_per_file: Vec<BuildInfoSemanticDiagnostic>,
    pub emit_diagnostics_per_file: Vec<BuildInfoDiagnosticsOfFile>,
    pub change_file_set: Vec<BuildInfoFileId>,
    pub affected_files_pending_emit: Vec<BuildInfoFilePendingEmit>,
    pub latest_changed_dts_file: String, // Because this is only output file in the program, we dont need fileId to deduplicate name
    pub emit_signatures: Vec<BuildInfoEmitSignature>,
    pub resolved_root: Vec<BuildInfoResolvedRoot>,

    // NonIncrementalProgram info
    pub semantic_errors: bool,
}

impl BuildInfo {
    pub(crate) fn performance_mode(canonical: bool) -> &'static str {
        match (canonical, tsrs_core::lazymembers::enabled()) {
            (false, false) => "",
            (false, true) => "lazy",
            (true, false) => "canonical",
            (true, true) => "canonical,lazy",
        }
    }

    // Go `json.Marshal(buildInfo)`.
    pub fn marshal_json(&self) -> Value {
        let mut o = Obj::default();
        o.str("version", &self.version);
        o.bool("errors", self.errors);
        o.bool("checkPending", self.check_pending);
        o.list("root", &self.root, BuildInfoRoot::marshal_json);
        o.opt_strings("packageJsons", self.package_jsons.as_deref());
        o.opt_strings("missingPackageJsons", self.missing_package_jsons.as_deref());
        o.opt_strings("contentMapperIdentities", self.content_mapper_identities.as_deref());
        o.str("tsrsCanonicalCheckSignature", &self.tsrs_canonical_check_signature);
        o.str("tsrsPerformance", &self.tsrs_performance);
        if !self.file_names.is_empty() {
            o.set("fileNames", strings(&self.file_names));
        }
        if self.file_infos_non_nil || !self.file_infos.is_empty() {
            o.set(
                "fileInfos",
                Value::Array(self.file_infos.iter().map(BuildInfoFileInfo::marshal_json).collect()),
            );
        }
        o.list("fileIdsList", &self.file_ids_list, |ids| {
            Value::Array(ids.iter().map(|&id| num(id)).collect())
        });
        if let Some(options) = &self.options {
            // omitzero on a pointer: only nil is omitted.
            o.set("options", Value::Object(options.clone()));
        }
        o.list("referencedMap", &self.referenced_map, BuildInfoReferenceMapEntry::marshal_json);
        o.list(
            "semanticDiagnosticsPerFile",
            &self.semantic_diagnostics_per_file,
            BuildInfoSemanticDiagnostic::marshal_json,
        );
        o.list(
            "emitDiagnosticsPerFile",
            &self.emit_diagnostics_per_file,
            BuildInfoDiagnosticsOfFile::marshal_json,
        );
        o.list("changeFileSet", &self.change_file_set, |&id| num(id));
        o.list(
            "affectedFilesPendingEmit",
            &self.affected_files_pending_emit,
            BuildInfoFilePendingEmit::marshal_json,
        );
        o.str("latestChangedDtsFile", &self.latest_changed_dts_file);
        o.list("emitSignatures", &self.emit_signatures, BuildInfoEmitSignature::marshal_json);
        o.list("resolvedRoot", &self.resolved_root, BuildInfoResolvedRoot::marshal_json);
        o.bool("semanticErrors", self.semantic_errors);
        o.done()
    }

    // Go `json.Unmarshal(data, &buildInfo)`.
    fn unmarshal_json(data: &Value) -> Result<BuildInfo, String> {
        Self::unmarshal_json_with_canonical_diagnostics(data, true)
    }

    fn unmarshal_json_with_canonical_diagnostics(
        data: &Value,
        canonical_diagnostics: bool,
    ) -> Result<BuildInfo, String> {
        let o = as_object(data)?;
        // Both directions require a fresh check: canonical diagnostics cannot be reused by Go/API history,
        // and a fast invocation must not reuse diagnostics computed with the TypeScript defaults.
        let canonical = canonical_diagnostics && !tsrs_core::compat::go_compatible_history();
        let tsrs_performance = get_string(o, "tsrsPerformance")?;
        if tsrs_performance != Self::performance_mode(canonical)
            || !canonical && o.contains_key("tsrsCanonicalCheckSignature")
        {
            return Err("build info cannot be reused with this diagnostic policy".to_string());
        }
        Ok(BuildInfo {
            tsrs_performance,
            version: get_string(o, "version")?,
            errors: get_bool(o, "errors")?,
            check_pending: get_bool(o, "checkPending")?,
            root: get_array(o, "root", BuildInfoRoot::unmarshal_json)?,
            package_jsons: get_opt_string_array(o, "packageJsons")?,
            missing_package_jsons: get_opt_string_array(o, "missingPackageJsons")?,
            content_mapper_identities: get_opt_string_array(o, "contentMapperIdentities")?,
            tsrs_canonical_check_signature: match o.get("tsrsCanonicalCheckSignature") {
                Some(Value::String(signature))
                    if signature.strip_prefix("go4-v1:").is_some_and(|hash| {
                        hash.len() == 32
                            && hash
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    }) =>
                {
                    signature.clone()
                }
                _ => String::new(),
            },
            file_names: get_string_array(o, "fileNames")?,
            file_infos: get_array(o, "fileInfos", BuildInfoFileInfo::unmarshal_json)?,
            file_infos_non_nil: matches!(o.get("fileInfos"), Some(Value::Array(_))),
            file_ids_list: get_array(o, "fileIdsList", |v| {
                as_uint_list(v).ok_or_else(|| "invalid fileIdsList".to_string())
            })?,
            options: match o.get("options") {
                None | Some(Value::Null) => None,
                Some(Value::Object(m)) => Some(m.clone()),
                Some(_) => return Err("invalid options".to_string()),
            },
            referenced_map: get_array(
                o,
                "referencedMap",
                BuildInfoReferenceMapEntry::unmarshal_json,
            )?,
            semantic_diagnostics_per_file: get_array(
                o,
                "semanticDiagnosticsPerFile",
                BuildInfoSemanticDiagnostic::unmarshal_json,
            )?,
            emit_diagnostics_per_file: get_array(
                o,
                "emitDiagnosticsPerFile",
                BuildInfoDiagnosticsOfFile::unmarshal_json,
            )?,
            change_file_set: get_array(o, "changeFileSet", |v| {
                as_uint(v).ok_or_else(|| "invalid changeFileSet".to_string())
            })?,
            affected_files_pending_emit: get_array(
                o,
                "affectedFilesPendingEmit",
                BuildInfoFilePendingEmit::unmarshal_json,
            )?,
            latest_changed_dts_file: get_string(o, "latestChangedDtsFile")?,
            emit_signatures: get_array(
                o,
                "emitSignatures",
                BuildInfoEmitSignature::unmarshal_json,
            )?,
            resolved_root: get_array(o, "resolvedRoot", BuildInfoResolvedRoot::unmarshal_json)?,
            semantic_errors: get_bool(o, "semanticErrors")?,
        })
    }

    /// `json::marshal(&self.marshal_json())`, written straight into the output: the large arrays (`fileIdsList`
    /// holds ~9.5 M ids on the 38k-file codebase) never become a `Value` tree. Fields and omission rules are
    /// `marshal_json`'s, in its order; elements of the other lists go through their own `marshal_json`.
    pub fn marshal(&self) -> String {
        let mut out = String::with_capacity(self.marshal_size_hint());
        let mut w = ObjWriter { out: &mut out, first: true };
        w.out.push('{');
        w.str("version", &self.version);
        w.bool("errors", self.errors);
        w.bool("checkPending", self.check_pending);
        w.values("root", &self.root, BuildInfoRoot::marshal_json);
        w.opt_strings("packageJsons", self.package_jsons.as_deref());
        w.opt_strings("missingPackageJsons", self.missing_package_jsons.as_deref());
        w.opt_strings("contentMapperIdentities", self.content_mapper_identities.as_deref());
        w.str("tsrsCanonicalCheckSignature", &self.tsrs_canonical_check_signature);
        w.str("tsrsPerformance", &self.tsrs_performance);
        if !self.file_names.is_empty() {
            w.strings("fileNames", &self.file_names);
        }
        if self.file_infos_non_nil || !self.file_infos.is_empty() {
            w.key("fileInfos");
            write_array(w.out, &self.file_infos, |out, info| {
                write_compact(out, &info.marshal_json());
            });
        }
        w.list("fileIdsList", &self.file_ids_list, |out, ids| {
            write_array(out, ids, |out, &id| {
                tsrs_core::json::write_compact_int(out, id as i64);
            });
        });
        if let Some(options) = &self.options {
            // omitzero on a pointer: only nil is omitted.
            w.key("options");
            write_compact(w.out, &Value::Object(options.clone()));
        }
        w.values("referencedMap", &self.referenced_map, BuildInfoReferenceMapEntry::marshal_json);
        w.values(
            "semanticDiagnosticsPerFile",
            &self.semantic_diagnostics_per_file,
            BuildInfoSemanticDiagnostic::marshal_json,
        );
        w.values(
            "emitDiagnosticsPerFile",
            &self.emit_diagnostics_per_file,
            BuildInfoDiagnosticsOfFile::marshal_json,
        );
        w.list("changeFileSet", &self.change_file_set, |out, &id| {
            tsrs_core::json::write_compact_int(out, id as i64);
        });
        w.values(
            "affectedFilesPendingEmit",
            &self.affected_files_pending_emit,
            BuildInfoFilePendingEmit::marshal_json,
        );
        w.str("latestChangedDtsFile", &self.latest_changed_dts_file);
        w.values("emitSignatures", &self.emit_signatures, BuildInfoEmitSignature::marshal_json);
        w.values("resolvedRoot", &self.resolved_root, BuildInfoResolvedRoot::marshal_json);
        w.bool("semanticErrors", self.semantic_errors);
        out.push('}');
        out
    }

    // A rough upper estimate of the encoded size, so the output buffer is not regrown (and copied) several times.
    fn marshal_size_hint(&self) -> usize {
        let ids: usize = self.file_ids_list.iter().map(|ids| ids.len() * 7 + 2).sum();
        let names: usize = self.file_names.iter().map(|n| n.len() + 3).sum();
        ids + names
            + self.file_infos.len() * 120
            + self.referenced_map.len() * 16
            + self.semantic_diagnostics_per_file.len() * 8
            + 4096
    }

    pub fn unmarshal(text: &str) -> Result<BuildInfo, String> {
        BuildInfo::unmarshal_json(&tsrs_core::json::unmarshal(text)?)
    }

    pub(crate) fn unmarshal_with_canonical_diagnostics(
        text: &str,
        canonical_diagnostics: bool,
    ) -> Result<BuildInfo, String> {
        Self::unmarshal_json_with_canonical_diagnostics(
            &tsrs_core::json::unmarshal(text)?,
            canonical_diagnostics,
        )
    }

    // buildInfo.go:500
    pub fn is_valid_version(&self) -> bool {
        self.version == tsrs_core::version()
    }

    // ContentMapperIdentitiesMatch reports whether the content mapper identities recorded in this build info
    // match the given current identities (as produced by ContentMapperIdentities).
    pub fn content_mapper_identities_match(&self, current: Option<&[String]>) -> bool {
        self.content_mapper_identities.as_deref().unwrap_or_default() == current.unwrap_or_default()
    }

    // buildInfo.go:519
    pub fn is_incremental(&self) -> bool {
        !self.file_names.is_empty()
    }

    // buildInfo.go:527
    fn file_name(&self, file_id: BuildInfoFileId) -> &str {
        if file_id == 0 || file_id as usize > self.file_names.len() {
            return "";
        }
        &self.file_names[file_id as usize - 1]
    }

    // buildInfo.go:534
    fn file_info(&self, file_id: BuildInfoFileId) -> Option<&BuildInfoFileInfo> {
        if file_id == 0 || file_id as usize > self.file_infos.len() {
            return None;
        }
        Some(&self.file_infos[file_id as usize - 1])
    }

    // buildInfo.go:541
    pub fn get_compiler_options(&self, build_info_directory: &str) -> CompilerOptions {
        let mut options = tsrs_core::CompilerOptions::default();
        if let Some(entries) = &self.options {
            for (option, value) in entries {
                let value = json_to_option_value(value);
                if !build_info_directory.is_empty()
                    && let Some(result) = tsoptions::convert_option_to_absolute_path(
                        option,
                        &value,
                        &tsoptions::COMMAND_LINE_COMPILER_OPTIONS_MAP,
                        build_info_directory,
                    )
                {
                    tsoptions::parse_compiler_options(option, &result, &mut options);
                    continue;
                }
                tsoptions::parse_compiler_options(option, &value, &mut options);
            }
        }
        options
    }

    // buildInfo.go:557
    pub fn is_emit_pending(
        &self,
        resolved: &ParsedCommandLine,
        build_info_directory: &str,
    ) -> bool {
        let resolved_options = resolved.compiler_options().unwrap();
        // Some of the emit files like source map or dts etc are not yet done
        if !resolved_options.no_emit.is_true() || resolved_options.get_emit_declarations() {
            let mut pending_emit = get_pending_emit_kind_with_options(
                &resolved_options,
                &self.get_compiler_options(build_info_directory),
            );
            if resolved_options.no_emit.is_true() {
                pending_emit &= FileEmitKind::DtsErrors;
            }
            return pending_emit != FileEmitKind::None;
        }
        false
    }

    // buildInfo.go:569
    pub fn get_package_jsons(&self, build_info_directory: &str) -> Vec<String> {
        get_normalized_paths(
            self.package_jsons.as_deref().unwrap_or_default(),
            build_info_directory,
        )
    }

    // buildInfo.go:573
    pub fn get_missing_package_jsons(&self, build_info_directory: &str) -> Vec<String> {
        get_normalized_paths(
            self.missing_package_jsons.as_deref().unwrap_or_default(),
            build_info_directory,
        )
    }

    // buildInfo.go:589
    pub fn get_build_info_root_info_reader(
        &self,
        build_info_directory: &str,
        compare_path_options: &ComparePathsOptions,
    ) -> BuildInfoRootInfoReader {
        let mut resolved_root_file_infos: FxHashMap<Path, BuildInfoFileInfo> = FxHashMap::default();
        // Roots of the File
        let mut root_to_resolved: OrderedMap<Path, Path> = OrderedMap::default();
        let mut resolved_to_root: FxHashMap<Path, Path> = FxHashMap::default();
        let to_path = |file_name: &str| {
            tspath::to_path(
                file_name,
                build_info_directory,
                compare_path_options.use_case_sensitive_file_names,
            )
        };

        // Create map from resolvedRoot to Root
        resolved_to_root.extend(self.resolved_root.iter().filter_map(|resolved| {
            let resolved_root = self.file_name(resolved.resolved);
            let root = self.file_name(resolved.root);
            if !resolved_root.is_empty() && !root.is_empty() {
                Some((to_path(resolved_root), to_path(root)))
            } else {
                None
            }
        }));

        let mut add_root = |resolved_root: &str, file_info: Option<&BuildInfoFileInfo>| {
            if resolved_root.is_empty() {
                return;
            }
            let resolved_root_path = to_path(resolved_root);
            if let Some(root_path) = resolved_to_root.get(&resolved_root_path) {
                root_to_resolved.insert(root_path.clone(), resolved_root_path.clone());
            } else {
                root_to_resolved.insert(resolved_root_path.clone(), resolved_root_path.clone());
            }
            if let Some(file_info) = file_info {
                resolved_root_file_infos.insert(resolved_root_path, file_info.clone());
            }
        };

        for root in &self.root {
            if !root.non_incremental.is_empty() {
                add_root(&root.non_incremental, None);
            } else if root.end == 0 {
                add_root(self.file_name(root.start), self.file_info(root.start));
            } else {
                for i in root.start..=root.end {
                    add_root(self.file_name(i), self.file_info(i));
                }
            }
        }

        BuildInfoRootInfoReader { resolved_root_file_infos, root_to_resolved }
    }
}

// ContentMapperIdentities returns the project's sorted mapper transform identities. A nil project means
// the compiler host has no configured content mappers.
// buildInfo.go:501 (`None` is Go's nil slice.)
pub fn content_mapper_identities(
    project: Option<&dyn tsrs_contentmapper::Project>,
) -> Result<Option<Vec<String>>, tsrs_contentmapper::Error> {
    let Some(project) = project else {
        return Ok(None);
    };
    project.identities().map(Some)
}

// buildInfo.go:523
pub fn is_build_info_file_name_default_library(file_name: &str) -> bool {
    !tspath::path_is_relative(file_name) && !tspath::path_is_absolute(file_name)
}

// buildInfo.go:577
fn get_normalized_paths(paths: &[String], build_info_directory: &str) -> Vec<String> {
    paths
        .iter()
        .map(|path| tspath::get_normalized_absolute_path(path, build_info_directory))
        .collect()
}

pub struct BuildInfoRootInfoReader {
    resolved_root_file_infos: FxHashMap<Path, BuildInfoFileInfo>,
    root_to_resolved: OrderedMap<Path, Path>,
}

impl BuildInfoRootInfoReader {
    // buildInfo.go:640
    pub fn get_build_info_file_info(
        &self,
        input_file_path: &Path,
    ) -> (Option<&BuildInfoFileInfo>, Path) {
        if let Some(info) = self.resolved_root_file_infos.get(input_file_path) {
            return (Some(info), input_file_path.clone());
        }
        if let Some(resolved) = self.root_to_resolved.get(input_file_path) {
            return (self.resolved_root_file_infos.get(resolved), resolved.clone());
        }
        (None, Path::default())
    }

    // buildInfo.go:650
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.root_to_resolved.keys()
    }
}

// JSON helpers.

fn write_compact(out: &mut String, v: &Value) {
    tsrs_core::json::write_compact(out, v)
        .unwrap_or_else(|err| panic!("Failed to marshal build info: {err}"));
}

fn write_array<T>(out: &mut String, items: &[T], mut f: impl FnMut(&mut String, &T)) {
    out.push('[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        f(out, item);
    }
    out.push(']');
}

// `Obj` for BuildInfo::marshal: writes the members as it goes, with the same omission rules.
struct ObjWriter<'a> {
    out: &'a mut String,
    first: bool,
}

impl ObjWriter<'_> {
    fn key(&mut self, key: &str) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        tsrs_core::json::write_compact_string(self.out, key);
        self.out.push(':');
    }
    fn str(&mut self, key: &str, v: &str) {
        if !v.is_empty() {
            self.key(key);
            tsrs_core::json::write_compact_string(self.out, v);
        }
    }
    fn bool(&mut self, key: &str, v: bool) {
        if v {
            self.key(key);
            self.out.push_str("true");
        }
    }
    fn list<T>(&mut self, key: &str, items: &[T], f: impl FnMut(&mut String, &T)) {
        if !items.is_empty() {
            self.key(key);
            write_array(self.out, items, f);
        }
    }
    fn values<T>(&mut self, key: &str, items: &[T], f: impl Fn(&T) -> Value) {
        self.list(key, items, |out, item| write_compact(out, &f(item)));
    }
    fn strings(&mut self, key: &str, v: &[String]) {
        self.key(key);
        write_array(self.out, v, |out, s| {
            tsrs_core::json::write_compact_string(out, s);
        });
    }
    fn opt_strings(&mut self, key: &str, v: Option<&[String]>) {
        // omitzero on a slice omits nil and empty slices.
        if let Some(v) = v
            && !v.is_empty()
        {
            self.strings(key, v);
        }
    }
}

#[derive(Default)]
struct Obj(JsonMap);

impl Obj {
    fn set(&mut self, key: &str, v: Value) {
        self.0.insert(key.to_string(), v);
    }
    fn str(&mut self, key: &str, v: &str) {
        if !v.is_empty() {
            self.set(key, Value::String(v.to_string()));
        }
    }
    fn bool(&mut self, key: &str, v: bool) {
        if v {
            self.set(key, Value::Bool(true));
        }
    }
    fn int(&mut self, key: &str, v: i32) {
        if v != 0 {
            self.set(key, num(v));
        }
    }
    fn uint(&mut self, key: &str, v: u32) {
        if v != 0 {
            self.set(key, Value::Number(v as f64));
        }
    }
    fn list<T>(&mut self, key: &str, items: &[T], f: impl Fn(&T) -> Value) {
        if !items.is_empty() {
            self.set(key, Value::Array(items.iter().map(f).collect()));
        }
    }
    fn opt_strings(&mut self, key: &str, v: Option<&[String]>) {
        // omitzero on a slice omits nil and empty slices.
        if let Some(v) = v
            && !v.is_empty()
        {
            self.set(key, strings(v));
        }
    }
    fn done(self) -> Value {
        Value::Object(self.0)
    }
}

fn num(v: impl Into<f64>) -> Value {
    Value::Number(v.into())
}

fn strings(v: &[String]) -> Value {
    Value::Array(v.iter().map(|s| Value::String(s.clone())).collect())
}

fn show(v: &Value) -> String {
    tsrs_core::json::marshal(v).unwrap_or_default()
}

fn as_int(v: &Value) -> Option<i32> {
    match v {
        Value::Number(n) if n.fract() == 0.0 => Some(*n as i32),
        _ => None,
    }
}

fn as_uint(v: &Value) -> Option<u32> {
    match v {
        Value::Number(n) if n.fract() == 0.0 && *n >= 0.0 && *n <= u32::MAX as f64 => {
            Some(*n as u32)
        }
        _ => None,
    }
}

fn as_uint_list(v: &Value) -> Option<Vec<u32>> {
    match v {
        Value::Array(items) => items.iter().map(as_uint).collect(),
        Value::Null => Some(Vec::new()),
        _ => None,
    }
}

fn as_uint_tuple<const N: usize>(v: &Value) -> Option<[u32; N]> {
    // Go unmarshals a JSON array into a [N]int by position: missing elements stay zero, extra ones are dropped.
    let Value::Array(items) = v else { return None };
    let mut out = [0; N];
    for (i, item) in items.iter().enumerate() {
        let n = as_uint(item)?;
        if i < N {
            out[i] = n;
        }
    }
    Some(out)
}

fn as_object(v: &Value) -> Result<&JsonMap, String> {
    match v {
        Value::Object(o) => Ok(o),
        _ => Err(format!("expected object, got {}", show(v))),
    }
}

fn get_string(o: &JsonMap, key: &str) -> Result<String, String> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(v) => Err(format!("invalid {key}: {}", show(v))),
    }
}

fn get_bool(o: &JsonMap, key: &str) -> Result<bool, String> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(v) => Err(format!("invalid {key}: {}", show(v))),
    }
}

fn get_int(o: &JsonMap, key: &str) -> Result<i32, String> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(v) => as_int(v).ok_or_else(|| format!("invalid {key}: {}", show(v))),
    }
}

fn get_uint(o: &JsonMap, key: &str) -> Result<u32, String> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(n)) if n.fract() == 0.0 && *n >= 0.0 && *n <= u32::MAX as f64 => {
            Ok(*n as u32)
        }
        Some(v) => Err(format!("invalid {key}: {}", show(v))),
    }
}

fn get_array<T>(
    o: &JsonMap,
    key: &str,
    f: impl Fn(&Value) -> Result<T, String>,
) -> Result<Vec<T>, String> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(f).collect(),
        Some(v) => Err(format!("invalid {key}: {}", show(v))),
    }
}

fn get_string_array(o: &JsonMap, key: &str) -> Result<Vec<String>, String> {
    Ok(get_opt_string_array(o, key)?.unwrap_or_default())
}

fn get_opt_string_array(o: &JsonMap, key: &str) -> Result<Option<Vec<String>>, String> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => Ok(s.clone()),
                _ => Err(format!("invalid {key}")),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(v) => Err(format!("invalid {key}: {}", show(v))),
    }
}

pub fn category_to_i32(c: Category) -> i32 {
    match c {
        Category::Warning => 0,
        Category::Error => 1,
        Category::Suggestion => 2,
        Category::Message => 3,
    }
}

fn category_from_i32(c: i32) -> Category {
    match c {
        1 => Category::Error,
        2 => Category::Suggestion,
        3 => Category::Message,
        _ => Category::Warning,
    }
}

fn module_kind_from_i32(v: i32) -> ModuleKind {
    match v {
        1 => ModuleKind::COMMON_JS,
        2 => ModuleKind::AMD,
        3 => ModuleKind::UMD,
        4 => ModuleKind::SYSTEM,
        5 => ModuleKind::ES2015,
        6 => ModuleKind::ES2020,
        7 => ModuleKind::ES2022,
        99 => ModuleKind::ES_NEXT,
        100 => ModuleKind::NODE16,
        101 => ModuleKind::NODE18,
        102 => ModuleKind::NODE20,
        199 => ModuleKind::NODE_NEXT,
        200 => ModuleKind::PRESERVE,
        _ => ModuleKind::NONE,
    }
}

// A JSON value decoded into Go `any` (float64 numbers, []any arrays, map objects).
fn json_to_option_value(v: &Value) -> CompilerOptionsValue {
    match v {
        Value::Null => CompilerOptionsValue::Null,
        Value::Bool(b) => CompilerOptionsValue::Bool(*b),
        Value::Integer(n) => CompilerOptionsValue::Float(*n as f64),
        Value::Number(n) => CompilerOptionsValue::Float(*n),
        Value::String(s) => CompilerOptionsValue::String(s.clone()),
        Value::Array(items) => {
            CompilerOptionsValue::Array(items.iter().map(json_to_option_value).collect())
        }
        Value::Object(m) => CompilerOptionsValue::Object(
            m.iter().map(|(k, v)| (k.to_string(), json_to_option_value(v))).collect(),
        ),
    }
}
