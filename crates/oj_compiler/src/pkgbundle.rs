// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// oj-native partial bundling: concatenate one node_modules package's internal
// file graph into a single self-contained ES module, served as one request
// instead of one-per-file. Unlike esbuild's static CJS->ESM conversion (Vite's
// pre-bundle, which has a UMD/dynamic-export interop tail), each module keeps a
// real CommonJS runtime here (`module`/`exports`/`require` via an inline
// registry), so UMD and dynamic-export packages bundle correctly. Cross-package
// dependencies stay as native ESM imports to *their* bundles, so shared packages
// dedupe and their interop is likewise preserved.
//
// This module is the pure emitter: given each internal module's factory body and
// its resolved dependency edges, it produces the bundle source. Graph discovery,
// per-file compilation (via `cjs::analyze_for_factory`), and serving live in the
// server crate.

/// Where a `require(spec)` inside an internal module resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepTarget {
    /// Another file inside this same package bundle, by its bundle id.
    Internal(String),
    /// A different package — imported as native ESM from its own bundle.
    External(String),
}

/// Whether an internal module is a CommonJS or an ES module. CJS bodies run with
/// `(module, exports, require)` and reach deps through the `__oj_deps` map; ESM
/// bodies (from `bundle::compile_esm_factory`) run with `(module, __oj_exports,
/// __oj_require)` and have their dep targets baked into `__oj_require("#id"|"@url")`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    Cjs,
    Esm,
}

/// One file inside the package, already lowered to a factory body.
#[derive(Debug, Clone)]
pub struct PkgModule {
    /// Stable id within the bundle (the file's package-relative path).
    pub id: String,
    /// CJS or ESM. Decides the factory signature and interop at the boundary.
    pub kind: ModuleKind,
    /// Factory body. CJS: uses `module`/`exports`/`require`. ESM: uses
    /// `module`/`__oj_exports`/`__oj_require` plus the `__oj_esm`/`__oj_export_star`
    /// helpers from the enclosing bundle scope.
    pub body: String,
    /// CJS only: `require()` specifier -> where it resolves. ESM modules bake
    /// their targets into the body, so this is empty for them.
    pub deps: Vec<(String, DepTarget)>,
}

fn js_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

fn is_valid_ident(name: &str) -> bool {
    !name.is_empty()
        && name != "default"
        && name.chars().enumerate().all(|(i, c)| {
            c == '_' || c == '$' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
        })
}

