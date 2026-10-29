// Go `checkerSetup` / `setupChecker` and the response constructors that need snapshot registries
// (`newSymbolResponse`, `newTypeResponse`, `newSignatureResponse`, `newIndexInfoResponse`).

use tsrs_ast::{Node, SourceFile, Symbol, SymbolFlags};
use tsrs_checker::{Checker, IndexInfo, ObjectFlags, Signature, Type, TypeFlags};
use tsrs_compiler::{CheckerHandle, Program};
use tsrs_core::P;
use tsrs_core::context::{CheckerLifetime, with_checker_lifetime};
use tsrs_core::json::Value;
use tsrs_core::tspath::Path;
use tsrs_project::ID;

use super::host::{CheckerError, CheckerHost, CheckerResult, SnapshotScope};
use super::json::Obj;
use super::params::{
    DocumentIdentifier, SYMBOL_OWNER_KIND_FILE, SYMBOL_OWNER_KIND_SNAPSHOT, SymbolReference,
};
use super::symbol_index::source_file_symbol;

/// A snapshot + project without a checker (Go `getSnapshotData` + `params.Project`), used by the property
/// handlers that Go serves straight from the registries.
pub(crate) struct SnapshotCtx<'h> {
    host: &'h dyn CheckerHost,
    pub(crate) scope: SnapshotScope,
    pub(crate) project: String,
}

impl<'h> SnapshotCtx<'h> {
    pub(crate) fn new(
        host: &'h dyn CheckerHost,
        snapshot: u64,
        project: &str,
    ) -> CheckerResult<SnapshotCtx<'h>> {
        let scope = host.snapshot(snapshot)?;
        Ok(SnapshotCtx { host, scope, project: project.to_string() })
    }

    /// Go `snapshotData.newSymbolResponse(symbol, canonicalProject)`.
    pub(crate) fn symbol_response(
        &self,
        symbol: P<Symbol>,
        canonical_project: &str,
    ) -> CheckerResult<Value> {
        if symbol_owner_file(symbol).is_some() {
            return file_symbol_response(self.host, symbol);
        }
        let (id, project) = self.scope.registry.register_symbol(symbol, canonical_project)?;
        let mut reference = Obj::new();
        reference.num("kind", SYMBOL_OWNER_KIND_SNAPSHOT as f64);
        reference.num("snapshot", self.scope.handle as f64);
        reference.str_nonempty("project", &project);
        reference.num("id", id as f64);
        build_symbol_response(self.host, symbol, reference.build())
    }

    /// Go `snapshotData.newSignatureResponse(projectID, sig)` for a signature produced by `checker_id`.
    pub(crate) fn signature_response(
        &self,
        checker_id: u32,
        sig: P<Signature>,
    ) -> CheckerResult<Value> {
        let id = self.scope.registry.register_signature(&self.project, checker_id, sig)?;
        let mut o = Obj::new();
        o.num("id", id as f64);
        o.num("flags", sig.flags().bits() as f64);
        if let Some(decl) = sig.declaration() {
            o.str_nonempty("declaration", &self.host.node_handle(decl)?);
        }
        o.ids("typeParameters", sig.type_parameters().iter().map(|t| t.id().0 as f64));
        if !sig.parameters().is_empty() {
            o.set(
                "parameters",
                Value::Array(
                    sig.parameters()
                        .iter()
                        .map(|p| compact_symbol_reference(self.host, *p))
                        .collect::<CheckerResult<_>>()?,
                ),
            );
        }
        if let Some(this) = sig.this_parameter() {
            o.set("thisParameter", compact_symbol_reference(self.host, this)?);
        }
        if let Some(target) = sig.target() {
            o.nonzero("target", target.id().0 as f64);
        }
        Ok(o.build())
    }

    pub(crate) fn resolve_type(&self, id: u32, checker_id: Option<u32>) -> CheckerResult<P<Type>> {
        self.scope.registry.resolve_type(&self.project, id, checker_id)
    }

    pub(crate) fn resolve_signature(
        &self,
        id: u64,
        checker_id: Option<u32>,
    ) -> CheckerResult<P<Signature>> {
        self.scope.registry.resolve_signature(&self.project, id, checker_id)
    }

    /// The API checker that produced the project's registered handles (for responses built without one).
    pub(crate) fn registered_checker_id(&self) -> u32 {
        self.scope.registry.project_checker_id(&self.project)
    }

    pub(crate) fn program(&self) -> CheckerResult<&'static Program> {
        let project = self
            .scope
            .snapshot
            .project_collection
            .get_project(&ID(self.project.clone()))
            .ok_or_else(|| CheckerError::client(format!("project {} not found", self.project)))?;
        project.get_program().ok_or_else(|| CheckerError::client("project has no program"))
    }
}

