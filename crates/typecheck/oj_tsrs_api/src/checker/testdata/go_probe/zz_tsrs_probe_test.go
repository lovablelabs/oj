package api

// tsrs checker-lane differential probe. Copied temporarily into the pinned reference checkout
// (ts-ref/tsc/internal/api) by regen.sh, which removes it again afterwards. Drives the pinned Go API
// session over its JSON request surface with the fixture in ./fixture and the needles in needles.txt,
// and writes go_probe_b85298b6.jsonl. The request sequence mirrors probe_lines() in
// crates/tsrs_api/tests/integration/checker_test.rs; keep both in sync.

import (
	"context"
	"fmt"
	"os"
	"strings"
	"testing"
	"unicode/utf16"

	"github.com/microsoft/TypeScript/tsc/internal/ast"
	"github.com/microsoft/TypeScript/tsc/internal/astnav"
	"github.com/microsoft/TypeScript/tsc/internal/json"
	"github.com/microsoft/TypeScript/tsc/internal/project"
	"github.com/microsoft/TypeScript/tsc/internal/testutil/projecttestutil"
)

type probe struct {
	s    *Session
	snap float64
	proj string
	out  *os.File
}

func (p *probe) call(method string, params map[string]any) (result any, errText string) {
	// Like the ipc connections (conn_sync.go / conn_async.go): a handler panic becomes "panic: <value>\n<stack>".
	defer func() {
		if r := recover(); r != nil {
			result, errText = nil, fmt.Sprintf("panic: %v\n<stack>", r)
		}
	}()
	b, _ := json.Marshal(params)
	res, err := p.s.HandleRequest(context.Background(), method, json.Value(b))
	if err != nil {
		return nil, err.Error()
	}
	out, _ := json.Marshal(res)
	var v any
	_ = json.Unmarshal(out, &v)
	return v, ""
}

// callRaw sends a raw JSON params payload (e.g. with explicit nulls) through HandleRequest.
func (p *probe) callRaw(method string, raw string) (result any, errText string) {
	defer func() {
		if r := recover(); r != nil {
			result, errText = nil, fmt.Sprintf("panic: %v\n<stack>", r)
		}
	}()
	res, err := p.s.HandleRequest(context.Background(), method, json.Value(raw))
	if err != nil {
		return nil, err.Error()
	}
	out, _ := json.Marshal(res)
	var v any
	_ = json.Unmarshal(out, &v)
	return v, ""
}

func (p *probe) sp(kv ...any) map[string]any {
	m := map[string]any{"snapshot": p.snap, "project": p.proj}
	for i := 0; i < len(kv); i += 2 {
		m[kv[i].(string)] = kv[i+1]
	}
	return m
}

func (p *probe) emit(label string, v any) {
	b, _ := json.Marshal(map[string]any{"q": label, "r": v}, json.Deterministic(true))
	fmt.Fprintln(p.out, string(b))
}

func u16(text, needle string) int {
	i := strings.Index(text, needle)
	if i < 0 {
		panic(needle)
	}
	return len(utf16.Encode([]rune(text[:i])))
}

func get(v any, k string) any {
	if m, ok := v.(map[string]any); ok {
		return m[k]
	}
	return nil
}

