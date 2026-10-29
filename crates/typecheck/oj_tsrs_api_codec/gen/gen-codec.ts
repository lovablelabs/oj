// Rust API codec generator: reads ts-ref/tools/scripts/tsc/ast.json through tools/gen-ast/schema.ts (the
// verbatim copy of the pinned TypeScript generator's schema.ts) and writes
// crates/tsrs_api_codec/src/generated.rs, the Rust counterpart of the pinned
// tsc/internal/api/encoder/{encoder,decoder}_generated.go produced by
// tools/scripts/tsc/generate-encoder.ts.
//
// Usage: node crates/tsrs_api_codec/gen/gen-codec.ts [--check]
//
// The classification functions below (classifyDataType, needsHandWrittenCommonData, getAutoEncodedLayout,
// unionBitWidth, childType, ...) are ports of the pinned generate-encoder.ts functions of the same name; keep
// them in sync with the pinned source when the TypeScript commit moves. Only the emitted language differs.

import { execFileSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import type { KindType, MemberInfo, NodeType, Type } from "../../../tools/gen-ast/schema.ts";
import { api } from "../../../tools/gen-ast/schema.ts";

const crateRoot = path.resolve(import.meta.dirname, "..");
const outPath = path.join(crateRoot, "src/generated.rs");
const coveragePath = path.join(crateRoot, "COVERAGE.generated.md");

// ── naming (same rules as tools/gen-ast/gen-ast.ts) ───────────────────────────

const RUST_KEYWORDS = new Set(["type", "in", "as", "ref", "mod", "match", "loop", "override", "static", "const", "struct", "enum", "impl", "fn", "let", "yield", "abstract", "final", "use", "where", "true", "false", "self", "super", "crate", "move", "return", "if", "else", "for", "while", "break", "continue", "pub", "trait", "unsafe", "extern", "dyn", "async", "await", "box", "do", "macro", "priv", "typeof", "unsized", "virtual", "try"]);

function snake(s: string): string {
    s = s.replace(/JSDoc/g, "Jsdoc");
    return s.replace(/([a-z0-9])([A-Z])/g, "$1_$2").replace(/([A-Z]+)([A-Z][a-z])/g, "$1_$2").toLowerCase();
}

function ident(s: string): string {
    const n = snake(s);
    return RUST_KEYWORDS.has(n) ? n + "_" : n;
}

// ── classification (ported from pinned generate-encoder.ts) ───────────────────

function schemaMembers(node: NodeType): MemberInfo[] {
    return node.members.filter(m => !m.noTS && !m.noGo);
}

function isEncodedChild(m: MemberInfo): boolean {
    return m.isChild() && !m.noTS && !m.noGo;
}

type ChildType = "node" | "nodeList" | "rawNodeList" | "modifierList";

function childType(m: MemberInfo): ChildType {
    if (m.listKind === "ModifierList") return "modifierList";
    if (m.listKind === "NodeList") return "nodeList";
    if (m.type.baseKind() === "list" && m.listKind === "raw") return "rawNodeList";
    return "node";
}

function isNodeFlagsMember(m: MemberInfo): boolean {
    const dt = m.declaredType;
    return (dt.kind === "primitive" && dt.name === "NodeFlags") ||
        (!!m.bitmask && m.bitmask.startsWith("NodeFlags"));
}

function resolveUnion(dt: Type): Type | undefined {
    if (dt.kind === "union") return dt;
    if (dt.kind === "alias") return resolveUnion(dt.resolved);
    return undefined;
}

function isSyntaxKindUnion(m: MemberInfo): boolean {
    const dt = resolveUnion(m.declaredType);
    if (!dt || dt.kind !== "union") return false;
    return dt.types.every(t => t.kind === "kind");
}

function unionKindValues(dt: Type): KindType[] {
    const resolved = resolveUnion(dt);
    if (!resolved || resolved.kind !== "union") return [];
    return resolved.types.filter((t): t is KindType => t.kind === "kind");
}

function unionBitWidth(m: MemberInfo): number {
    const dt = resolveUnion(m.declaredType);
    if (!dt || dt.kind !== "union") return 0;
    const count = m.optional ? dt.types.length + 1 : dt.types.length;
    return Math.ceil(Math.log2(count));
}

function needsHandWrittenCommonData(node: NodeType): boolean {
    if (node.handWritten) return false;
    return schemaMembers(node).some(m => {
        if (m.isChild() || m.isKindParam()) return false;
        const dt = m.declaredType;
        if (dt.kind === "primitive" && dt.name === "bool") return false;
        if (dt.kind === "primitive" && dt.name === "string") return false;
        if (isSyntaxKindUnion(m)) return false;
        if (isNodeFlagsMember(m)) return false;
        return true;
    });
}

function getHandWrittenDataMembers(node: NodeType): MemberInfo[] {
    return schemaMembers(node).filter(m => {
        if (m.isChild() || m.isKindParam()) return false;
        const dt = m.declaredType;
        if (dt.kind === "primitive" && dt.name === "string") return false;
        if (isNodeFlagsMember(m)) return false;
        return true;
    });
}

function getAutoBoolBits(node: NodeType): MemberInfo[] {
    if (needsHandWrittenCommonData(node)) return [];
    return schemaMembers(node).filter(m => {
        if (m.isChild() || m.isKindParam()) return false;
        const dt = m.declaredType;
        return dt.kind === "primitive" && dt.name === "bool";
    });
}

function getAutoUnionBits(node: NodeType): MemberInfo[] {
    if (needsHandWrittenCommonData(node)) return [];
    return schemaMembers(node).filter(m => !m.isChild() && !m.isKindParam() && isSyntaxKindUnion(m));
}

type DataType = "string" | "children" | "extended";

function classifyDataType(node: NodeType): DataType {
    if (node.handWritten) return "extended";
    const stringMembers = schemaMembers(node).filter(m =>
        !m.isChild() && !m.isKindParam() &&
        m.declaredType.kind === "primitive" && m.declaredType.name === "string"
    );
    if (stringMembers.length > 1) return "extended";
    if (stringMembers.length === 1) {
        if (needsHandWrittenCommonData(node)) return "extended";
        return "string";
    }
    return "children";
}

interface Info {
    node: NodeType;
    dataType: DataType;
    childProps: MemberInfo[];
    autoBoolBits: MemberInfo[];
    autoUnionBits: MemberInfo[];
    handWrittenCommonData: boolean;
    textMember?: MemberInfo;
    kindMember?: MemberInfo;
    factoryMembers: MemberInfo[];
}

function analyzeNode(node: NodeType): Info {
    const dataType = classifyDataType(node);
    const members = schemaMembers(node);
    return {
        node,
        dataType,
        childProps: members.filter(isEncodedChild),
        autoBoolBits: getAutoBoolBits(node),
        autoUnionBits: getAutoUnionBits(node),
        handWrittenCommonData: needsHandWrittenCommonData(node),
        textMember: dataType === "string"
            ? members.find(m => !m.isChild() && !m.isKindParam() && m.declaredType.kind === "primitive" && m.declaredType.name === "string")
            : undefined,
        kindMember: members.find(m => m.isKindParam()),
        factoryMembers: node.members.filter(m => !m.noFactory),
    };
}

function getAutoEncodedLayout(info: Info): { member: MemberInfo; bitPos: number; bitWidth: number; }[] {
    const layout: { member: MemberInfo; bitPos: number; bitWidth: number; }[] = [];
    let bitPos = 0;
    for (const m of info.autoBoolBits) {
        layout.push({ member: m, bitPos, bitWidth: 1 });
        bitPos++;
    }
    for (const m of info.autoUnionBits) {
        const width = unionBitWidth(m);
        layout.push({ member: m, bitPos, bitWidth: width });
        bitPos += width;
    }
    if (bitPos > 6) throw new Error(`${info.node.name}: commonData layout needs ${bitPos} bits (max 6)`);
    return layout;
}

function hasAutoEncodedData(info: Info): boolean {
    return info.autoBoolBits.length > 0 || info.autoUnionBits.length > 0;
}

// ── Rust emission ─────────────────────────────────────────────────────────────

const lines: string[] = [];
function w(s = "") {
    lines.push(s);
}

const kindPat = (node: NodeType) => node.allKinds().map(k => `Kind::${k.name}`).join(" | ");
const cast = (node: NodeType) => `as_${snake(node.name)}()`;
const getter = (m: MemberInfo) => `n.${ident(m.name)}()`;

function genGetNodeDataType() {
    const stringKinds: string[] = [];
    const extendedKinds: string[] = [];
    for (const node of api.nodes()) {
        const info = analyzeNode(node);
        const kinds = node.allKinds().map(k => `Kind::${k.name}`);
        if (info.dataType === "string") stringKinds.push(...kinds);
        else if (info.dataType === "extended") extendedKinds.push(...kinds);
    }
    w("/// Go `getNodeDataType`.");
    w("pub fn node_data_type(node: P<Node>) -> u32 {");
    w("    match node.kind() {");
    w(`        ${stringKinds.join("\n        | ")} => NODE_DATA_TYPE_STRING,`);
    w(`        ${extendedKinds.join("\n        | ")} => NODE_DATA_TYPE_EXTENDED_DATA,`);
    w("        _ => NODE_DATA_TYPE_CHILDREN,");
    w("    }");
    w("}");
    w();
}

function genGetChildrenPropertyMask() {
    w("/// Go `getChildrenPropertyMask`: one bit per encoded child property, in visitor order.");
    w("pub fn children_property_mask(node: P<Node>) -> u8 {");
    w("    match node.kind() {");
    for (const node of api.nodes()) {
        const info = analyzeNode(node);
        if (info.dataType === "extended") continue;
        if (info.childProps.length === 0) continue;
        if (info.childProps.length > 8) throw new Error(`${node.name}: more than 8 child properties`);
        const parts = info.childProps.map((m, i) => {
            const ct = childType(m);
            let check: string;
            if (ct === "modifierList") check = `has_modifiers(n.modifiers())`;
            else if (ct === "rawNodeList") check = `!${getter(m)}.is_empty()`;
            else check = `present(${getter(m)})`;
            const shifted = i === 0 ? `${check} as u8` : `(${check} as u8) << ${i}`;
            return info.childProps.length === 1 ? shifted : `(${shifted})`;
        });
        w(`        ${kindPat(node)} => {`);
        w(`            let n = node.${cast(node)};`);
        w(`            ${parts.join("\n                | ")}`);
        w(`        }`);
    }
    w("        _ => 0,");
    w("    }");
    w("}");
    w();
}

function genGetNodeCommonData() {
    w("/// Go `getNodeCommonData`: the 6 bits at 24..30 of the node data word.");
    w("pub fn node_common_data(node: P<Node>) -> Result<u32, EncodeError> {");
    w("    Ok(match node.kind() {");
    for (const node of api.nodes()) {
        const info = analyzeNode(node);
        if (info.dataType === "extended") continue;
        if (info.handWrittenCommonData) {
            w(`        ${kindPat(node)} => node_common_data_${snake(node.name)}(node)?,`);
        }
        else if (hasAutoEncodedData(info)) {
            const parts: string[] = [];
            const pre: string[] = [];
            const layout = getAutoEncodedLayout(info);
            for (const { member: m, bitPos } of layout) {
                if (!isSyntaxKindUnion(m)) {
                    const shifted = `(${getter(m)} as u32) << ${24 + bitPos}`;
                    parts.push(layout.length === 1 ? shifted : `(${shifted})`);
                }
                else {
                    const kinds = unionKindValues(m.declaredType);
                    const v = `${ident(m.name)}_idx`;
                    const arms: string[] = [];
                    if (m.optional) kinds.forEach((k, i) => arms.push(`Kind::${k.name} => ${i + 1}`));
                    else kinds.forEach((k, i) => i > 0 && arms.push(`Kind::${k.name} => ${i}`));
                    pre.push(`let ${v}: u32 = match ${getter(m)} { ${arms.join(", ")}, _ => 0 };`);
                    const shifted = `${v} << ${24 + bitPos}`;
                    parts.push(layout.length === 1 ? shifted : `(${shifted})`);
                }
            }
            w(`        ${kindPat(node)} => {`);
            w(`            let n = node.${cast(node)};`);
            for (const p of pre) w(`            ${p}`);
            w(`            ${parts.join(" | ")}`);
            w(`        }`);
        }
    }
    w("        _ => 0,");
    w("    })");
    w("}");
    w();
}

function genRecordNodeStrings() {
    w("/// Go `recordNodeStrings`.");
    w("pub fn record_node_strings(node: P<Node>, strs: &mut StringTable) -> u32 {");
    w("    match node.kind() {");
    const patternsByAccess = new Map<string, string[]>();
    for (const node of api.nodes()) {
        const info = analyzeNode(node);
        if (info.dataType !== "string" || !info.textMember) continue;
        // Go reads a private text member through the promoted `Node.Text()` (which joins JSDoc text segments).
        const access = info.textMember.private ? `node.text()` : `node.${cast(node)}.${ident(info.textMember.name)}()`;
        const patterns = patternsByAccess.get(access) ?? [];
        patterns.push(kindPat(node));
        patternsByAccess.set(access, patterns);
    }
    for (const [access, patterns] of patternsByAccess) {
        w(`        ${patterns.join(" | ")} => strs.add(${access}, node.kind(), node.pos(), node.end()),`);
    }
    w(`        k => unreachable!("record_node_strings: {k:?} is not a string node"),`);
    w("    }");
    w("}");
    w();
}

function genRecordExtendedData() {
    w("/// Go `recordExtendedData`: dispatches to the hand-written `record_extended_data_*` functions.");
    w("pub fn record_extended_data(node: P<Node>, cx: &mut EncodeContext) -> Result<u32, EncodeError> {");
    w("    let offset = cx.extended_data.len() as u32;");
    w("    match node.kind() {");
    for (const node of api.nodes()) {
        const info = analyzeNode(node);
        if (info.dataType !== "extended") continue;
        w(`        ${kindPat(node)} => { record_extended_data_${snake(node.name)}(node, cx)?; },`);
    }
    w(`        k => unreachable!("record_extended_data: {k:?} is not an extended data node"),`);
    w("    }");
    w("    Ok(offset)");
    w("}");
    w();
}

// ── decoder ──────────────────────────────────────────────────────────────────

function commonDataDecode(node: NodeType, info: Info, out: string[]): Map<MemberInfo, string> {
    const vars = new Map<MemberInfo, string>();
    if (info.handWrittenCommonData) {
        const dataMembers = getHandWrittenDataMembers(node);
        if (dataMembers.length === 0) return vars;
        const names = dataMembers.map(m => ident(m.goParamName()));
        out.push(`let (${names.join(", ")},) = decode_node_common_data_${snake(node.name)}(common_data)?;`);
        dataMembers.forEach((m, i) => vars.set(m, names[i]));
        return vars;
    }
    for (const { member: m, bitPos, bitWidth } of getAutoEncodedLayout(info)) {
        const v = ident(m.goParamName());
        if (!isSyntaxKindUnion(m)) {
            out.push(`let ${v} = common_data & ${1 << bitPos} != 0;`);
        }
        else {
            const kinds = unionKindValues(m.declaredType);
            const mask = (1 << bitWidth) - 1;
            const idx = bitPos === 0 ? `common_data & ${mask}` : `(common_data >> ${bitPos}) & ${mask}`;
            if (m.optional) {
                out.push(`let ${v} = match ${idx} { ${kinds.map((k, i) => `${i + 1} => Kind::${k.name}`).join(", ")}, _ => Kind::Unknown };`);
            }
            else if (kinds.length === 2) {
                out.push(`let ${v} = if ${idx} != 0 { Kind::${kinds[1].name} } else { Kind::${kinds[0].name} };`);
            }
            else {
                // Go leaves the zero Kind for an out-of-range index; the decoder rejects it instead.
                out.push(`let ${v} = match ${idx} { ${kinds.map((k, i) => `${i} => Kind::${k.name}`).join(", ")}, i => return Err(DecodeError::InvalidCommonData { kind, value: i }) };`);
            }
        }
        vars.set(m, v);
    }
    return vars;
}

function factoryCall(node: NodeType, info: Info, args: string[], kindVar: string): string {
    if (node.kindAliases.length > 0 && !info.kindMember) {
        const arms = [`Kind::${node.syntaxKindName} => f.new_${snake(node.name)}(${args.join(", ")})`];
        for (const a of node.kindAliases) arms.push(`Kind::${a} => f.new_${snake(a)}(${args.join(", ")})`);
        return `match ${kindVar} { ${arms.join(", ")}, k => return Err(DecodeError::UnexpectedKind(k)) }`;
    }
    return `f.new_${snake(node.name)}(${args.join(", ")})`;
}

function argFor(m: MemberInfo, vars: Map<MemberInfo, string>): string {
    if (m.isKindParam()) return "kind";
    const v = vars.get(m);
    if (v) return v;
    return "absent()?";
}

function genCreateStringNode() {
    w("impl Decoder<'_> {");
    w("    /// Go `createStringNode`.");
    w("    pub(crate) fn create_string_node(&self, kind: Kind, data: u32, common_data: u32) -> Result<P<Node>, DecodeError> {");
    w("        let text = self.get_string(data & NODE_DATA_STRING_INDEX_MASK)?;");
    w("        let f = &self.factory;");
    w("        let _ = common_data;");
    w("        Ok(match kind {");
    for (const node of api.nodes()) {
        const info = analyzeNode(node);
        if (info.dataType !== "string") continue;
        const pre: string[] = [];
        const vars = commonDataDecode(node, info, pre);
        const args = info.factoryMembers.map(m => {
            if (m === info.textMember) return m.listKind === "raw" ? "alloc_slice(&[text])" : "text";
            if (isEncodedChild(m)) return "absent()?"; // Go passes nil for the children of string nodes
            return argFor(m, vars);
        });
        w(`            ${kindPat(node)} => {`);
        for (const p of pre) w(`                ${p}`);
        w(`                ${factoryCall(node, info, args, "kind")}`);
        w(`            }`);
    }
    w("            k => return Err(DecodeError::UnexpectedKind(k)),");
    w("        })");
    w("    }");
    w();
}

function genCreateExtendedNode() {
    w("    /// Go `createExtendedNode`: dispatches to the hand-written `decode_extended_data_*` functions.");
    w("    pub(crate) fn create_extended_node(&self, kind: Kind, data: u32, child_indices: &[usize], common_data: u32) -> Result<P<Node>, DecodeError> {");
    w("        match kind {");
    for (const node of api.nodes()) {
        const info = analyzeNode(node);
        if (info.dataType !== "extended") continue;
        w(`            ${kindPat(node)} => self.decode_extended_data_${snake(node.name)}(data, child_indices, common_data),`);
    }
    w("            k => Err(DecodeError::UnexpectedKind(k)),");
    w("        }");
    w("    }");
    w();
}

function genCreateChildrenNode() {
    const kindOwner = new Map<string, { node: NodeType; total: number; }>();
    for (const node of api.nodes()) {
        const info = analyzeNode(node);
        if (info.dataType !== "children") continue;
        const kinds = node.allKinds().map(k => k.name);
        for (const k of kinds) {
            const existing = kindOwner.get(k);
            if (!existing || kinds.length < existing.total) kindOwner.set(k, { node, total: kinds.length });
        }
    }
    const nodeKinds = new Map<string, string[]>();
    for (const [k, owner] of kindOwner) {
        if (!nodeKinds.has(owner.node.name)) nodeKinds.set(owner.node.name, []);
        nodeKinds.get(owner.node.name)!.push(k);
    }

    w("    /// Go `createChildrenNode`.");
    w("    pub(crate) fn create_children_node(&self, kind: Kind, data: u32, child_indices: &[usize], common_data: u32) -> Result<P<Node>, DecodeError> {");
    w("        let mask = (data & NODE_DATA_CHILD_MASK) as u8;");
    w("        let _ = (mask, common_data);");
    w("        let f = self.factory.clone();");
    w("        let f = &f;");
    w("        Ok(match kind {");
    for (const node of api.nodes()) {
        const info = analyzeNode(node);
        if (info.dataType !== "children") continue;
        const kinds = nodeKinds.get(node.name);
        if (!kinds || kinds.length === 0) continue;
        const pre: string[] = [];
        const vars = commonDataDecode(node, info, pre);
        const childVars = new Map<MemberInfo, string>();
        const hasCommonData = info.autoBoolBits.length > 0 || info.handWrittenCommonData;
        if (info.childProps.length === 1) {
            const cp = info.childProps[0];
            const ct = childType(cp);
            const v = ident(cp.goParamName());
            // The pinned Go decoder fills `d.allocNodeSlice(n)` (a zero-length slice) by index, so it panics with
            // "index out of range [0] with length 0" for any non-empty raw list; reproduce that failure.
            if (ct === "rawNodeList") pre.push(`if !child_indices.is_empty() { return Err(DecodeError::GoDecoderPanic("runtime error: index out of range [0] with length 0".to_string())); }`, `let ${v}: &'static [P<Node>] = &[];`);
            else if (ct === "nodeList") pre.push(hasCommonData ? `let ${v} = from_list(self.list_at(child_indices.first().copied().unwrap_or(0)))?;` : `let ${v} = from_list(self.single_node_list_child(child_indices))?;`);
            else if (ct === "modifierList") pre.push(`let ${v} = self.modifier_list_at(child_indices.first().copied().unwrap_or(0));`);
            else pre.push(`let ${v} = from_node(self.single_child(child_indices))?;`);
            childVars.set(cp, v);
        }
        else if (info.childProps.length > 1) {
            pre.push("let mut it = ChildIter::new(child_indices);");
            info.childProps.forEach((m, i) => {
                const ct = childType(m);
                const v = ident(m.goParamName());
                if (ct === "modifierList") pre.push(`let ${v} = self.modifier_list_at(it.next_if(mask, ${i}));`);
                else if (ct === "rawNodeList") pre.push(`let ${v} = self.list_at(it.next_if(mask, ${i})).map_or(&[][..], |l| l.nodes());`);
                else if (ct === "nodeList") pre.push(`let ${v} = from_list(self.list_at(it.next_if(mask, ${i})))?;`);
                else pre.push(`let ${v} = from_node(self.node_at_opt(it.next_if(mask, ${i})))?;`);
                childVars.set(m, v);
            });
        }
        const args = info.factoryMembers.map(m => childVars.get(m) ?? argFor(m, vars));
        w(`            ${kinds.map(k => `Kind::${k}`).join(" | ")} => {`);
        for (const p of pre) w(`                ${p}`);
        w(`                ${factoryCall(node, info, args, "kind")}`);
        w(`            }`);
    }
    w("            k => return Err(DecodeError::UnexpectedKind(k)),");
    w("        })");
    w("    }");
    w("}");
    w();
}

// ── coverage ─────────────────────────────────────────────────────────────────

function genCoverage(): string {
    const out: string[] = [];
    out.push("<!-- Code generated by crates/tsrs_api_codec/gen/gen-codec.ts from ts-ref/tools/scripts/tsc/ast.json. DO NOT EDIT. -->");
    out.push("");
    out.push("# Node kind coverage (generated)");
    out.push("");
    out.push("Every node struct in the pinned `ast.json`, the binary data type the pinned encoder assigns it, its encoded");
    out.push("child properties (bit order of the children mask) and its commonData members. The Rust encoder and decoder");
    out.push("arms are generated from exactly this table.");
    out.push("");
    out.push("| Node struct | Kinds | Data type | Children (mask bit order) | commonData |");
    out.push("| --- | --- | --- | --- | --- |");
    let kinds = 0;
    for (const node of api.nodes()) {
        const info = analyzeNode(node);
        kinds += node.allKinds().length;
        const cd = info.handWrittenCommonData
            ? `hand-written (${getHandWrittenDataMembers(node).map(m => m.name).join(", ")})`
            : getAutoEncodedLayout(info).map(l => `${l.member.name}@${l.bitPos}/${l.bitWidth}`).join(", ");
        out.push(`| ${node.name} | ${node.allKinds().map(k => k.name).join(", ")} | ${info.dataType} | ${info.childProps.map(m => `${m.name}:${childType(m)}`).join(", ")} | ${cd} |`);
    }
    out.push("");
    out.push(`${api.nodes().length} node structs, ${kinds} kinds.`);
    out.push("");
    return out.join("\n");
}

// ── main ─────────────────────────────────────────────────────────────────────

w("// Code generated by crates/tsrs_api_codec/gen/gen-codec.ts from ts-ref/tools/scripts/tsc/ast.json. DO NOT EDIT.");
w("//");
w("// Rust counterpart of the pinned tsc/internal/api/encoder/{encoder,decoder}_generated.go.");
w();
w("use tsrs_ast::*;");
w("use tsrs_core::{alloc_slice, P};");
w();
w("use crate::decoder::*;");
w("use crate::encoder::*;");
w("use crate::format::*;");
w("use crate::stringtable::StringTable;");
w();
genGetNodeDataType();
genGetChildrenPropertyMask();
genGetNodeCommonData();
genRecordNodeStrings();
genRecordExtendedData();
genCreateStringNode();
genCreateExtendedNode();
genCreateChildrenNode();

// Compare the same formatting that cargo fmt applies to the checked-in file.
const generated = execFileSync("rustfmt", [
    "--edition", "2024",
    "--config-path", path.resolve(crateRoot, "../..", ".rustfmt.toml"),
], {
    input: lines.join("\n"),
    encoding: "utf8",
    cwd: path.resolve(crateRoot, "../.."),
});
const coverage = genCoverage();
if (process.argv.includes("--check")) {
    let ok = true;
    for (const [p, c] of [[outPath, generated], [coveragePath, coverage]] as const) {
        const existing = fs.existsSync(p) ? fs.readFileSync(p, "utf8") : "";
        if (existing !== c) {
            console.error(`stale: ${path.relative(crateRoot, p)} (run node crates/tsrs_api_codec/gen/gen-codec.ts)`);
            ok = false;
        }
    }
    process.exit(ok ? 0 : 1);
}
fs.writeFileSync(outPath, generated);
fs.writeFileSync(coveragePath, coverage);
console.log(`Wrote ${outPath}`);
console.log(`Wrote ${coveragePath}`);