/// Go `checkerSetup`. Field order matters: the checker handle is released before the snapshot scope.
pub(crate) struct Setup<'h> {
    checker: CheckerHandle,
    // Dropped after `checker`: the gate is released only once the checker slot is free again.
    _lease: super::lease::ApiCheckerLease,
    pub(crate) checker_id: u32,
    program: &'static Program,
    pub(crate) sd: SnapshotCtx<'h>,
}

impl<'h> Setup<'h> {
    /// Go `setupChecker`: resolves snapshot and program and acquires the program's persistent API checker
    /// (`core.CheckerLifetimeAPI`). The checker is exclusive and not reentrant: a handler must acquire it
    /// once and never call back into anything that acquires the API checker again.
    pub(crate) fn new(
        host: &'h dyn CheckerHost,
        snapshot: u64,
        project: &str,
    ) -> CheckerResult<Setup<'h>> {
        let sd = SnapshotCtx::new(host, snapshot, project)?;
        let program = sd.program()?;
        let lease = super::lease::acquire(program)?;
        let ctx = with_checker_lifetime(&host.context(), CheckerLifetime::API);
        let checker = program.get_type_checker(&ctx);
        let checker_id = checker.id;
        Ok(Setup { checker, _lease: lease, checker_id, program, sd })
    }

    pub(crate) fn c(&mut self) -> &mut Checker {
        &mut self.checker
    }

    fn symbol_response(&self, symbol: P<Symbol>) -> CheckerResult<Value> {
        self.sd.symbol_response(symbol, &self.sd.project)
    }

    pub(crate) fn opt_symbol_response(&self, symbol: Option<P<Symbol>>) -> CheckerResult<Value> {
        symbol.map_or(Ok(Value::Null), |s| self.symbol_response(s))
    }

    pub(crate) fn symbols_response(&self, symbols: &[P<Symbol>]) -> CheckerResult<Value> {
        Ok(Value::Array(
            symbols.iter().map(|s| self.symbol_response(*s)).collect::<CheckerResult<_>>()?,
        ))
    }

    pub(crate) fn signature_response(&self, sig: P<Signature>) -> CheckerResult<Value> {
        self.sd.signature_response(self.checker_id, sig)
    }

    fn register_type(&self, t: P<Type>) -> CheckerResult<u32> {
        self.sd.scope.registry.register_type(&self.sd.project, self.checker_id, t)
    }

    fn register_opt_type(&self, t: Option<P<Type>>) -> CheckerResult<u32> {
        t.map_or(Ok(0), |t| self.register_type(t))
    }

    /// Go `snapshotData.newTypeResponse(projectID, t, checker)`.
    pub(crate) fn type_response(&mut self, t: P<Type>) -> CheckerResult<Value> {
        let id = self.register_type(t)?;
        let mut o = type_response_base(t, id);
        // The port answers member queries on instantiated references through lazy member tables without
        // setting MembersResolved; report the flag where pinned Go's resolveStructuredTypeMembers would have
        // set it (only when such a query actually happened, never for unresolved types).
        if t.flags().intersects(TypeFlags::Object)
            && !t.object_flags().intersects(ObjectFlags::MembersResolved)
            && self.checker.members_resolved_like_go(t)
        {
            o.num("objectFlags", (go_object_flags(t) | ObjectFlags::MembersResolved).bits() as f64);
        }
        if let Some(symbol) = t.symbol() {
            o.set("symbol", compact_symbol_reference(self.sd.host, symbol)?);
        }
        if let Some(alias_symbol) = t.alias().and_then(|a| a.symbol()) {
            o.set("aliasSymbol", compact_symbol_reference(self.sd.host, alias_symbol)?);
        }
        if t.object_flags().intersects(ObjectFlags::Mapped) {
            let mapped = t.as_mapped_type();
            tsrs_checker::MappedType::resolve_components(&mut self.checker, t);
            let tp = self.register_opt_type(mapped.type_parameter())?;
            let ct = self.register_opt_type(mapped.constraint_type())?;
            let nt = self.register_opt_type(mapped.name_type())?;
            let tt = self.register_opt_type(mapped.template_type())?;
            o.nonzero("typeParameter", tp as f64);
            o.nonzero("constraintType", ct as f64);
            o.nonzero("nameType", nt as f64);
            o.nonzero("templateType", tt as f64);
        }
        if tsrs_checker::is_tuple_type_target(t) {
            let infos = t.as_tuple_type().element_infos();
            if infos.iter().any(|info| info.labeled_declaration().is_some()) {
                let decls = infos
                    .iter()
                    .map(|info| {
                        info.labeled_declaration()
                            .map_or(Ok(String::new()), |d| self.sd.host.node_handle(d))
                            .map(Value::String)
                    })
                    .collect::<CheckerResult<_>>()?;
                o.set("labeledElementDeclarations", Value::Array(decls));
            }
        }
        Ok(o.build_ordered(TYPE_RESPONSE_FIELD_ORDER))
    }

    pub(crate) fn opt_type_response(&mut self, t: Option<P<Type>>) -> CheckerResult<Value> {
        t.map_or(Ok(Value::Null), |t| self.type_response(t))
    }

    pub(crate) fn types_response(&mut self, types: &[P<Type>]) -> CheckerResult<Value> {
        let mut out = Vec::with_capacity(types.len());
        out = types.iter().try_fold(out, |mut out, t| {
            out.push(self.type_response(*t)?);

            Ok::<_, CheckerError>(out)
        })?;
        Ok(Value::Array(out))
    }

    /// Go `checkerSetup.newIndexInfoResponse`.
    pub(crate) fn index_info_response(&mut self, info: P<IndexInfo>) -> CheckerResult<Value> {
        let mut o = Obj::new();
        o.set("keyType", self.type_response(info.key_type())?);
        o.set("valueType", self.type_response(info.value_type())?);
        o.set("isReadonly", Value::Bool(info.is_readonly()));
        if let Some(decl) = info.declaration() {
            o.str_nonempty("declaration", &self.sd.host.node_handle(decl)?);
        }
        Ok(o.build())
    }

    pub(crate) fn resolve_type(&self, id: u32) -> CheckerResult<P<Type>> {
        self.sd.resolve_type(id, Some(self.checker_id))
    }

    pub(crate) fn resolve_signature(&self, id: u64) -> CheckerResult<P<Signature>> {
        self.sd.resolve_signature(id, Some(self.checker_id))
    }

    /// Go `checkerSetup.resolveSymbolHandle`.
    pub(crate) fn resolve_symbol(&self, r: &SymbolReference) -> CheckerResult<P<Symbol>> {
        resolve_symbol_for_program(&self.sd, self.program, r)
    }

    pub(crate) fn source_file(&self, file: &DocumentIdentifier) -> CheckerResult<P<SourceFile>> {
        self.program.get_source_file(&file.to_file_name()).ok_or_else(|| {
            CheckerError::client(format!("source file not found: {}", file.display()))
        })
    }

    pub(crate) fn resolve_node(&self, handle: &str) -> CheckerResult<P<Node>> {
        self.sd.host.resolve_node_handle(self.program, handle)
    }

    /// Go `checkerSetup.resolveLocation`: a node handle, or file + UTF-16 position, or nothing.
    pub(crate) fn resolve_location(
        &self,
        handle: &str,
        file: Option<&DocumentIdentifier>,
        position: Option<u32>,
    ) -> CheckerResult<Option<P<Node>>> {
        if !handle.is_empty() {
            return self.resolve_node(handle).map(Some);
        }
        if let (Some(file), Some(position)) = (file, position) {
            let source_file = self.source_file(file)?;
            return Ok(Some(touching_property_name_at_utf16(source_file, position)));
        }
        Ok(None)
    }
}

