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
"#,
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

import { pantry, recipeTitle } from "../lib/recipe.js";

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
        html.contains("lentil-soup-marker:4") && html.contains("pantry-marker"),
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