func TestTsrsCheckerProbe(t *testing.T) {
	dir := os.Getenv("TSRS_FX")
	if dir == "" {
		t.Skip("TSRS_FX not set")
	}
	read := func(n string) string { b, _ := os.ReadFile(dir + "/" + n); return string(b) }
	main := read("main.ts")
	files := map[string]any{
		"/p/tsconfig.json": read("tsconfig.json"), "/p/main.ts": main, "/p/types.d.ts": read("types.d.ts"), "/p/other.ts": read("other.ts"),
		"/q/tsconfig.json": `{"files":["q.ts"]}`, "/q/q.ts": "export const q: number = 1;\n",
	}
	init, _ := projecttestutil.GetSessionInitOptions(files, nil, &projecttestutil.TypingsInstallerOptions{})
	s := NewStandaloneSession(init, nil)
	defer s.Close()
	out, _ := os.Create(os.Getenv("TSRS_OUT"))
	defer out.Close()
	p := &probe{s: s, out: out}
	snap, errs := p.call("createSnapshot", map[string]any{"openProjects": []string{"/p/tsconfig.json", "/q/tsconfig.json"}})
	if errs != "" {
		t.Fatal(errs)
	}
	p.snap = get(snap, "snapshot").(float64)
	for _, pr := range get(snap, "projects").([]any) {
		if strings.HasPrefix(get(pr, "configFileName").(string), "/p/") {
			p.proj = get(pr, "id").(string)
		}
	}
	p.emit("flags", map[string]any{"Value": float64(ast.SymbolFlagsValue), "Type": float64(ast.SymbolFlagsType)})
	ts := func(t any) any {
		if t == nil {
			return nil
		}
		r, e := p.call("typeToString", p.sp("type", get(t, "id")))
		if e != "" {
			return e
		}
		return r
	}
	needlesText, _ := os.ReadFile(os.Getenv("TSRS_NEEDLES_FILE"))
	for _, n := range strings.Split(string(needlesText), "\n") {
		if n == "" {
			continue
		}
		pos := u16(main, n)
		ty, e1 := p.call("getTypeAtPosition", p.sp("file", "/p/main.ts", "position", pos))
		sy, e2 := p.call("getSymbolAtPosition", p.sp("file", "/p/main.ts", "position", pos))
		p.emit("at:"+n, map[string]any{"type": ts(ty), "typeFlags": get(ty, "flags"), "objectFlags": get(ty, "objectFlags"), "sym": get(sy, "name"), "symFlags": get(sy, "flags"), "kind": get(get(sy, "reference"), "kind"), "e": e1 + e2})
	}
	symRef := func(needle string) any {
		sy, _ := p.call("getSymbolAtPosition", p.sp("file", "/p/main.ts", "position", u16(main, needle)))
		return get(sy, "reference")
	}
	declared := func(needle string) any {
		t, _ := p.call("getDeclaredTypeOfSymbol", p.sp("symbol", symRef(needle)))
		return t
	}
	// A. conditional check type constraint.
	c := declared("C<T> =")
	check, _ := p.call("getCheckTypeOfType", p.sp("objectId", get(c, "id")))
	cons, e := p.call("getConstraintOfTypeParameter", p.sp("objectId", get(check, "id")))
	// Process-wide symbol ids differ between runs: report identity relations instead.
	p.emit("C.check", map[string]any{"str": ts(check), "flags": get(check, "flags")})
	p.emit("C.constraint", map[string]any{"str": ts(cons), "flags": get(cons, "flags"), "sameAsCheck": get(cons, "id") == get(check, "id"), "sameSymbolAsCheck": get(get(cons, "symbol"), "id") == get(get(check, "symbol"), "id"), "e": e})
	if cons != nil {
		cons2, _ := p.call("getConstraintOfTypeParameter", p.sp("objectId", get(cons, "id")))
		p.emit("C.constraint.constraint", ts(cons2))
	}
	ia := declared("IA<T, K")
	k, _ := p.call("getIndexTypeOfType", p.sp("objectId", get(ia, "id")))
	kc, _ := p.call("getConstraintOfTypeParameter", p.sp("objectId", get(k, "id")))
	p.emit("IA.K.constraint", ts(kc))
	// B. snapshot-less file-owned symbol lookups.
	dog := declared("Dog extends")
	props, _ := p.call("getPropertiesOfType", p.sp("type", get(dog, "id")))
	names := func(v any) any {
		a, ok := v.([]any)
		if !ok {
			return v
		}
		var r []any
		for _, x := range a {
			r = append(r, get(x, "name"))
		}
		return r
	}
	p.emit("Dog.props", names(props))
	legs := props.([]any)[0]
	p.emit("legs.ref.kind", get(get(legs, "reference"), "kind"))
	par, e := p.call("getParentOfSymbol", map[string]any{"symbol": get(legs, "reference")})
	p.emit("legs.parent", map[string]any{"name": get(par, "name"), "kind": get(get(par, "reference"), "kind"), "e": e})
	mem, e := p.call("getMembersOfSymbol", map[string]any{"symbol": symRef("Dog extends")})
	p.emit("Dog.members", map[string]any{"names": names(mem), "e": e})
	mod, _ := p.call("getSymbolOfSourceFile", p.sp("file", "/p/main.ts"))
	exp, e := p.call("getExportsOfSymbol", map[string]any{"symbol": get(mod, "reference")})
	p.emit("main.exportsOfSymbol", map[string]any{"names": names(exp), "e": e})
	expm, e := p.call("getExportsOfModule", p.sp("symbol", get(mod, "reference")))
	p.emit("main.exportsOfModule", map[string]any{"names": names(expm), "e": e})
	scope, _ := p.call("getSymbolsInScope", p.sp("file", "/p/main.ts", "position", u16(main, "box.value"), "meaning", float64(ast.SymbolFlagsValue)))
	for _, sym := range scope.([]any) {
		if get(sym, "name") == "box" {
			es, e := p.call("getExportSymbolOfSymbol", map[string]any{"symbol": get(sym, "reference")})
			p.emit("box.local.exportSymbol", map[string]any{"name": get(es, "name"), "localFlags": get(sym, "flags"), "localKind": get(get(sym, "reference"), "kind"), "exportFlags": get(es, "flags"), "e": e})
			es2, e := p.call("getExportSymbolOfSymbolForChecker", p.sp("symbol", get(sym, "reference")))
			p.emit("box.local.exportSymbolForChecker", map[string]any{"name": get(es2, "name"), "flags": get(es2, "flags"), "e": e})
		}
	}
	// C. resolveName with and without a location.
	for _, q := range [][]any{{"Array", ast.SymbolFlagsType}, {"box", ast.SymbolFlagsValue}, {"Promise", ast.SymbolFlagsValue}, {"Animal", ast.SymbolFlagsValue}} {
		r, e := p.call("resolveName", p.sp("name", q[0], "meaning", float64(q[1].(ast.SymbolFlags))))
		p.emit("resolveName.noloc:"+q[0].(string), map[string]any{"name": get(r, "name"), "flags": get(r, "flags"), "kind": get(get(r, "reference"), "kind"), "e": e})
		r, e = p.call("resolveName", p.sp("name", q[0], "meaning", float64(q[1].(ast.SymbolFlags)), "file", "/p/main.ts", "position", u16(main, "r = over")))
		p.emit("resolveName.loc:"+q[0].(string), map[string]any{"name": get(r, "name"), "flags": get(r, "flags"), "kind": get(get(r, "reference"), "kind"), "e": e})
	}
	// D. stale / foreign handles.
	box, _ := p.call("getTypeAtPosition", p.sp("file", "/p/main.ts", "position", u16(main, "box:")))
	id := get(box, "id")
	_, e = p.call("getTypeArguments", map[string]any{"snapshot": p.snap, "project": "/other/tsconfig.json", "type": id})
	p.emit("err.unknownProject", e)
	qproj := ""
	for _, pr := range get(snap, "projects").([]any) {
		if strings.HasPrefix(get(pr, "configFileName").(string), "/q/") {
			qproj = get(pr, "id").(string)
		}
	}
	r, e := p.call("getTypeArguments", map[string]any{"snapshot": p.snap, "project": qproj, "type": id})
	p.emit("err.foreignProjectType", map[string]any{"r": r, "e": e})
	_, e = p.call("getTypeArguments", p.sp("type", 999999))
	p.emit("err.unknownType", e)
	_, e = p.call("getTypeArguments", map[string]any{"snapshot": p.snap + 1000, "project": p.proj, "type": id})
	p.emit("err.unknownSnapshot", e)
	_, e = p.call("getTypeArguments", p.sp("type", "x"))
	p.emit("err.badType", e)
	arr, _ := p.call("resolveName", p.sp("name", "Array", "meaning", float64(ast.SymbolFlagsType)))
	ref := get(arr, "reference").(map[string]any)
	forged := map[string]any{}
	for k, v := range ref {
		forged[k] = v
	}
	forged["snapshot"] = p.snap + 1
	_, e = p.call("getTypeOfSymbol", p.sp("symbol", forged))
	p.emit("err.forgedSnapshotRef", e)
	qt, e := p.call("getTypeOfSymbol", map[string]any{"snapshot": p.snap, "project": qproj, "symbol": ref})
	p.emit("crossProject.snapshotSymbol", map[string]any{"type": ts2(p, qproj, qt), "e": e})
	_, e = p.call("getParentOfSymbol", map[string]any{"symbol": map[string]any{"kind": 0, "id": 1}})
	p.emit("err.fileRefNoDescriptor", e)
	_, e = p.call("release", map[string]any{"snapshot": p.snap})
	_, e2 := p.call("getTypeArguments", p.sp("type", id))
	p.emit("err.afterRelease", map[string]any{"release": e, "e": e2})
	_, e = p.call("getParentOfSymbol", map[string]any{"symbol": get(legs, "reference")})
	p.emit("afterRelease.fileOwnedParent", e)
}

