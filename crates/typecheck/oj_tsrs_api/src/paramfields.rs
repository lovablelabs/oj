// Generated from proto.go (pinned) by a script that is not checked in, so update it by hand when proto.go changes.
// It lists the fields of each method's params struct and of the api-package structs nested in them, their decode
// kinds and, for arrays, element kinds; for Go-order decoding checks before any lookup (`predecode.rs`). Unknown
// keys are ignored like Go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Str,
    I32,
    I64,
    U32,
    U64,
    Bool,
    Doc,
    DocList,
    Array,
    Object,
    Any,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elem {
    Str,
    I32,
    I64,
    U32,
    U64,
    Bool,
    Object,
    Any,
}

pub struct FieldSpec {
    pub name: &'static str,
    pub go_type: &'static str,
    pub kind: FieldKind,
    pub nullable: bool,
    pub elem: Option<Elem>,
}

pub fn params_fields(method: &str) -> &'static [FieldSpec] {
    match method {
        "batchRequests" => &[
            FieldSpec {
                name: "requests",
                go_type: "[]BatchRequest",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
            FieldSpec {
                name: "continuationToken",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "maxResponseBytesPerPage",
                go_type: "int",
                kind: FieldKind::I64,
                nullable: false,
                elem: None,
            },
        ],
        "release" | "ojIncrementalPrograms" => &[FieldSpec {
            name: "snapshot",
            go_type: "SnapshotID",
            kind: FieldKind::U64,
            nullable: false,
            elem: None,
        }],
        "releaseSourceFile" => &[FieldSpec {
            name: "lease",
            go_type: "SourceFileLeaseID",
            kind: FieldKind::U64,
            nullable: false,
            elem: None,
        }],
        "retainSourceFile" | "getCachedSourceFile" => &[FieldSpec {
            name: "file",
            go_type: "SourceFileDescriptor",
            kind: FieldKind::Object,
            nullable: false,
            elem: None,
        }],
        "getSymbolOfDeclaration" => &[
            FieldSpec {
                name: "file",
                go_type: "SourceFileDescriptor",
                kind: FieldKind::Object,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "index",
                go_type: "uint32",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
        ],
        "createSnapshot" => &[
            FieldSpec {
                name: "openProjects",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "closeProjects",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "openFiles",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "closeFiles",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "createPrograms",
                go_type: "[]*CreateSnapshotProgramParams",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
            FieldSpec {
                name: "reconfigurePrograms",
                go_type: "[]*ReconfigureSnapshotProgramParams",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
            FieldSpec {
                name: "removePrograms",
                go_type: "[]project.SyntheticProjectID",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Any),
            },
            FieldSpec {
                name: "ensurePrograms",
                go_type: "EnsurePrograms",
                kind: FieldKind::Any,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "fileNotifications",
                go_type: "FileNotifications",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "fileSystem",
                go_type: "requestfilesystem.RequestFileSystem",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
        ],
        "updateSnapshot" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "changes",
                go_type: "CreateSnapshotParams",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
        ],
        "getCurrentLanguageServerSnapshot" => &[
            FieldSpec {
                name: "baseSnapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "changes",
                go_type: "LanguageServerSnapshotChanges",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
        ],
        "createBuildOrchestrator" => &[
            FieldSpec {
                name: "rootNames",
                go_type: "[]string",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Str),
            },
            FieldSpec {
                name: "cwd",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "buildOptions",
                go_type: "core.BuildOptions",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "compilerOptions",
                go_type: "core.CompilerOptions",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
        ],
        "disposeBuildOrchestrator" => &[FieldSpec {
            name: "buildOrchestratorID",
            go_type: "BuildOrchestratorID",
            kind: FieldKind::U64,
            nullable: false,
            elem: None,
        }],
        "build" | "buildReferences" | "cleanBuild" | "cleanReferences" => &[
            FieldSpec {
                name: "buildOrchestratorID",
                go_type: "BuildOrchestratorID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
        ],
        "createModuleResolver" => &[
            FieldSpec {
                name: "compilerOptions",
                go_type: "core.CompilerOptions",
                kind: FieldKind::Object,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "moduleResolutions",
                go_type: "ModuleResolutionSpec",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "resolveModuleNameCallback",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
        ],
        "releaseModuleResolver" => &[FieldSpec {
            name: "resolver",
            go_type: "ModuleResolverID",
            kind: FieldKind::U64,
            nullable: false,
            elem: None,
        }],
        "resolveModuleName" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "inProgressSnapshot",
                go_type: "uint64",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "resolver",
                go_type: "ModuleResolverID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "moduleName",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "containingDirectory",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "resolutionMode",
                go_type: "ResolutionMode",
                kind: FieldKind::I32,
                nullable: true,
                elem: None,
            },
        ],
        "ojStartSignatureJob" => &[
            FieldSpec {
                name: "skip",
                go_type: "[]string",
                kind: FieldKind::Array,
                nullable: true,
                elem: Some(Elem::Str),
            },
            FieldSpec {
                name: "slowMs",
                go_type: "uint64",
                kind: FieldKind::U64,
                nullable: true,
                elem: None,
            },
        ],
        "parseCommandLine" => &[FieldSpec {
            name: "commandLine",
            go_type: "[]string",
            kind: FieldKind::Array,
            nullable: false,
            elem: Some(Elem::Str),
        }],
        "readConfigFile" | "parseConfigFile" => &[FieldSpec {
            name: "file",
            go_type: "DocumentIdentifier",
            kind: FieldKind::Doc,
            nullable: false,
            elem: None,
        }],
        "parseJsonConfigFileContent" => &[
            FieldSpec {
                name: "json",
                go_type: "packagejson.JSONValue",
                kind: FieldKind::Any,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "configDirectory",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "configFileName",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: true,
                elem: None,
            },
        ],
        "createSourceFile" => &[
            FieldSpec {
                name: "fileName",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "sourceText",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "options",
                go_type: "CreateSourceFileOptions",
                kind: FieldKind::Object,
                nullable: false,
                elem: None,
            },
        ],
        "createSourceFileFromFile" => &[
            FieldSpec {
                name: "fileName",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "options",
                go_type: "CreateSourceFileOptions",
                kind: FieldKind::Object,
                nullable: false,
                elem: None,
            },
        ],
        "transpileModule" | "transpileDeclaration" => &[
            FieldSpec {
                name: "input",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "options",
                go_type: "TranspileOptions",
                kind: FieldKind::Object,
                nullable: false,
                elem: None,
            },
        ],
        "transpileModuleFromFile" | "transpileDeclarationFromFile" => &[
            FieldSpec {
                name: "fileName",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "options",
                go_type: "TranspileOptions",
                kind: FieldKind::Object,
                nullable: false,
                elem: None,
            },
        ],
        "getDefaultProjectForFile" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
        ],
        "getSourceFile"
        | "getSourceFileMetadata"
        | "getConfigSourceFile"
        | "getSymbolOfSourceFile" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
        ],
        "getSourceFileNames"
        | "getConfigFileNames"
        | "getAnyType"
        | "getStringType"
        | "getNumberType"
        | "getBooleanType"
        | "getVoidType"
        | "getUndefinedType"
        | "getNullType"
        | "getNeverType"
        | "getUnknownType"
        | "getBigIntType"
        | "getESSymbolType"
        | "getNonPrimitiveType"
        | "getWellKnownSymbols"
        | "getWellKnownSignatures"
        | "getProgramDiagnostics"
        | "ojGetIncrementalSemanticDiagnostics"
        | "ojGetGlobalDiagnostics"
        | "getGlobalDiagnostics"
        | "getConfigFileParsingDiagnostics" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
        ],
        "getModeForUsageLocation" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "usage",
                go_type: "NodeHandle",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
        ],
        "getModeForResolutionAtIndex" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "index",
                go_type: "int",
                kind: FieldKind::I64,
                nullable: false,
                elem: None,
            },
        ],
        "getResolvedModule" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "moduleName",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "mode",
                go_type: "core.ResolutionMode",
                kind: FieldKind::I32,
                nullable: false,
                elem: None,
            },
        ],
        "getResolvedModuleFromModuleSpecifier" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "moduleSpecifier",
                go_type: "NodeHandle",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "sourceFile",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: true,
                elem: None,
            },
        ],
        "getResolvedTypeReferenceDirective" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "typeDirectiveName",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "mode",
                go_type: "core.ResolutionMode",
                kind: FieldKind::I32,
                nullable: false,
                elem: None,
            },
        ],
        "getResolvedTypeReferenceDirectiveFromTypeReferenceDirective" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "sourceFile",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "typeDirectiveName",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "resolutionMode",
                go_type: "core.ResolutionMode",
                kind: FieldKind::I32,
                nullable: false,
                elem: None,
            },
        ],
        "getSymbolAtPosition" | "getTypeAtPosition" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "position",
                go_type: "uint32",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
        ],
        "getSymbolsAtPositions" | "getTypesAtPositions" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "positions",
                go_type: "[]uint32",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::U32),
            },
        ],
        "getSymbolAtLocation"
        | "getResolvedSignature"
        | "getTypeAtLocation"
        | "getContextualType"
        | "getTypeFromTypeNode"
        | "getShorthandAssignmentValueSymbol"
        | "isContextSensitive"
        | "getConstantValue"
        | "getSignatureFromDeclaration"
        | "getExportSpecifierLocalTargetSymbol" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "location",
                go_type: "NodeHandle",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
        ],
        "getSymbolsAtLocations" | "getTypeAtLocations" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "locations",
                go_type: "[]NodeHandle",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Str),
            },
        ],
        "getSymbolsOfSourceFiles"
        | "getJavaScriptEmit"
        | "getDeclarationEmit"
        | "getSyntacticDiagnostics"
        | "getBindDiagnostics"
        | "getSemanticDiagnostics"
        | "getSuggestionDiagnostics"
        | "getDeclarationDiagnostics" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "files",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
        ],
        "getTypeOfSymbol"
        | "getDeclaredTypeOfSymbol"
        | "getNonMissingTypeOfSymbol"
        | "getAliasedSymbol"
        | "getImmediateAliasedSymbol"
        | "getTargetSymbol"
        | "getExportSymbolOfSymbolForChecker"
        | "getFullyQualifiedName"
        | "getExportsOfModule"
        | "getJsDocTags"
        | "getDocumentationComment"
        | "isReadonlySymbol" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "symbol",
                go_type: "SymbolReference",
                kind: FieldKind::Object,
                nullable: false,
                elem: None,
            },
        ],
        "getTypesOfSymbols" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "symbols",
                go_type: "[]SymbolReference",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
        ],
        "resolveName" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "name",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "location",
                go_type: "NodeHandle",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "position",
                go_type: "uint32",
                kind: FieldKind::U32,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "meaning",
                go_type: "uint32",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "excludeGlobals",
                go_type: "bool",
                kind: FieldKind::Bool,
                nullable: false,
                elem: None,
            },
        ],
        "getSymbolsInScope" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "location",
                go_type: "NodeHandle",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "position",
                go_type: "uint32",
                kind: FieldKind::U32,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "meaning",
                go_type: "uint32",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
        ],
        "getSignaturesOfType" | "getIndexInfoOfType" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "type",
                go_type: "TypeID",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "kind",
                go_type: "int32",
                kind: FieldKind::I32,
                nullable: false,
                elem: None,
            },
        ],
        "getParentOfSymbol"
        | "getMembersOfSymbol"
        | "getExportsOfSymbol"
        | "getExportSymbolOfSymbol" => &[FieldSpec {
            name: "symbol",
            go_type: "SymbolReference",
            kind: FieldKind::Object,
            nullable: false,
            elem: None,
        }],
        "getSymbolOfType"
        | "getTargetOfType"
        | "getFreshTypeOfType"
        | "getRegularTypeOfType"
        | "getTypesOfType"
        | "getTypeParametersOfType"
        | "getOuterTypeParametersOfType"
        | "getLocalTypeParametersOfType"
        | "getThisTypeOfType"
        | "getAliasTypeArgumentsOfType"
        | "getAliasSymbolOfType"
        | "getObjectTypeOfType"
        | "getIndexTypeOfType"
        | "getCheckTypeOfType"
        | "getExtendsTypeOfType"
        | "getBaseTypeOfType"
        | "getConstraintOfType"
        | "getTypeParameterOfMappedType"
        | "getConstraintTypeOfMappedType"
        | "getNameTypeOfMappedType"
        | "getTemplateTypeOfMappedType"
        | "getTrueTypeOfConditionalType"
        | "getFalseTypeOfConditionalType"
        | "getNonNullableType"
        | "getApparentPropertiesOfType"
        | "getApparentType"
        | "getReducedType"
        | "getConstraintOfTypeParameter"
        | "getDefaultFromTypeParameter" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "objectId",
                go_type: "TypeID",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
        ],
        "getTypeParametersOfSignature"
        | "getParametersOfSignature"
        | "getThisParameterOfSignature"
        | "getTargetOfSignature"
        | "getReturnTypeOfSignature" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "objectId",
                go_type: "SignatureID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
        ],
        "getContextualTypeForArgument" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "location",
                go_type: "NodeHandle",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "index",
                go_type: "int32",
                kind: FieldKind::I32,
                nullable: false,
                elem: None,
            },
        ],
        "getAwaitedType"
        | "getBaseTypeOfLiteralType"
        | "getWidenedType"
        | "isArrayLikeType"
        | "getBaseTypes"
        | "getPropertiesOfType"
        | "getIndexInfosOfType"
        | "getBaseConstraintOfType"
        | "getTypeArguments"
        | "isArrayType" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "type",
                go_type: "TypeID",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
        ],
        "getParameterType" | "getTypeParameterAtPosition" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "signature",
                go_type: "SignatureID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "index",
                go_type: "int32",
                kind: FieldKind::I32,
                nullable: false,
                elem: None,
            },
        ],
        "isTypeAssignableTo" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "source",
                go_type: "TypeID",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "target",
                go_type: "TypeID",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
        ],
        "getTypeOfSymbolAtLocation" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "symbol",
                go_type: "SymbolReference",
                kind: FieldKind::Object,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "location",
                go_type: "NodeHandle",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
        ],
        "typeToTypeNode" | "typeToString" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "type",
                go_type: "TypeID",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "location",
                go_type: "NodeHandle",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "flags",
                go_type: "int32",
                kind: FieldKind::I32,
                nullable: false,
                elem: None,
            },
        ],
        "signatureToSignatureDeclaration" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "signature",
                go_type: "SignatureID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "kind",
                go_type: "int32",
                kind: FieldKind::I32,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "location",
                go_type: "NodeHandle",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "flags",
                go_type: "int32",
                kind: FieldKind::I32,
                nullable: false,
                elem: None,
            },
        ],
        "getRestTypeOfSignature" | "getTypePredicateOfSignature" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "signature",
                go_type: "SignatureID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
        ],
        "getPropertyOfType" | "getTypeOfPropertyOfType" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "type",
                go_type: "TypeID",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "name",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
        ],
        "getImportAdderEdits" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "actions",
                go_type: "[]ImportAdderAction",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
        ],
        "getMemberInModuleExports" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "symbol",
                go_type: "SymbolReference",
                kind: FieldKind::Object,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "name",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
        ],
        "getReferencesToSymbolInFile" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "symbol",
                go_type: "SymbolReference",
                kind: FieldKind::Object,
                nullable: false,
                elem: None,
            },
        ],
        "getReferencedSymbolsForNode" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "node",
                go_type: "NodeHandle",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "position",
                go_type: "int",
                kind: FieldKind::I64,
                nullable: false,
                elem: None,
            },
        ],
        "getSignatureUsages" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "signatureDecl",
                go_type: "NodeHandle",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
        ],
        "getCompletionsAtPosition" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "position",
                go_type: "uint32",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "triggerCharacter",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "includeSymbol",
                go_type: "bool",
                kind: FieldKind::Bool,
                nullable: false,
                elem: None,
            },
        ],
        "printNode" => &[
            FieldSpec {
                name: "data",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "preserveSourceNewlines",
                go_type: "bool",
                kind: FieldKind::Bool,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "neverAsciiEscape",
                go_type: "bool",
                kind: FieldKind::Bool,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "terminateUnterminatedLiterals",
                go_type: "bool",
                kind: FieldKind::Bool,
                nullable: false,
                elem: None,
            },
        ],
        "formatNodeForInsertion" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "position",
                go_type: "uint32",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "data",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
        ],
        "emit" | "emitToString" => &[
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "emitOnly",
                go_type: "uint32",
                kind: FieldKind::U32,
                nullable: true,
                elem: None,
            },
        ],
        "startCPUProfile" | "saveHeapProfile" => &[FieldSpec {
            name: "dir",
            go_type: "string",
            kind: FieldKind::Str,
            nullable: false,
            elem: None,
        }],
        _ => &[],
    }
}

