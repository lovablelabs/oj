// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `import recipe from "./recipe.json" with { type: "json" }` in a module
//! the server renders, as Vite runs it: the SSR transform replaces every
//! static import with a `__vite_ssr_import__` call (`ssrTransform.ts`,
//! `defineImport` over each `ImportDeclaration`), so the attribute never
//! reaches the runtime and the JSON file is the json plugin's module
//! (`export default {...}` plus named exports). oj's SSR compile kept the
//! attribute, the engine then asked for a JSON module under it, and the
//! JavaScript module the host served did not match: "Cannot resolve module
//! ./recipe.json from .../recipe.js?v=0" on every page. Node itself requires
//! the attribute for JSON, so the source cannot drop it.
//!
//! The app is the Start fixture (see `start_app`).

#[allow(dead_code)]
mod start_app;

use std::path::Path;

use start_app::{app, build, Server};

fn write_route(root: &Path) {
    std::fs::write(
        root.join("src/lib/recipe.json"),
        r#"{ "title": "lentil-soup-marker", "serves": 4 }"#,
    )
    .unwrap();
    // A plain .js module, as a project written in JavaScript has it; the
    // re-export covers the other static form that carries attributes.
    std::fs::write(
        root.join("src/lib/recipe.js"),
        r#"import recipe from "./recipe.json" with { type: "json" };
export { default as pantry } from "./pantry.json" with { type: "json" };

export const recipeTitle = `${recipe.title}:${recipe.serves}`;
// A dynamic import keeps its options in the compiled code; the runner loads
// it by its specifier, as Vite's `__vite_ssr_dynamic_import__` does.
export const later = (await import("./later.json", { with: { type: "json" } })).default;
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib/later.json"),
        r#"{ "when": "later-marker" }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib/pantry.json"),
        r#"{ "staple": "pantry-marker" }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/routes/recipe.tsx"),
        r#"import { createRoute } from "@tanstack/react-router";

import { later, pantry, recipeTitle } from "../lib/recipe.js";

import { rootRoute } from "./__root";

export const recipeRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/recipe",
  component: Recipe,
});

function Recipe() {
  return (
    <main>
      <pre id="recipe-title">{recipeTitle}</pre>
      <pre id="pantry">{pantry.staple}</pre>
      <pre id="later">{later.when}</pre>
    </main>
  );
}
"#,
    )
    .unwrap();
    let tree = root.join("src/routeTree.ts");
    let source = std::fs::read_to_string(&tree).unwrap();
    let source = source
        .replace(
            "import { rootRoute } from \"./routes/__root\";",
            "import { rootRoute } from \"./routes/__root\";\nimport { recipeRoute } from \"./routes/recipe\";",
        )
        .replace("addChildren([", "addChildren([recipeRoute, ");
    assert!(source.contains("recipeRoute, "), "{source}");
    std::fs::write(tree, source).unwrap();
}

#[test]
fn a_json_import_with_attributes_renders_on_the_server() {
    let Some(app) = app("json-attrs") else {
        return;
    };
    let root = &app.0;
    write_route(root);

    let dev = Server::oj_dev(root);
    let (status, html) = dev.request("GET", "/recipe", &[]);
    assert_eq!(
        status,
        200,
        "the page with a `with {{ type: \"json\" }}` import\n{}",
        dev.output()
    );
    assert!(
        html.contains("lentil-soup-marker:4")
            && html.contains("pantry-marker")
            && html.contains("later-marker"),
        "the server render is missing the JSON values:\n{html}\n{}",
        dev.output()
    );
    assert!(
        !dev.output().contains("Cannot resolve module"),
        "the runner failed to resolve the JSON import\n{}",
        dev.output()
    );
    drop(dev);

    build(root);
}

/// The app's node_modules as a directory of its own (each entry a link to
/// the fixture's), so a test can add a package beside the installed ones.
fn own_node_modules(root: &Path) {
    let modules = root.join("node_modules");
    let fixture = std::fs::read_link(&modules).unwrap();
    std::fs::remove_file(&modules).unwrap();
    std::fs::create_dir(&modules).unwrap();
    for entry in std::fs::read_dir(&fixture).unwrap() {
        let entry = entry.unwrap();
        std::os::unix::fs::symlink(entry.path(), modules.join(entry.file_name())).unwrap();
    }
}