/// `astnav.GetTouchingPropertyName(file, positionMap.UTF16ToUTF8(position))`.
pub(crate) fn touching_property_name_at_utf16(
    source_file: P<SourceFile>,
    position: u32,
) -> P<Node> {
    let utf8 = source_file.get_position_map().utf16_to_utf8(position);
    tsrs_astnav::get_touching_property_name(source_file, utf8)
}

/// Go `symbolOwnerFile`.
pub(crate) fn symbol_owner_file(symbol: P<Symbol>) -> Option<P<SourceFile>> {
    if symbol.flags().intersects(SymbolFlags::Transient) {
        return None;
    }
    let file = tsrs_ast::get_source_file_of_symbol(symbol)?;
    if file.is_content_mapped() {
        return None;
    }
    Some(file)
}

/// Go `newFileSymbolResponse`.
pub fn file_symbol_response(host: &dyn CheckerHost, symbol: P<Symbol>) -> CheckerResult<Value> {
    let file = symbol_owner_file(symbol).expect("Expected a file-owned symbol");
    let mut reference = Obj::new();
    reference.num("kind", SYMBOL_OWNER_KIND_FILE as f64);
    reference.set("file", host.source_file_descriptor(file)?);
    reference.num("id", tsrs_ast::get_symbol_id(symbol).0 as f64);
    build_symbol_response(host, symbol, reference.build())
}

