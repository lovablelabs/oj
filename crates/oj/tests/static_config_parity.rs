// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! oj reads a literal vite.config in Rust (oj_server's static_config) instead
//! of evaluating it with Vite on a JS engine. Its output must equal the
//! engine's, field for field, or a boot without an engine serves a different
//! config. Runs against e2e/fixtures/stylex-app's installed Vite,
//! `@vitejs/plugin-react` and `@stylexjs/unplugin` (`npm ci` there); skips
//! without them unless `OJ_REQUIRE_FIXTURES` is set.

use std::path::{Path, PathBuf};

fn fixture() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/stylex-app")
        .canonicalize()
        .unwrap();
    if dir.join("node_modules/vite").is_dir() {
        return Some(dir);
    }
    assert!(
        std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
        "OJ_REQUIRE_FIXTURES is set but the stylex-app fixture is not installed"
    );
    eprintln!("skipping: npm ci --prefix e2e/fixtures/stylex-app");
    None
}

/// An app at `dir/app` using the fixture's node_modules, with this config.
fn app(fixture: &Path, dir: &Path, file: &str, config: &str) -> (PathBuf, PathBuf) {
    let root = dir.join("app");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::copy(fixture.join("package.json"), root.join("package.json")).unwrap();
    std::os::unix::fs::symlink(fixture.join("node_modules"), root.join("node_modules")).unwrap();
    std::fs::write(root.join(file), config).unwrap();
    let root = root.canonicalize().unwrap();
    let vite = root.join(file);
    (root, vite)
}

fn both(root: &Path, vite: &Path) -> (Option<serde_json::Value>, serde_json::Value) {
    let (read, evaluated) = oj_server::plugins::static_and_engine_extraction(root, vite);
    (
        read,
        strip(evaluated.expect("the engine evaluates the config")),
    )
}

/// The engine's output without what it records for the cache.
fn strip(mut evaluated: serde_json::Value) -> serde_json::Value {
    let fields = evaluated.as_object_mut().unwrap();
    fields.remove("__deps");
    fields.remove("__depsTruncated");
    // What the evaluation read of the env, for the cache like __deps.
    fields.remove("__envReads");
    fields.remove("__envEnumerated");
    fields.remove("__envPrefixes");
    // The transcript, stored apart from the values in the cache.
    fields.remove("__stderr");
    evaluated
}

const CONFIGS: &[(&str, &str, &str)] = &[
    (
        "react and stylex, plain object",
        "vite.config.mjs",
        r#"import stylex from "@stylexjs/unplugin";
import react from "@vitejs/plugin-react";
export default { plugins: [stylex.vite(), react()], logLevel: "warn" };
"#,
    ),
    (
        "defineConfig, stylex options and a port",
        "vite.config.ts",
        r#"import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import stylex from "@stylexjs/unplugin";
export default defineConfig({
  plugins: [react(), stylex.vite({ useCSSLayers: true, dev: true, unstable_moduleResolution: { type: "commonJS" } })],
  server: { port: 8080 },
  clearScreen: false,
});
"#,
    ),
    (
        "react only with path aliases",
        "vite.config.ts",
        r#"import path from "node:path";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
      "~lib": path.resolve(import.meta.dirname, "src/../lib"),
      shared: "/abs/shared",
    },
  },
});
"#,
    ),
    (
        "the Lovable template's shape: host, port and an alias",
        "vite.config.ts",
        r#"import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "path";
export default defineConfig({
  server: { host: "::", port: 8080 },
  plugins: [react()],
  resolve: { alias: { "@": path.resolve(__dirname, "./src") } },
});
"#,
    ),
    (
        "an IPv4 host with stylex",
        "vite.config.mjs",
        r#"import stylex from "@stylexjs/unplugin";
import react from "@vitejs/plugin-react";
export default { plugins: [stylex.vite(), react()], server: { host: "0.0.0.0" } };
"#,
    ),
    (
        "trailing-slash aliases and a whole-number stylex option",
        "vite.config.mjs",
        r#"import stylex from "@stylexjs/unplugin";
import react from "@vitejs/plugin-react";
export default {
  plugins: [react(), stylex.vite({ lightningcssOptions: { targets: { chrome: 6225920 } } })],
  resolve: { alias: { "@/": "/abs/src/", "~": "/abs/lib/", "@": "/abs/other" } },
};
"#,
    ),
];