/// An ES module package that imports its own JSON with the attribute, as
/// Node requires of one (`import data from "./data.json" with { type:
/// "json" }`), and re-exports another.
fn write_dependency(root: &Path) {
    let dep = root.join("node_modules/json-attr-dep");
    std::fs::create_dir_all(&dep).unwrap();
    std::fs::write(
        dep.join("package.json"),
        r#"{ "name": "json-attr-dep", "version": "1.0.0", "type": "module", "exports": "./index.js" }"#,
    )
    .unwrap();
    std::fs::write(dep.join("data.json"), r#"{ "marker": "dep-json-marker" }"#).unwrap();
    std::fs::write(dep.join("more.json"), r#"{ "marker": "dep-more-marker" }"#).unwrap();
    std::fs::write(
        dep.join("index.js"),
        r#"import data from "./data.json" with { type: "json" };
export { default as more } from "./more.json" with { type: "json" };

export const depMarker = data.marker;
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/routes/recipe.tsx"),
        r#"import { createRoute } from "@tanstack/react-router";
import { depMarker, more } from "json-attr-dep";

import { rootRoute } from "./__root";

export const recipeRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/recipe",
  component: Recipe,
});

function Recipe() {
  return (
    <main>
      <pre id="dep">{depMarker}</pre>
      <pre id="more">{more.marker}</pre>
    </main>
  );
}
"#,
    )
    .unwrap();
}

fn renders_the_dependencys_json(label: &str, server: &Server) {
    let (status, html) = server.request("GET", "/recipe", &[]);
    assert_eq!(
        status,
        200,
        "{label}: the page importing the dependency\n{html}\n{}",
        server.output()
    );
    assert!(
        html.contains("dep-json-marker") && html.contains("dep-more-marker"),
        "{label}: the render is missing the dependency's JSON values:\n{html}\n{}",
        server.output()
    );
}

/// The dependency's JSON reaches the render in dev and from the built
/// server; `inline` lists the package under `ssr.noExternal`.
fn dependency_json_renders(name: &str, inline: bool) {
    let Some(app) = app(name) else {
        return;
    };
    let root = &app.0;
    write_route(root);
    own_node_modules(root);
    write_dependency(root);
    if inline {
        let config = root.join("vite.config.ts");
        let source = std::fs::read_to_string(&config).unwrap();
        let with = source.replace(
            "resolve: { tsconfigPaths: true },",
            "resolve: { tsconfigPaths: true },\n  ssr: { noExternal: [\"json-attr-dep\"] },",
        );
        assert_ne!(with, source, "the fixture's config changed shape");
        std::fs::write(config, with).unwrap();
    }

    let dev = Server::oj_dev(root);
    renders_the_dependencys_json("dev", &dev);
    let (status, _) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", dev.output());
    assert!(
        !dev.output().contains("Cannot resolve module"),
        "the runner failed to resolve the dependency's JSON import\n{}",
        dev.output()
    );
    drop(dev);

    build(root);
    let built = Server::node_server(root);
    renders_the_dependencys_json("build", &built);
}

/// A dependency that imports its own JSON with the attribute. Vite
/// externalizes a node_modules package on the server and Node loads it, JSON
/// module and all (the module runner's `runExternalModule` is a native
/// `import()`), so the import asks for a JSON module and gets one. oj's host
/// answered every dependency JSON with the json plugin's JavaScript module,
/// no match for it: "Cannot resolve module ./data.json".
#[test]
fn a_dependencys_json_import_with_attributes_renders_on_the_server() {
    dependency_json_renders("json-attrs-dep", false);
}

/// The same package inlined (`ssr.noExternal`): Vite runs it through
/// ssrTransform like the app's own modules, so its attributes go and the
/// json plugin's module is what it imports.
#[test]
fn an_inlined_dependencys_json_import_with_attributes_renders_on_the_server() {
    dependency_json_renders("json-attrs-dep-inline", true);
}