/// Go `buildSymbolResponse`.
fn build_symbol_response(
    host: &dyn CheckerHost,
    symbol: P<Symbol>,
    reference: Value,
) -> CheckerResult<Value> {
    let mut o = Obj::new();
    o.set("reference", reference);
    o.set("name", Value::String(tsrs_ast::escape_symbol_name(symbol.name())));
    o.num("flags", symbol.flags().bits() as f64);
    o.num("checkFlags", symbol.check_flags().bits() as f64);
    let decls = symbol.declarations();
    if !decls.is_empty() {
        o.set(
            "declarations",
            Value::Array(
                decls
                    .iter()
                    .map(|d| host.node_handle(*d).map(Value::String))
                    .collect::<CheckerResult<_>>()?,
            ),
        );
    }
    if let Some(vd) = symbol.value_declaration() {
        o.str_nonempty("valueDeclaration", &host.node_handle(vd)?);
    }
    if let Some(parent) = symbol.parent() {
        o.set("parent", compact_symbol_reference(host, parent)?);
    }
    if let Some(export_symbol) = symbol.export_symbol() {
        o.set("exportSymbol", compact_symbol_reference(host, export_symbol)?);
    }
    Ok(o.build())
}

/// Go `newSymbolReference` (`CompactSymbolReference`).
fn compact_symbol_reference(host: &dyn CheckerHost, symbol: P<Symbol>) -> CheckerResult<Value> {
    let mut o = Obj::new();
    o.num("id", tsrs_ast::get_symbol_id(symbol).0 as f64);
    if let Some(file) = symbol_owner_file(symbol) {
        o.set("file", Value::String(host.source_file_node_id(file)?.to_string()));
    }
    Ok(o.build())
}