#[test]
fn literal_configs_read_in_rust_equal_the_engine_evaluation() {
    let Some(fixture) = fixture() else {
        return;
    };
    oj_server::plugins::engine_jobs_via_subprocess(PathBuf::from(env!("CARGO_BIN_EXE_oj")));
    for (name, file, config) in CONFIGS {
        let dir = tempfile::tempdir().unwrap();
        let (root, vite) = app(&fixture, dir.path(), file, config);
        let (read, evaluated) = both(&root, &vite);
        let read = read.unwrap_or_else(|| panic!("{name}: Rust declined the config"));
        assert_eq!(read, evaluated, "{name}");
    }
}

/// Vite allows the workspace root, not the app, when the app sits in a
/// pnpm workspace: the Rust reader must find the same root.
#[test]
fn a_workspace_app_allows_the_workspace_root_like_vite() {
    let Some(fixture) = fixture() else {
        return;
    };
    oj_server::plugins::engine_jobs_via_subprocess(PathBuf::from(env!("CARGO_BIN_EXE_oj")));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pnpm-workspace.yaml"), "packages: [app]\n").unwrap();
    let (root, vite) = app(&fixture, dir.path(), "vite.config.mjs", CONFIGS[0].2);
    let (read, evaluated) = both(&root, &vite);
    assert_eq!(read.expect("Rust reads the config"), evaluated);
    assert_eq!(
        evaluated["fsAllow"][0],
        dir.path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
}

/// Vite bundles a symlinked config from its real path, so `import.meta.dirname`
/// is the target's directory, not the app's.
#[test]
fn a_symlinked_config_resolves_paths_from_its_target_like_vite() {
    let Some(fixture) = fixture() else {
        return;
    };
    oj_server::plugins::engine_jobs_via_subprocess(PathBuf::from(env!("CARGO_BIN_EXE_oj")));
    let dir = tempfile::tempdir().unwrap();
    let config = r#"import path from "node:path";
import react from "@vitejs/plugin-react";
export default { plugins: [react()], resolve: { alias: { "@": path.resolve(import.meta.dirname, "src") } } };
"#;
    let (root, vite) = app(&fixture, dir.path(), "vite.config.mjs", "");
    let shared = dir.path().join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("vite.config.mjs"), config).unwrap();
    std::fs::remove_file(&vite).unwrap();
    std::os::unix::fs::symlink(shared.join("vite.config.mjs"), &vite).unwrap();
    let (read, evaluated) = both(&root, &vite);
    assert_eq!(read.expect("Rust reads the config"), evaluated);
    let target = shared.canonicalize().unwrap().join("src");
    assert_eq!(evaluated["alias"]["@"], target.to_string_lossy().as_ref());
}

/// What Vite makes of an app, by the package.json files around its config.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Vite {
    /// It loads, and the cache dir is under the app.
    App,
    /// It loads, and the cache dir is under the directory above the app.
    Above,
    /// The config does not load.
    Fails,
}

/// Where oj's two readings stand beside Vite's.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Oj {
    /// The reader and the engine both do as Vite does.
    AsVite,
    /// oj's engine does not boot under this package.json (one with a BOM,
    /// or a broken one further up) though Node runs Vite there, so the
    /// reader is held to Vite's answer alone.
    EngineFails,
    /// oj's engine loads the config where Node refuses the package.json
    /// (its `require` is laxer than Node's). The reader declines, as it must
    /// where Vite fails, so the day the engine refuses too the app fails as
    /// under Vite.
    EngineLoads,
    /// Vite loads the config; the reader declines it (JSON serde's `Value`
    /// cannot hold) and oj's engine does not boot under it.
    NeitherLoads,
}

/// An app at `top/ws/app`, its config in the app or a symlink there to one
/// in `top/shared`.
struct Layout {
    file: &'static str,
    /// The app's package.json.
    own: Option<&'static [u8]>,
    /// The one in the directory above the app.
    above: Option<&'static [u8]>,
    /// With the config a symlink into `top/shared`: that directory's
    /// package.json.
    shared: Option<Option<&'static [u8]>>,
    /// The app's package.json cannot be read.
    unreadable: bool,
}

const MJS: &str = "vite.config.mjs";
const TS: &str = "vite.config.ts";

fn layout(file: &'static str, own: Option<&'static [u8]>, above: Option<&'static [u8]>) -> Layout {
    Layout {
        file,
        own,
        above,
        shared: None,
        unreadable: false,
    }
}

fn linked(file: &'static str, own: &'static [u8], shared: Option<&'static [u8]>) -> Layout {
    Layout {
        shared: Some(shared),
        ..layout(file, Some(own), Some(b"{}"))
    }
}