/// Emit the self-contained ESM bundle for one package.
///
/// - `modules`: every internal file, in any order.
/// - `entry_id`: the id whose exports become the bundle's exports.
/// - `externals`: distinct cross-package specifiers, in stable order.
/// - `named_exports`: statically-known named exports of the entry to re-export.
pub fn emit_package_bundle(
    modules: &[PkgModule],
    entry_id: &str,
    externals: &[String],
    named_exports: &[String],
) -> String {
    let mut out = String::new();

    // 1. Import each cross-package dependency as native ESM (one bundle each).
    //    `__oj_extns[url]` is the namespace; an ESM importer sees it directly, a
    //    CJS importer sees its unwrapped CommonJS value via `__oj_extcjs`.
    for (i, ext) in externals.iter().enumerate() {
        out.push_str(&format!(
            "import * as __oj_extns{i} from {};\n",
            js_str(ext)
        ));
    }
    out.push_str("function __oj_extcjs(ns) {\n");
    out.push_str("  return ns && ns.__cjs_exports !== undefined ? ns.__cjs_exports : (ns && ns.default !== undefined ? ns.default : ns);\n");
    out.push_str("}\n");
    out.push_str("const __oj_extns = {\n");
    for (i, ext) in externals.iter().enumerate() {
        out.push_str(&format!("  {}: __oj_extns{i},\n", js_str(ext)));
    }
    out.push_str("};\n");
    out.push_str("function __oj_ext_target(url, importerKind) {\n");
    out.push_str("  const ns = __oj_extns[url];\n");
    out.push_str("  return importerKind === \"esm\" ? ns : __oj_extcjs(ns);\n");
    out.push_str("}\n");

    // 2. ESM interop helpers (Vite's __esModule/default interop shape).
    out.push_str("function __oj_esm(exports, getters) {\n");
    out.push_str("  Object.defineProperty(exports, \"__esModule\", { value: true });\n");
    out.push_str("  for (const k of Object.keys(getters)) Object.defineProperty(exports, k, { enumerable: true, get: getters[k] });\n");
    out.push_str("}\n");
    out.push_str("function __oj_export_star(from, exports) {\n");
    out.push_str("  for (const k of Object.keys(from)) if (k !== \"default\" && !Object.prototype.hasOwnProperty.call(exports, k)) Object.defineProperty(exports, k, { enumerable: true, get: () => from[k] });\n");
    out.push_str("}\n");
    out.push_str("function __oj_cjs_ns(rec) {\n");
    out.push_str("  const ns = { __proto__: null };\n");
    out.push_str("  const raw = () => rec.exports;\n");
    out.push_str("  Object.defineProperty(ns, \"default\", { enumerable: true, get: () => (raw() && raw().__esModule ? raw().default : raw()) });\n");
    out.push_str("  for (const k of Object.keys(rec.exports)) if (k !== \"default\") Object.defineProperty(ns, k, { enumerable: true, get: () => raw()[k] });\n");
    out.push_str(
        "  Object.defineProperty(ns, \"__cjs_exports\", { enumerable: true, get: raw });\n",
    );
    out.push_str("  return ns;\n");
    out.push_str("}\n");

    // 3. Per-CJS-module dependency maps: spec -> "#<internalId>" | "@<externalUrl>".
    out.push_str("const __oj_deps = {\n");
    for m in modules {
        if m.kind != ModuleKind::Cjs {
            continue;
        }
        out.push_str(&format!("  {}: {{", js_str(&m.id)));
        let mut first = true;
        for (spec, target) in &m.deps {
            if !first {
                out.push_str(", ");
            }
            first = false;
            let v = match target {
                DepTarget::Internal(id) => format!("#{id}"),
                DepTarget::External(pkg) => format!("@{pkg}"),
            };
            out.push_str(&format!("{}: {}", js_str(spec), js_str(&v)));
        }
        out.push_str("},\n");
    }
    out.push_str("};\n");

    // 4. Registry: each module's kind + factory. CJS factories take
    //    (module, exports, require); ESM factories take (module, __oj_exports,
    //    __oj_require) and lean on the helpers defined above.
    out.push_str("const __oj_reg = {\n");
    for m in modules {
        let (params, kind) = match m.kind {
            ModuleKind::Cjs => ("module, exports, require", "cjs"),
            ModuleKind::Esm => ("module, __oj_exports, __oj_require", "esm"),
        };
        out.push_str(&format!(
            "  {}: {{ kind: {}, factory: function ({}) {{\n{}\n}} }},\n",
            js_str(&m.id),
            js_str(kind),
            params,
            m.body,
        ));
    }
    out.push_str("};\n");

    // 5. Instantiation + interop-aware require, so CJS<->ESM edges resolve
    //    the way Node/Vite interop does.
    out.push_str(
        "const __oj_inst = {};\n\
         function __oj_get(id) { return __oj_inst[id] || __oj_instantiate(id); }\n\
         function __oj_instantiate(id) {\n\
         \x20 const reg = __oj_reg[id];\n\
         \x20 if (!reg) throw new Error(\"[oj] module not in package bundle: \" + id);\n\
         \x20 const module = { exports: {} };\n\
         \x20 const rec = { module, exports: module.exports, ns: null, kind: reg.kind };\n\
         \x20 __oj_inst[id] = rec;\n\
         \x20 if (reg.kind === \"cjs\") {\n\
         \x20   const map = __oj_deps[id] || {};\n\
         \x20   const require = (spec) => {\n\
         \x20     const t = map[spec];\n\
         \x20     if (t === undefined) throw new Error(\"[oj] unresolved require(\" + JSON.stringify(spec) + \") in package bundle\");\n\
         \x20     return __oj_require_target(t, \"cjs\");\n\
         \x20   };\n\
         \x20   reg.factory.call(module.exports, module, module.exports, require);\n\
         \x20   rec.exports = module.exports;\n\
         \x20 } else {\n\
         \x20   const req = (t) => __oj_require_target(t, \"esm\");\n\
         \x20   reg.factory.call(undefined, module, module.exports, req);\n\
         \x20   rec.exports = module.exports;\n\
         \x20 }\n\
         \x20 return rec;\n\
         }\n\
         function __oj_require_target(t, importerKind) {\n\
         \x20 if (t[0] === \"@\") return __oj_ext_target(t.slice(1), importerKind);\n\
         \x20 const rec = __oj_get(t.slice(1));\n\
         \x20 if (importerKind === \"esm\" && rec.kind === \"cjs\") {\n\
         \x20   if (!rec.ns) rec.ns = __oj_cjs_ns(rec);\n\
         \x20   return rec.ns;\n\
         \x20 }\n\
         \x20 return rec.exports;\n\
         }\n\
         function __oj_ns_of(id) {\n\
         \x20 const rec = __oj_get(id);\n\
         \x20 if (rec.kind === \"cjs\") { if (!rec.ns) rec.ns = __oj_cjs_ns(rec); return rec.ns; }\n\
         \x20 return rec.exports;\n\
         }\n\
         function __oj_import_lazy(t) {\n\
         \x20 if (t[0] === \"#\") return Promise.resolve(__oj_ns_of(t.slice(1)));\n\
         \x20 if (t[0] === \"@\") return import(t.slice(1));\n\
         \x20 return import(t);\n\
         }\n",
    );

    // 6. Instantiate the entry and re-export it as ESM. `__oj_entry_ns` is the
    //    namespace view (getters for named + default); `__oj_entry_cjs` is the
    //    raw value other bundles' CJS requires should see.
    out.push_str(&format!(
        "const __oj_entry_ns = __oj_ns_of({});\n",
        js_str(entry_id)
    ));
    out.push_str(&format!(
        "const __oj_entry_cjs = __oj_get({}).exports;\n",
        js_str(entry_id)
    ));
    out.push_str(
        "export default (__oj_entry_ns && __oj_entry_ns.default !== undefined) ? __oj_entry_ns.default : __oj_entry_cjs;\n",
    );
    out.push_str("export const __cjs_exports = __oj_entry_cjs;\n");

    let mut seen = std::collections::HashSet::new();
    for name in named_exports {
        if is_valid_ident(name) && seen.insert(name.as_str()) {
            out.push_str(&format!(
                "const __oj_x_{name} = __oj_entry_ns[{}];\nexport {{ __oj_x_{name} as {name} }};\n",
                js_str(name),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> serde_json::Value {
        run_probe(
            src,
            "import def, { __cjs_exports } from BUNDLE;\nprocess.stdout.write(JSON.stringify({ def, cjs: __cjs_exports }));\n",
        )
    }

    // Evaluate the emitted ESM under node with a caller-supplied probe. The token
    // `BUNDLE` in `probe_tpl` is replaced with the bundle file's path literal.
    fn run_probe(src: &str, probe_tpl: &str) -> serde_json::Value {
        let dir = std::env::temp_dir().join(format!("oj-pkgbundle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(format!("b-{}.mjs", fastrand_like(src)));
        std::fs::write(&f, src).unwrap();
        let lit = format!("{:?}", f.to_string_lossy());
        let probe = dir.join(format!(
            "probe-{}.mjs",
            fastrand_like(&(src.to_string() + probe_tpl))
        ));
        std::fs::write(&probe, probe_tpl.replace("BUNDLE", &lit)).unwrap();
        let out = std::process::Command::new("node")
            .arg(&probe)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "node failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    // A tiny deterministic name so parallel tests don't collide (no rand dep).
    fn fastrand_like(s: &str) -> u64 {
        let mut h: u64 = 1469598103934665603;
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        h
    }

    fn cjs(id: &str, body: &str, deps: Vec<(&str, DepTarget)>) -> PkgModule {
        PkgModule {
            id: id.into(),
            kind: ModuleKind::Cjs,
            body: body.into(),
            deps: deps.into_iter().map(|(s, t)| (s.to_string(), t)).collect(),
        }
    }
    fn esm(id: &str, body: &str) -> PkgModule {
        PkgModule {
            id: id.into(),
            kind: ModuleKind::Esm,
            body: body.into(),
            deps: vec![],
        }
    }

    #[test]
    fn bundles_internal_cjs_graph_with_entry_exports() {
        // entry requires an internal file; both are CJS.
        let modules = vec![
            cjs(
                "index.js",
                "const dep = require(\"./dep\");\nmodule.exports.greet = (n) => dep.hi + n;",
                vec![("./dep", DepTarget::Internal("dep.js".into()))],
            ),
            cjs("dep.js", "module.exports.hi = \"hi \";", vec![]),
        ];
        let src = emit_package_bundle(&modules, "index.js", &[], &["greet".into()]);
        // Cross-file require resolves and the named export is callable.
        let v = run_probe(
            &src,
            "import { greet } from BUNDLE;\nprocess.stdout.write(JSON.stringify({ g: greet(\"x\") }));\n",
        );
        assert_eq!(
            v["g"],
            serde_json::json!("hi x"),
            "cross-file cjs require works: {v}"
        );
    }

    #[test]
    fn default_export_follows_module_exports() {
        let modules = vec![cjs("index.js", "module.exports = 41 + 1;", vec![])];
        let src = emit_package_bundle(&modules, "index.js", &[], &[]);
        let v = run(&src);
        assert_eq!(
            v["def"],
            serde_json::json!(42),
            "module.exports becomes default: {v}"
        );
    }

    #[test]
    fn esmodule_flag_unwraps_default() {
        let modules = vec![cjs(
            "index.js",
            "Object.defineProperty(exports, \"__esModule\", { value: true });\nexports.default = 7;",
            vec![],
        )];
        let src = emit_package_bundle(&modules, "index.js", &[], &[]);
        let v = run(&src);
        assert_eq!(
            v["def"],
            serde_json::json!(7),
            "__esModule default unwrapped: {v}"
        );
    }

    #[test]
    fn external_require_routes_through_esm_import() {
        // The external dep isn't resolvable here; assert the wiring is emitted
        // (import + namespace map + deps map) rather than executing it.
        let modules = vec![cjs(
            "index.js",
            "const react = require(\"react\");\nmodule.exports = react;",
            vec![("react", DepTarget::External("react".into()))],
        )];
        let src = emit_package_bundle(&modules, "index.js", &["/@oj-pkg/react.mjs".into()], &[]);
        assert!(
            src.contains("import * as __oj_extns0 from \"/@oj-pkg/react.mjs\""),
            "{src}"
        );
        assert!(
            src.contains("__oj_extns0,"),
            "external namespace mapped: {src}"
        );
        assert!(
            src.contains(r#""react": "@react""#),
            "external dep mapped: {src}"
        );
    }

    // --- ESM-inside-the-package coverage ---------------------------------------

    #[test]
    fn esm_entry_named_and_default_exports() {
        // An ESM entry compiled by bundle::compile_esm_factory shape.
        let modules = vec![esm(
            "index.js",
            "__oj_esm(__oj_exports, { \"answer\": () => answer, \"default\": () => __oj_default });\nvar __oj_default;\nconst answer = 42;\n__oj_default = \"D\";",
        )];
        let src = emit_package_bundle(&modules, "index.js", &[], &["answer".into()]);
        let v = run_probe(
            &src,
            "import def, { answer } from BUNDLE;\nprocess.stdout.write(JSON.stringify({ def, answer }));\n",
        );
        assert_eq!(v["answer"], serde_json::json!(42), "esm named export: {v}");
        assert_eq!(v["def"], serde_json::json!("D"), "esm default export: {v}");
    }

    #[test]
    fn esm_module_importing_internal_cjs_gets_interop_namespace() {
        // ESM entry imports an internal CJS file: default = the cjs value (no
        // __esModule), and a named key reads through the interop namespace.
        let modules = vec![
            esm(
                "index.js",
                "__oj_esm(__oj_exports, { \"n\": () => n, \"who\": () => who });\nvar _oj_m0 = __oj_require(\"#dep.js\");\nconst n = _oj_m0.val;\nconst who = _oj_m0.name;",
            ),
            cjs("dep.js", "module.exports.val = 5;\nmodule.exports.name = \"ada\";", vec![]),
        ];
        let src = emit_package_bundle(&modules, "index.js", &[], &["n".into(), "who".into()]);
        let v = run_probe(
            &src,
            "import { n, who } from BUNDLE;\nprocess.stdout.write(JSON.stringify({ n, who }));\n",
        );
        // Named keys of the internal CJS module read through the interop namespace.
        assert_eq!(
            v["n"],
            serde_json::json!(5),
            "cjs named via interop ns: {v}"
        );
        assert_eq!(
            v["who"],
            serde_json::json!("ada"),
            "cjs named via interop ns: {v}"
        );
    }

    #[test]
    fn cjs_module_requiring_internal_esm() {
        // CJS entry requires an internal ESM file and reads a named export off it.
        let modules = vec![
            cjs(
                "index.js",
                "const m = require(\"./m\");\nmodule.exports.v = m.val * 2;",
                vec![("./m", DepTarget::Internal("m.js".into()))],
            ),
            esm(
                "m.js",
                "__oj_esm(__oj_exports, { \"val\": () => val });\nconst val = 21;",
            ),
        ];
        let src = emit_package_bundle(&modules, "index.js", &[], &["v".into()]);
        let v = run_probe(
            &src,
            "import { v } from BUNDLE;\nprocess.stdout.write(JSON.stringify({ v }));\n",
        );
        assert_eq!(
            v["v"],
            serde_json::json!(42),
            "cjs reads named export of internal esm: {v}"
        );
    }

    #[test]
    fn dynamic_import_of_internal_resolves_to_namespace() {
        // `import("./lazy")` lowers to `__oj_import_lazy("#lazy.js")`, which the
        // runtime resolves to the already-bundled module's namespace (a promise).
        let modules = vec![
            esm(
                "index.js",
                "__oj_esm(__oj_exports, { \"load\": () => load });\nasync function load() { const m = await __oj_import_lazy(\"#lazy.js\"); return m.gift; }",
            ),
            esm("lazy.js", "__oj_esm(__oj_exports, { \"gift\": () => gift });\nconst gift = 7;"),
        ];
        let src = emit_package_bundle(&modules, "index.js", &[], &["load".into()]);
        let v = run_probe(
            &src,
            "import { load } from BUNDLE;\nload().then((g) => process.stdout.write(JSON.stringify({ g })));\n",
        );
        assert_eq!(
            v["g"],
            serde_json::json!(7),
            "dynamic import of internal works: {v}"
        );
    }

    #[test]
    fn esm_star_barrel_reexports_names() {
        // Entry `export *`s an internal ESM submodule; the re-exported name must
        // be reachable on the entry namespace at runtime.
        let modules = vec![
            esm(
                "index.js",
                "__oj_esm(__oj_exports, {});\nvar _oj_m0 = __oj_require(\"#sub.js\");\n__oj_export_star(_oj_m0, __oj_exports);",
            ),
            esm("sub.js", "__oj_esm(__oj_exports, { \"deep\": () => deep });\nconst deep = 99;"),
        ];
        // The builder would pass "deep" as a named export via the transitive walk.
        let src = emit_package_bundle(&modules, "index.js", &[], &["deep".into()]);
        let v = run_probe(
            &src,
            "import { deep } from BUNDLE;\nimport * as ns from BUNDLE;\nprocess.stdout.write(JSON.stringify({ deep, nsHasDeep: \"deep\" in ns }));\n",
        );
        assert_eq!(
            v["deep"],
            serde_json::json!(99),
            "star-barrel name re-exported: {v}"
        );
    }
}