func ts2(p *probe, proj string, t any) any {
	if t == nil {
		return nil
	}
	r, e := p.call("typeToString", map[string]any{"snapshot": p.snap, "project": proj, "type": get(t, "id")})
	if e != "" {
		return e
	}
	return r
}

// TestTsrsCheckerShapes records full response objects (not just selected fields) and the outcome of
// wrong-kind requests, for comparison with tsrs through its real Session (encoding/json v2 omitempty
// and nil-slice rules, panic-vs-error behavior). Same fixture; ids are normalized by the comparison.
func TestTsrsCheckerShapes(t *testing.T) {
	dir := os.Getenv("TSRS_FX")
	outPath := os.Getenv("TSRS_SHAPES_OUT")
	if dir == "" || outPath == "" {
		t.Skip("TSRS_FX / TSRS_SHAPES_OUT not set")
	}
	read := func(n string) string { b, _ := os.ReadFile(dir + "/" + n); return string(b) }
	main := read("main.ts")
	files := map[string]any{"/p/tsconfig.json": read("tsconfig.json"), "/p/main.ts": main, "/p/types.d.ts": read("types.d.ts"), "/p/other.ts": read("other.ts")}
	init, _ := projecttestutil.GetSessionInitOptions(files, nil, &projecttestutil.TypingsInstallerOptions{})
	s := NewStandaloneSession(init, nil)
	defer s.Close()
	out, _ := os.Create(outPath)
	defer out.Close()
	p := &probe{s: s, out: out}
	snap, errs := p.call("createSnapshot", map[string]any{"openProjects": []string{"/p/tsconfig.json"}})
	if errs != "" {
		t.Fatal(errs)
	}
	p.snap = get(snap, "snapshot").(float64)
	p.proj = get(get(snap, "projects").([]any)[0], "id").(string)
	firstLine := func(e string) string { return strings.SplitN(e, "\n", 2)[0] }
	shape := func(label, method string, params map[string]any) any {
		r, e := p.call(method, params)
		if e != "" {
			p.emit("shape:"+label, map[string]any{"error": firstLine(e)})
			return nil
		}
		p.emit("shape:"+label, r)
		return r
	}
	wrong := func(label, method string, params map[string]any) {
		r, e := p.call(method, params)
		if e != "" {
			p.emit("wrong:"+label, map[string]any{"error": firstLine(e)})
		} else {
			p.emit("wrong:"+label, map[string]any{"result": r})
		}
	}
	at := func(n string) map[string]any { return p.sp("file", "/p/main.ts", "position", u16(main, n)) }
	symRef := func(needle string) any {
		sy, _ := p.call("getSymbolAtPosition", at(needle))
		return get(sy, "reference")
	}

	lit := shape("type.literal", "getTypeAtPosition", at("lit ="))
	box := shape("type.reference", "getTypeAtPosition", at("box:"))
	shape("type.union", "getTypeAtPosition", at("maybe:"))
	shape("type.bigint", "getTypeAtPosition", at("big ="))
	pair, _ := p.call("getDeclaredTypeOfSymbol", p.sp("symbol", symRef("Pair<A, B>")))
	shape("type.tupleTarget", "getTargetOfType", p.sp("objectId", get(pair, "id")))
	boxTarget := shape("type.interfaceTarget", "getTargetOfType", p.sp("objectId", get(box, "id")))
	shape("type.typeParameter", "getLocalTypeParametersOfType", p.sp("objectId", get(boxTarget, "id")))
	shape("type.thisType", "getThisTypeOfType", p.sp("objectId", get(boxTarget, "id")))
	shape("type.mapped", "getDeclaredTypeOfSymbol", p.sp("symbol", symRef("M = ")))
	shape("type.conditional", "getDeclaredTypeOfSymbol", p.sp("symbol", symRef("C<T> =")))
	shape("type.templateLiteral", "getDeclaredTypeOfSymbol", p.sp("symbol", symRef("TL = ")))
	shape("type.indexedAccess", "getDeclaredTypeOfSymbol", p.sp("symbol", symRef("IA<T, K")))
	str, _ := p.call("getStringType", p.sp())
	shape("type.intrinsic", "getStringType", p.sp())
	shape("indexInfos", "getIndexInfosOfType", p.sp("type", get(box, "id")))
	shape("indexInfo.number.none", "getIndexInfoOfType", p.sp("type", get(str, "id"), "kind", 1))
	shape("symbol.variable", "getSymbolAtPosition", at("box:"))
	shape("symbol.class", "getSymbolAtPosition", at("Dog extends"))
	shape("symbol.global", "resolveName", p.sp("name", "Array", "meaning", float64(ast.SymbolFlagsType)))
	shape("symbol.none", "resolveName", p.sp("name", "nope", "meaning", float64(ast.SymbolFlagsValue)))
	shape("members.empty", "getMembersOfSymbol", map[string]any{"symbol": symRef("box:")})
	shape("exports.empty", "getExportsOfSymbol", map[string]any{"symbol": symRef("box:")})
	shape("properties.empty", "getPropertiesOfType", p.sp("type", get(str, "id")))
	shape("typeArguments", "getTypeArguments", p.sp("type", get(box, "id")))
	shape("aliasTypeArguments.empty", "getAliasTypeArgumentsOfType", p.sp("objectId", get(box, "id")))
	over, _ := p.call("getTypeAtPosition", at("over(x: string)"))
	sigs := shape("signatures", "getSignaturesOfType", p.sp("type", get(over, "id"), "kind", 0))
	shape("signatures.construct.empty", "getSignaturesOfType", p.sp("type", get(over, "id"), "kind", 1))
	sig0 := sigs.([]any)[0]
	shape("signature.typeParameters.empty", "getTypeParametersOfSignature", p.sp("objectId", get(sig0, "id")))
	shape("signature.thisParameter.none", "getThisParameterOfSignature", p.sp("objectId", get(sig0, "id")))
	isStr, _ := p.call("getTypeAtPosition", at("isStr("))
	isStrSigs, _ := p.call("getSignaturesOfType", p.sp("type", get(isStr, "id"), "kind", 0))
	shape("typePredicate", "getTypePredicateOfSignature", p.sp("signature", get(isStrSigs.([]any)[0], "id")))
	shape("typePredicate.none", "getTypePredicateOfSignature", p.sp("signature", get(sig0, "id")))
	shape("jsDocTags", "getJsDocTags", p.sp("symbol", symRef("Animal {")))
	shape("jsDocTags.none", "getJsDocTags", p.sp("symbol", symRef("box:")))
	shape("exportsOfModule.none", "getExportsOfModule", p.sp("symbol", symRef("box:")))
	shape("baseTypes.none", "getBaseTypes", p.sp("type", get(boxTarget, "id")))
	boxSym, _ := p.call("getSymbolAtPosition", at("box:"))
	redSym, _ := p.call("getSymbolAtPosition", at("Red ="))
	shape("constantValue.none", "getConstantValue", p.sp("location", get(boxSym, "declarations").([]any)[0]))
	shape("constantValue.number", "getConstantValue", p.sp("location", get(redSym, "declarations").([]any)[0]))
	shape("completions", "getCompletionsAtPosition", p.sp("file", "/p/main.ts", "position", u16(main, "box.value")+4))
	shape("wellKnownSignatures", "getWellKnownSignatures", p.sp())

	litID, objID := get(lit, "id"), get(box, "id")
	wrong("getTypeArguments(literal)", "getTypeArguments", p.sp("type", litID))
	wrong("getBaseTypes(literal)", "getBaseTypes", p.sp("type", litID))
	wrong("getTargetOfType(literal)", "getTargetOfType", p.sp("objectId", litID))
	wrong("getFreshTypeOfType(object)", "getFreshTypeOfType", p.sp("objectId", objID))
	wrong("getRegularTypeOfType(object)", "getRegularTypeOfType", p.sp("objectId", objID))
	wrong("getTypesOfType(literal)", "getTypesOfType", p.sp("objectId", litID))
	wrong("getTypeParametersOfType(literal)", "getTypeParametersOfType", p.sp("objectId", litID))
	wrong("getOuterTypeParametersOfType(literal)", "getOuterTypeParametersOfType", p.sp("objectId", litID))
	wrong("getLocalTypeParametersOfType(literal)", "getLocalTypeParametersOfType", p.sp("objectId", litID))
	wrong("getThisTypeOfType(literal)", "getThisTypeOfType", p.sp("objectId", litID))
	wrong("getObjectTypeOfType(literal)", "getObjectTypeOfType", p.sp("objectId", litID))
	wrong("getIndexTypeOfType(literal)", "getIndexTypeOfType", p.sp("objectId", litID))
	wrong("getCheckTypeOfType(literal)", "getCheckTypeOfType", p.sp("objectId", litID))
	wrong("getExtendsTypeOfType(literal)", "getExtendsTypeOfType", p.sp("objectId", litID))
	wrong("getBaseTypeOfType(literal)", "getBaseTypeOfType", p.sp("objectId", litID))
	wrong("getConstraintOfType(literal)", "getConstraintOfType", p.sp("objectId", litID))
	wrong("getTypeParameterOfMappedType(literal)", "getTypeParameterOfMappedType", p.sp("objectId", litID))
	wrong("getConstraintTypeOfMappedType(literal)", "getConstraintTypeOfMappedType", p.sp("objectId", litID))
	wrong("getNameTypeOfMappedType(literal)", "getNameTypeOfMappedType", p.sp("objectId", litID))
	wrong("getTemplateTypeOfMappedType(literal)", "getTemplateTypeOfMappedType", p.sp("objectId", litID))
	wrong("getTrueTypeOfConditionalType(literal)", "getTrueTypeOfConditionalType", p.sp("objectId", litID))
	wrong("getFalseTypeOfConditionalType(literal)", "getFalseTypeOfConditionalType", p.sp("objectId", litID))
	wrong("getConstraintOfTypeParameter(literal)", "getConstraintOfTypeParameter", p.sp("objectId", litID))
	wrong("getDefaultFromTypeParameter(literal)", "getDefaultFromTypeParameter", p.sp("objectId", litID))
	wrong("getSignaturesOfType(kind 7)", "getSignaturesOfType", p.sp("type", objID, "kind", 7))
	wrong("getIndexInfoOfType(kind 7)", "getIndexInfoOfType", p.sp("type", objID, "kind", 7))
	wrong("signatureToSignatureDeclaration(kind 100000)", "signatureToSignatureDeclaration", p.sp("signature", get(sig0, "id"), "kind", 100000))
	wrong("getParameterType(index -1)", "getParameterType", p.sp("signature", get(sig0, "id"), "index", -1))

	// Missing / null / empty parameter values: Go decodes into zero values (DocumentIdentifier has a
	// custom decoder; *DocumentIdentifier and slices take null as nil).
	sp := fmt.Sprintf(`"snapshot":%v,"project":%q`, p.snap, p.proj)
	for _, c := range [][2]string{
		{"getSymbolAtPosition", `{` + sp + `,"position":0}`},
		{"getSymbolAtPosition", `{` + sp + `,"file":null,"position":0}`},
		{"getSymbolAtPosition", `{` + sp + `,"file":{},"position":0}`},
		{"getSymbolAtPosition", `{` + sp + `,"file":{"uri":null},"position":0}`},
		{"getSymbolAtPosition", `{` + sp + `,"file":5,"position":0}`},
		{"getSymbolAtPosition", `null`},
		{"getSymbolsOfSourceFiles", `{` + sp + `,"files":[null]}`},
		{"getSymbolsOfSourceFiles", `{` + sp + `,"files":[{}]}`},
		{"getSymbolsOfSourceFiles", `{` + sp + `,"files":null}`},
		{"resolveName", `{` + sp + `,"name":"Array","meaning":788968,"file":null,"position":0}`},
		{"resolveName", `{` + sp + `,"name":"box","meaning":111551,"file":"/p/main.ts","position":null}`},
		{"getSymbolsInScope", `{` + sp + `,"file":null,"position":0,"meaning":1}`},
		{"getSymbolsAtPositions", `{` + sp + `,"file":"/p/main.ts","positions":null}`},
		{"getTypesOfSymbols", `{` + sp + `,"symbols":[null]}`},
		{"getTypeOfSymbol", `{` + sp + `,"symbol":null}`},
		{"getSymbolAtLocation", `{` + sp + `,"location":null}`},
		{"getSymbolAtPosition", `{` + sp + `,"file":true,"position":0}`},
		{"getSymbolAtPosition", `{` + sp + `,"file":[],"position":0}`},
	} {
		r, e := p.callRaw(c[0], c[1])
		label := "params:" + c[0] + " " + strings.ReplaceAll(c[1], sp, "<sp>")
		if e != "" {
			p.emit(label, map[string]any{"error": firstLine(e)})
		} else {
			p.emit(label, map[string]any{"result": r})
		}
	}
}