/// Vite's default `cacheDir` sits under the nearest package.json it can
/// load (findNearestPackageData walks past one that throws), else under the
/// root, and its config load fails on a package.json rolldown or Node
/// refuses. The reader must give the engine's answer where Vite loads the
/// config and leave the app to the engine where Vite fails. Each row's
/// `Vite` is what Vite 8.3.5 did with the layout under Node 24.
#[test]
fn the_reader_follows_vite_through_the_package_json_files_around_a_config() {
    let Some(fixture) = fixture() else {
        return;
    };
    oj_server::plugins::engine_jobs_via_subprocess(PathBuf::from(env!("CARGO_BIN_EXE_oj")));
    const OBJECT: Option<&[u8]> = Some(b"{}");
    const MODULE: Option<&[u8]> = Some(br#"{"type":"module"}"#);
    let cases: Vec<(Layout, Vite, Oj)> = vec![
        // Not JSON to rolldown, which bundles the config: the load fails.
        (layout(MJS, Some(b""), OBJECT), Vite::Fails, Oj::AsVite),
        (
            layout(MJS, Some(b"{ not json"), OBJECT),
            Vite::Fails,
            Oj::AsVite,
        ),
        (layout(MJS, Some(b""), None), Vite::Fails, Oj::AsVite),
        (
            layout(MJS, None, Some(b"{ not json")),
            Vite::Fails,
            Oj::AsVite,
        ),
        // Bytes that are not UTF-8: rolldown refuses what JSON.parse reads.
        (
            layout(MJS, Some(b"{\"n\":\"\xe9\"}"), OBJECT),
            Vite::Fails,
            Oj::AsVite,
        ),
        // Only the nearest one is read.
        (
            layout(MJS, OBJECT, Some(b"{ not json")),
            Vite::App,
            Oj::EngineFails,
        ),
        // JSON loadPackageData throws on: the manifest above is the nearest.
        (layout(MJS, Some(b"null"), OBJECT), Vite::Above, Oj::AsVite),
        (
            layout(MJS, Some(br#"{"sideEffects":[1]}"#), OBJECT),
            Vite::Above,
            Oj::AsVite,
        ),
        // Loadable, whatever it holds.
        (
            layout(MJS, Some(b"\xef\xbb\xbf{}"), OBJECT),
            Vite::App,
            Oj::EngineFails,
        ),
        (layout(MJS, Some(b"[]"), OBJECT), Vite::App, Oj::AsVite),
        (
            layout(MJS, Some(br#"{"name":1}"#), OBJECT),
            Vite::App,
            Oj::AsVite,
        ),
        (
            layout(MJS, Some(br#"{"sideEffects":["*.css",["a"]]}"#), OBJECT),
            Vite::App,
            Oj::AsVite,
        ),
        // JSON serde's Value cannot hold: read by Vite, left to the engine.
        (
            layout(MJS, Some(br#"{"n":"\ud800"}"#), OBJECT),
            Vite::App,
            Oj::NeitherLoads,
        ),
        // No manifest in the app.
        (layout(MJS, None, OBJECT), Vite::Above, Oj::AsVite),
        // None Vite can load anywhere: the app's node_modules.
        (layout(MJS, Some(b"null"), None), Vite::App, Oj::AsVite),
        (layout(MJS, None, None), Vite::App, Oj::AsVite),
        // One that cannot be read is passed over, by rolldown too.
        (
            Layout {
                unreadable: true,
                ..layout(MJS, OBJECT, OBJECT)
            },
            Vite::Above,
            Oj::AsVite,
        ),
        (
            Layout {
                unreadable: true,
                ..layout(MJS, OBJECT, Some(b"{ not json"))
            },
            Vite::Fails,
            Oj::AsVite,
        ),
        // A symlinked config: rolldown reads the package.json by the link.
        (linked(MJS, b"{ not json", OBJECT), Vite::Fails, Oj::AsVite),
        (
            linked(MJS, b"{}", Some(b"{ not json")),
            Vite::App,
            Oj::AsVite,
        ),
        (linked(MJS, b"{}", Some(b"null")), Vite::App, Oj::AsVite),
        // A CommonJS config (no "type": "module" in the nearest loadable
        // package.json): its require of the plugin has Node read the
        // nearest package.json, which must be a package config.
        (layout(TS, OBJECT, OBJECT), Vite::App, Oj::AsVite),
        (
            layout(TS, Some(b"null"), OBJECT),
            Vite::Fails,
            Oj::EngineLoads,
        ),
        (
            layout(TS, Some(b"[]"), OBJECT),
            Vite::Fails,
            Oj::EngineLoads,
        ),
        (
            layout(TS, Some(br#"{"name":1}"#), OBJECT),
            Vite::Fails,
            Oj::EngineLoads,
        ),
        (
            layout(TS, Some(br#"{"name":null}"#), OBJECT),
            Vite::Fails,
            Oj::EngineLoads,
        ),
        (
            layout(TS, Some(br#"{"type":1}"#), OBJECT),
            Vite::Fails,
            Oj::EngineLoads,
        ),
        (
            layout(TS, Some(br#"{"type":"weird"}"#), OBJECT),
            Vite::App,
            Oj::AsVite,
        ),
        // `null` is passed over for the module above: an ES module config.
        (layout(TS, Some(b"null"), MODULE), Vite::Above, Oj::AsVite),
        // `[]` loads, so it is the nearest and the config is CommonJS.
        (
            layout(TS, Some(b"[]"), MODULE),
            Vite::Fails,
            Oj::EngineLoads,
        ),
        (
            layout(TS, Some(br#"{"type":"module","name":1}"#), OBJECT),
            Vite::App,
            Oj::AsVite,
        ),
        // Node reads by the file it runs: the symlink's target.
        (
            linked(TS, b"{}", Some(b"null")),
            Vite::Fails,
            Oj::EngineLoads,
        ),
        (
            linked(TS, b"{}", Some(br#"{"name":1}"#)),
            Vite::Fails,
            Oj::EngineLoads,
        ),
        (
            linked(TS, b"{}", Some(b"{ not json")),
            Vite::Fails,
            Oj::EngineLoads,
        ),
        (linked(TS, b"null", OBJECT), Vite::Above, Oj::AsVite),
        (
            linked(TS, br#"{"type":"module"}"#, Some(b"null")),
            Vite::App,
            Oj::AsVite,
        ),
    ];
    let config = |file: &str| {
        CONFIGS
            .iter()
            .find(|(name, of, _)| *of == file && (file == MJS || name.starts_with("the Lovable")))
            .unwrap()
            .2
    };
    let text = |bytes: Option<&[u8]>| bytes.map(|b| String::from_utf8_lossy(b).into_owned());
    for (at, (layout, vite_does, oj)) in cases.iter().enumerate() {
        let what = format!(
            "#{at} {}: app {:?}{}, above {:?}{}",
            layout.file,
            text(layout.own),
            if layout.unreadable {
                " (unreadable)"
            } else {
                ""
            },
            text(layout.above),
            match layout.shared {
                Some(shared) => format!(", config linked beside {:?}", text(shared)),
                None => String::new(),
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().canonicalize().unwrap();
        if layout.above.is_none() && top.ancestors().any(|d| d.join("package.json").exists()) {
            eprintln!("skipping ({what}): a package.json above the temp dir");
            continue;
        }
        let ws = top.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let (root, vite) = app(&fixture, &ws, layout.file, config(layout.file));
        std::fs::remove_file(root.join("package.json")).unwrap();
        if let Some(own) = layout.own {
            std::fs::write(root.join("package.json"), own).unwrap();
        }
        if let Some(above) = layout.above {
            std::fs::write(ws.join("package.json"), above).unwrap();
        }
        if let Some(manifest) = layout.shared {
            let shared = top.join("shared");
            std::fs::create_dir_all(&shared).unwrap();
            std::os::unix::fs::symlink(fixture.join("node_modules"), shared.join("node_modules"))
                .unwrap();
            if let Some(manifest) = manifest {
                std::fs::write(shared.join("package.json"), manifest).unwrap();
            }
            std::fs::rename(&vite, shared.join(layout.file)).unwrap();
            std::os::unix::fs::symlink(shared.join(layout.file), &vite).unwrap();
        }
        if layout.unreadable {
            use std::os::unix::fs::PermissionsExt;
            let manifest = root.join("package.json");
            std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o000)).unwrap();
            if std::fs::read(&manifest).is_ok() {
                eprintln!("skipping ({what}): this user reads a file without read permission");
                continue;
            }
        }
        let declined = oj_server::plugins::static_extraction_declined(&root, &vite);
        let (read, evaluated) = oj_server::plugins::static_and_engine_extraction(&root, &vite);
        let want = match vite_does {
            Vite::App => Some(root.join("node_modules/.vite")),
            Vite::Above => Some(ws.join("node_modules/.vite")),
            Vite::Fails => None,
        };
        let cache_dir = |v: &serde_json::Value| PathBuf::from(v["cacheDir"].as_str().unwrap());
        let Some(want) = want else {
            // Vite fails: the reader leaves the app to the engine, for the
            // package.json and nothing else.
            let reason =
                declined.unwrap_or_else(|| panic!("Rust read a config Vite fails on, {what}"));
            assert!(
                reason.contains("package.json")
                    && (reason.contains("is not JSON") || reason.contains("no package config")),
                "declined for another reason ({reason}), {what}"
            );
            assert!(read.is_none(), "{what}");
            match oj {
                Oj::AsVite => assert!(evaluated.is_none(), "the engine loaded the config, {what}"),
                Oj::EngineLoads => {
                    assert!(evaluated.is_some(), "the engine fails now, {what}: AsVite")
                }
                other => panic!("{other:?} is not for a config Vite fails on, {what}"),
            }
            continue;
        };
        match oj {
            Oj::AsVite => {
                let evaluated =
                    strip(evaluated.unwrap_or_else(|| panic!("the engine failed, {what}")));
                assert_eq!(cache_dir(&evaluated), want, "the engine, {what}");
                let read = read.unwrap_or_else(|| panic!("Rust declined ({declined:?}), {what}"));
                assert_eq!(read, evaluated, "{what}");
            }
            Oj::EngineFails => {
                assert!(evaluated.is_none(), "the engine boots now, {what}: AsVite");
                let read = read.unwrap_or_else(|| panic!("Rust declined ({declined:?}), {what}"));
                assert_eq!(cache_dir(&read), want, "{what}");
            }
            Oj::NeitherLoads => {
                assert!(evaluated.is_none(), "the engine boots now, {what}");
                assert!(read.is_none(), "Rust reads it now, {what}");
            }
            Oj::EngineLoads => panic!("EngineLoads is for a config Vite fails on, {what}"),
        }
    }
}

/// The template holds one Vite's defaults (8.0 had another `fs.deny`), so a
/// Vite other than the one it was taken from goes to the engine.
#[test]
fn another_vite_version_goes_to_the_engine() {
    let Some(fixture) = fixture() else {
        return;
    };
    let config = CONFIGS[3].2;
    let with_vite = |version: &str| {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::copy(fixture.join("package.json"), root.join("package.json")).unwrap();
        // The fixture's install, but a Vite claiming `version`.
        let modules = root.join("node_modules");
        std::fs::create_dir_all(&modules).unwrap();
        for entry in std::fs::read_dir(fixture.join("node_modules"))
            .unwrap()
            .flatten()
        {
            if entry.file_name() != "vite" {
                std::os::unix::fs::symlink(entry.path(), modules.join(entry.file_name())).unwrap();
            }
        }
        let vite = modules.join("vite");
        std::fs::create_dir_all(vite.join("dist/client")).unwrap();
        std::fs::write(
            vite.join("package.json"),
            format!(r#"{{"name":"vite","version":"{version}"}}"#),
        )
        .unwrap();
        std::fs::write(root.join("vite.config.ts"), config).unwrap();
        oj_server::plugins::static_extraction(&root, &root.join("vite.config.ts"))
    };
    assert!(
        with_vite("8.3.5").is_some(),
        "the template's own version is read"
    );
    assert!(
        with_vite("8.0.0").is_none(),
        "Vite 8.0 must go to the engine"
    );
}

/// Shapes the Rust reader must hand to the engine: values only evaluation
/// knows, plugins outside oj's native set, and options it has no template for.
#[test]
fn configs_outside_the_literal_shape_go_to_the_engine() {
    let Some(fixture) = fixture() else {
        return;
    };
    let declined = [
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react()], server: { host: true } };\n",
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react(), { name: \"mine\" }] };\n",
        "import react from \"@vitejs/plugin-react\";\nexport default ({ mode }) => ({ plugins: [react()] });\n",
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react()], server: { port: Number(process.env.PORT) } };\n",
        "import react from \"@vitejs/plugin-react\";\nconsole.log(1);\nexport default { plugins: [react()] };\n",
        "import stylex from \"@stylexjs/unplugin\";\nexport default { plugins: [stylex.vite()] };\n",
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react({ babel: {} })] };\n",
        // plugin-react imported but not listed: its config hook never runs.
        "import react from \"@vitejs/plugin-react\";\nexport default { server: { port: 3000 } };\n",
        // JavaScript keeps the last of a repeated key.
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react()], resolve: { alias: { \"@\": \"/a\", \"@\": \"/b\" } } };\n",
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react()], resolve: { alias: { a: \"/a\" }, alias: { b: \"/b\" } } };\n",
        // `__proto__` sets the prototype, not an alias.
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react()], resolve: { alias: { __proto__: \"/x\" } } };\n",
    ];
    for config in declined {
        let dir = tempfile::tempdir().unwrap();
        let (root, vite) = app(&fixture, dir.path(), "vite.config.mjs", config);
        let (read, _) = oj_server::plugins::static_and_engine_extraction(&root, &vite);
        assert!(read.is_none(), "Rust must decline:\n{config}");
    }
}