/// Go package-level `newTypeResponse(t, id)` (everything except symbol/alias symbol/mapped/tuple labels).
fn type_response_base(t: P<Type>, id: u32) -> Obj {
    let mut o = Obj::new();
    o.num("id", id as f64);
    let flags = t.flags();
    o.num("flags", flags.bits() as f64);
    let mut value = Value::Null;
    let mut object_flags = 0u32;
    let mut is_tuple = false;
    let mut target = 0u32;
    let mut late = Obj::new();
    if flags.intersects(TypeFlags::Freshable) {
        let lit = t.as_literal_type();
        if flags.intersects(TypeFlags::Literal) {
            value = super::json::literal_value_to_json(lit.value());
        }
        late.nonzero("freshType", lit.fresh_type().map_or(0.0, |f| f.id().0 as f64));
        late.nonzero("regularType", lit.regular_type().map_or(0.0, |r| r.id().0 as f64));
    } else if flags.intersects(TypeFlags::Object) {
        object_flags = go_object_flags(t).bits();
        is_tuple = tsrs_checker::is_tuple_type_exported(t);
        let of = t.object_flags();
        if of.intersects(ObjectFlags::Reference) {
            if tsrs_checker::is_tuple_type_target(t) {
                let tuple = t.as_tuple_type();
                // `elementFlags` is `omitempty`: an empty tuple target omits it (fixedLength/readonly are pointers).
                let element_flags = tuple.element_flags();
                if !element_flags.is_empty() {
                    late.set(
                        "elementFlags",
                        Value::Array(
                            element_flags.iter().map(|f| Value::Number(f.bits() as f64)).collect(),
                        ),
                    );
                }
                late.num("fixedLength", tuple.fixed_length() as f64);
                late.set("readonly", Value::Bool(tuple.is_readonly()));
            }
            target = t.target().map_or(0, |x| x.id().0);
        }
        if of.intersects(ObjectFlags::ClassOrInterface) {
            let iface = t.as_interface_type();
            late.ids("typeParameters", iface.type_parameters().iter().map(|x| x.id().0 as f64));
            late.ids(
                "outerTypeParameters",
                iface.outer_type_parameters().iter().map(|x| x.id().0 as f64),
            );
            late.ids(
                "localTypeParameters",
                iface.local_type_parameters().iter().map(|x| x.id().0 as f64),
            );
            late.nonzero("thisType", iface.this_type().map_or(0.0, |x| x.id().0 as f64));
        }
    } else if flags.intersects(TypeFlags::UnionOrIntersection) {
        // types omitted; fetched via separate request
    } else if flags.intersects(TypeFlags::Index) {
        target = t.as_index_type().target().map_or(0, |x| x.id().0);
    } else if flags.intersects(TypeFlags::IndexedAccess) {
        let data = t.as_indexed_access_type();
        late.nonzero("objectType", data.object_type().map_or(0.0, |x| x.id().0 as f64));
        late.nonzero("indexType", data.index_type().map_or(0.0, |x| x.id().0 as f64));
    } else if flags.intersects(TypeFlags::Conditional) {
        let data = t.as_conditional_type();
        late.nonzero("checkType", data.check_type().map_or(0.0, |x| x.id().0 as f64));
        late.nonzero("extendsType", data.extends_type().map_or(0.0, |x| x.id().0 as f64));
    } else if flags.intersects(TypeFlags::Substitution) {
        let data = t.as_substitution_type();
        late.nonzero("baseType", data.base_type().map_or(0.0, |x| x.id().0 as f64));
        late.nonzero("substConstraint", data.subst_constraint().map_or(0.0, |x| x.id().0 as f64));
    } else if flags.intersects(TypeFlags::TemplateLiteral) {
        let texts = t.as_template_literal_type().texts();
        if !texts.is_empty() {
            late.set(
                "texts",
                Value::Array(texts.iter().map(|s| Value::String(s.to_string())).collect()),
            );
        }
    } else if flags.intersects(TypeFlags::StringMapping) {
        target = t.as_string_mapping_type().target().map_or(0, |x| x.id().0);
    } else if flags.intersects(TypeFlags::TypeParameter) {
        late.set("isThisType", Value::Bool(t.as_type_parameter().is_this_type()));
    } else if flags.intersects(TypeFlags::Intrinsic) {
        late.str_nonempty("intrinsicName", t.as_intrinsic_type().intrinsic_name());
    }
    // encoding/json v2 `omitempty` only drops empty JSON values (null, "", [], {}): uint32 0 and false
    // are emitted (pinned Go always sends objectFlags, isTupleType and isThisType).
    o.num("objectFlags", object_flags as f64);
    o.set("isTupleType", Value::Bool(is_tuple));
    o.set("value", value);
    o.nonzero("target", target as f64);
    if let Some(alias) = t.alias() {
        late.ids("aliasTypeArguments", alias.type_arguments().iter().map(|x| x.id().0 as f64));
    }
    o.extend(late);
    if !flags.intersects(TypeFlags::TypeParameter) {
        o.set("isThisType", Value::Bool(false));
    }
    o
}