/// Fields of api-package struct `name` (Go type name without package), for nested checks.
pub fn struct_fields(name: &str) -> &'static [FieldSpec] {
    match name {
        "BatchRequest" => &[
            FieldSpec {
                name: "method",
                go_type: "Method",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "params",
                go_type: "json.Value",
                kind: FieldKind::Any,
                nullable: false,
                elem: None,
            },
        ],
        "CreateProgramOptions" => &[
            FieldSpec {
                name: "projectReferences",
                go_type: "[]*core.ProjectReference",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
            FieldSpec {
                name: "configFileParsingDiagnostics",
                go_type: "[]*DiagnosticResponse",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
            FieldSpec {
                name: "moduleResolver",
                go_type: "ModuleResolverID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
        ],
        "CreateSnapshotParams" => &[
            FieldSpec {
                name: "openProjects",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "closeProjects",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "openFiles",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "closeFiles",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "createPrograms",
                go_type: "[]*CreateSnapshotProgramParams",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
            FieldSpec {
                name: "reconfigurePrograms",
                go_type: "[]*ReconfigureSnapshotProgramParams",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
            FieldSpec {
                name: "removePrograms",
                go_type: "[]project.SyntheticProjectID",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Any),
            },
            FieldSpec {
                name: "ensurePrograms",
                go_type: "EnsurePrograms",
                kind: FieldKind::Any,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "userPreferences",
                go_type: "*lsutil.UserPreferences",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "prepareAutoImports",
                go_type: "*DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "fileNotifications",
                go_type: "FileNotifications",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "fileSystem",
                go_type: "requestfilesystem.RequestFileSystem",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
        ],
        "CreateSnapshotProgramParams" => &[
            FieldSpec {
                name: "rootFiles",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "compilerOptions",
                go_type: "core.CompilerOptions",
                kind: FieldKind::Object,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "options",
                go_type: "CreateProgramOptions",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
        ],
        "CreateSourceFileOptions" => &[FieldSpec {
            name: "scriptKind",
            go_type: "core.ScriptKind",
            kind: FieldKind::I32,
            nullable: false,
            elem: None,
        }],
        "DiagnosticPositionResponse" => &[
            FieldSpec {
                name: "line",
                go_type: "int",
                kind: FieldKind::I64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "character",
                go_type: "core.UTF16Offset",
                kind: FieldKind::Object,
                nullable: false,
                elem: None,
            },
        ],
        "DiagnosticResponse" => &[
            FieldSpec {
                name: "fileName",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "pos",
                go_type: "int",
                kind: FieldKind::I64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "end",
                go_type: "int",
                kind: FieldKind::I64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "startPosition",
                go_type: "DiagnosticPositionResponse",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "endPosition",
                go_type: "DiagnosticPositionResponse",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "sourceLines",
                go_type: "[]*DiagnosticSourceLineResponse",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
            FieldSpec {
                name: "code",
                go_type: "int32",
                kind: FieldKind::I32,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "category",
                go_type: "diagnostics.Category",
                kind: FieldKind::I32,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "source",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "text",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "reportsUnnecessary",
                go_type: "bool",
                kind: FieldKind::Bool,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "reportsDeprecated",
                go_type: "bool",
                kind: FieldKind::Bool,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "messageChain",
                go_type: "[]*DiagnosticResponse",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
            FieldSpec {
                name: "relatedInformation",
                go_type: "[]*DiagnosticResponse",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
        ],
        "DiagnosticSourceLineResponse" => &[
            FieldSpec {
                name: "line",
                go_type: "int",
                kind: FieldKind::I64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "text",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
        ],
        "FileNotifications" => &[
            FieldSpec {
                name: "invalidateAll",
                go_type: "bool",
                kind: FieldKind::Bool,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "changed",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "created",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "deleted",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
        ],
        "ImportAdderAction" => &[
            FieldSpec {
                name: "kind",
                go_type: "ImportAdderActionKind",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "symbol",
                go_type: "SymbolReference",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "isValidTypeOnlyUseSite",
                go_type: "bool",
                kind: FieldKind::Bool,
                nullable: true,
                elem: None,
            },
        ],
        "LanguageServerSnapshotChanges" => &[
            FieldSpec {
                name: "openProjects",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "closeProjects",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "openFiles",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "closeFiles",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "createPrograms",
                go_type: "[]*CreateSnapshotProgramParams",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
            FieldSpec {
                name: "reconfigurePrograms",
                go_type: "[]*ReconfigureSnapshotProgramParams",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
            FieldSpec {
                name: "removePrograms",
                go_type: "[]project.SyntheticProjectID",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Any),
            },
            FieldSpec {
                name: "ensurePrograms",
                go_type: "EnsurePrograms",
                kind: FieldKind::Any,
                nullable: true,
                elem: None,
            },
        ],
        "ModuleResolutionEntry" => &[
            FieldSpec {
                name: "moduleName",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "containingDirectory",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "resolutionMode",
                go_type: "ResolutionMode",
                kind: FieldKind::I32,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "result",
                go_type: "StaticModuleResolution",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
        ],
        "ModuleResolutionSpec" => &[
            FieldSpec {
                name: "fallback",
                go_type: "ModuleResolutionFallback",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "entries",
                go_type: "[]*ModuleResolutionEntry",
                kind: FieldKind::Array,
                nullable: false,
                elem: Some(Elem::Object),
            },
        ],
        "PackageId" => &[
            FieldSpec {
                name: "name",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "subModuleName",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "version",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "peerDependencies",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
        ],
        "ReconfigureSnapshotProgramParams" => &[
            FieldSpec {
                name: "id",
                go_type: "project.SyntheticProjectID",
                kind: FieldKind::Any,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "rootFiles",
                go_type: "[]DocumentIdentifier",
                kind: FieldKind::DocList,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "compilerOptions",
                go_type: "core.CompilerOptions",
                kind: FieldKind::Object,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "options",
                go_type: "CreateProgramOptions",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
        ],
        "SourceFileDescriptor" => &[
            FieldSpec {
                name: "fileName",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "path",
                go_type: "tspath.Path",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "contentHash",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "parseOptionsKey",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "scriptKind",
                go_type: "core.ScriptKind",
                kind: FieldKind::I32,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "nodeId",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
        ],
        "StaticModuleResolution" => &[
            FieldSpec {
                name: "resolvedFileName",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "originalPath",
                go_type: "DocumentIdentifier",
                kind: FieldKind::Doc,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "packageId",
                go_type: "PackageId",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
        ],
        "SymbolReference" => &[
            FieldSpec {
                name: "kind",
                go_type: "SymbolOwnerKind",
                kind: FieldKind::U32,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "file",
                go_type: "SourceFileDescriptor",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "snapshot",
                go_type: "SnapshotID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "project",
                go_type: "project.ID",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "id",
                go_type: "SymbolID",
                kind: FieldKind::U64,
                nullable: false,
                elem: None,
            },
        ],
        "TranspileOptions" => &[
            FieldSpec {
                name: "compilerOptions",
                go_type: "core.CompilerOptions",
                kind: FieldKind::Object,
                nullable: true,
                elem: None,
            },
            FieldSpec {
                name: "fileName",
                go_type: "string",
                kind: FieldKind::Str,
                nullable: false,
                elem: None,
            },
            FieldSpec {
                name: "reportDiagnostics",
                go_type: "bool",
                kind: FieldKind::Bool,
                nullable: false,
                elem: None,
            },
        ],
        _ => &[],
    }
}