// TestTsrsCheckerFlags records full type responses (including objectFlags) for the cases in flags/cases.txt
// (fixture directories flags/t and flags/m, mounted at /t and /m), each followed by getTargetOfType when the
// type has a target (the property request takes the type itself). Requests run in file order on one snapshot per fixture.
func TestTsrsCheckerFlags(t *testing.T) {
	dir := os.Getenv("TSRS_FLAGS_DIR")
	outPath := os.Getenv("TSRS_FLAGS_OUT")
	if dir == "" || outPath == "" {
		t.Skip("TSRS_FLAGS_DIR / TSRS_FLAGS_OUT not set")
	}
	casesText, _ := os.ReadFile(dir + "/cases.txt")
	read := func(n string) string { b, _ := os.ReadFile(dir + "/" + n); return string(b) }
	files := map[string]any{}
	for _, fx := range []string{"t", "m"} {
		entries, _ := os.ReadDir(dir + "/" + fx)
		for _, e := range entries {
			files["/"+fx+"/"+e.Name()] = read(fx + "/" + e.Name())
		}
	}
	init, _ := projecttestutil.GetSessionInitOptions(files, nil, &projecttestutil.TypingsInstallerOptions{})
	s := NewStandaloneSession(init, nil)
	defer s.Close()
	out, _ := os.Create(outPath)
	defer out.Close()
	p := &probe{s: s, out: out}
	snaps := map[string][2]any{}
	for _, fx := range []string{"t", "m"} {
		r, e := p.call("createSnapshot", map[string]any{"openProjects": []string{"/" + fx + "/tsconfig.json"}})
		if e != "" {
			t.Fatal(e)
		}
		snaps[fx] = [2]any{get(r, "snapshot"), get(get(r, "projects").([]any)[0], "id")}
	}
	for _, line := range strings.Split(strings.TrimSpace(string(casesText)), "\n") {
		f := strings.Split(line, "\t")
		fx, file, needle, kind, method := f[0], f[1], f[2], f[3], f[4]
		snap, proj := snaps[fx][0], snaps[fx][1]
		sd, _ := s.getSnapshotData(SnapshotID(snap.(float64)))
		program, _ := sd.getProgram(project.ID(proj.(string)))
		sf := program.GetSourceFile("/" + fx + "/" + file)
		text := sf.Text()
		pos := strings.Index(text, needle) + len(needle)
		if kind != "alias" {
			pos--
		}
		node := astnav.GetTouchingPropertyName(sf, pos)
		for node != nil {
			if kind == "alias" && node.Parent != nil && node.Parent.Kind == ast.KindTypeAliasDeclaration {
				break
			}
			if kind == "ArrayLiteralExpression" && node.Kind == ast.KindArrayLiteralExpression {
				break
			}
			node = node.Parent
		}
		// "<method>+prop:<name>": the method, then getPropertyOfType(<name>) on its result, then the method again
		// (the second response is recorded).
		prop := ""
		if i := strings.Index(method, "+prop:"); i >= 0 {
			method, prop = method[:i], method[i+len("+prop:"):]
		}
		sp := map[string]any{"snapshot": snap, "project": proj, "location": string(nodeHandleFrom(node))}
		r, e := p.call(method, sp)
		if prop != "" && e == "" {
			p.call("getPropertyOfType", map[string]any{"snapshot": snap, "project": proj, "type": get(r, "id"), "name": prop})
			r, e = p.call(method, sp)
		}
		label := fx + "/" + file + " " + method + " @" + needle
		if e != "" {
			p.emit(label, map[string]any{"error": strings.SplitN(e, "\n", 2)[0]})
			continue
		}
		p.emit(label, r)
		if get(r, "target") != nil {
			tr, te := p.call("getTargetOfType", map[string]any{"snapshot": snap, "project": proj, "objectId": get(r, "id")})
			if te != "" {
				p.emit(label+" target", map[string]any{"error": te})
			} else {
				p.emit(label+" target", tr)
			}
		}
	}
}