/// JSON field order of Go `TypeResponse` (encoding/json emits struct fields in declaration order).
const TYPE_RESPONSE_FIELD_ORDER: &[&str] = &[
    "id",
    "flags",
    "objectFlags",
    "isTupleType",
    "value",
    "target",
    "typeParameters",
    "outerTypeParameters",
    "localTypeParameters",
    "elementFlags",
    "fixedLength",
    "readonly",
    "labeledElementDeclarations",
    "objectType",
    "indexType",
    "checkType",
    "extendsType",
    "baseType",
    "substConstraint",
    "typeParameter",
    "constraintType",
    "nameType",
    "templateType",
    "texts",
    "freshType",
    "regularType",
    "isThisType",
    "thisType",
    "intrinsicName",
    "aliasTypeArguments",
    "aliasSymbol",
    "symbol",
];

/// Go `checkerSetup{sd, snapshot, program, projectID}.resolveSymbolHandle(ref)`: snapshot-owned references
/// must name this snapshot; file-owned references must name a file of `program` with an identical descriptor.
pub(crate) fn resolve_symbol_for_program(
    sd: &SnapshotCtx,
    program: &'static Program,
    r: &SymbolReference,
) -> CheckerResult<P<Symbol>> {
    match r.kind {
        SYMBOL_OWNER_KIND_SNAPSHOT => {
            if r.snapshot != sd.scope.handle || r.file.is_some() {
                return Err(CheckerError::client(
                    "snapshot symbol reference does not match the requested checker",
                ));
            }
            sd.scope.registry.resolve_symbol(r.id)
        }
        SYMBOL_OWNER_KIND_FILE => {
            if r.file.is_none() || r.snapshot != 0 || !r.project.is_empty() {
                return Err(CheckerError::client("invalid file symbol reference"));
            }
            let path = r.file_path().unwrap_or_default();
            let source_file = program.get_source_file_by_path(&Path::new(path));
            let matches = match source_file {
                Some(f) => match &r.file {
                    Some(descriptor) => sd.host.source_file_descriptor_matches(f, descriptor)?,
                    None => false,
                },
                None => false,
            };
            if !matches {
                return Err(CheckerError::client(
                    "source file is not part of the requested program",
                ));
            }
            source_file_symbol(source_file.unwrap(), r.id).ok_or_else(|| {
                CheckerError::client(format!("symbol handle {} not found in source file", r.id))
            })
        }
        kind => Err(CheckerError::client(format!("invalid symbol reference kind {kind}"))),
    }
}

// Go's ObjectFlags end at 1 << 29 (checker/types.go); tsrs keeps its isWeakType and shouldNormalizeIntersection memos
// in bits 30 and 31 (tsrs_checker types.rs). proto.go:1261 sends `uint32(t.ObjectFlags())`, which never has them.
fn go_object_flags(t: P<Type>) -> ObjectFlags {
    t.object_flags() - ObjectFlags::WeakTypeMemo
}
