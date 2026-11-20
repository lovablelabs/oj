// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The Start dev client bundle restored from oj's cache holds the routes the
//! app has now. The bundle loads its routes from the route tree, which the
//! dev server regenerates on every boot; the tree is in no closure (the dev
//! server rebundles on a route change itself), so the store keys each
//! generation by the trees' contents. A route created while no server ran
//! is in the client the next boot serves, and a boot with nothing changed
//! still restores. Before, the restore matched the old closure and served
//! the old tree: the new route answered on the server and 404'd in the
//! browser until `.oj-cache` was deleted.
//!
//! The same holds for the other inputs a restore cannot see in the closure:
//! the env client code inlines (`import.meta.env`, from the `.env` files in
//! `envDir` and the process vars `envPrefix` admits) and a config file given
//! by `--config`. Vite reads all of them anew on every start.
//!
//! The app is the Start fixture (see `start_app`) moved to file-based
//! routing.

#[allow(dead_code)]
mod start_app;

use std::path::Path;

use start_app::{app, Server};

const ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const Route = createRootRoute({
  component: () => (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  ),
});
"#;

const INDEX: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => <p id="home">home page</p>,
});
"#;

const EXTRA: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/extra")({
  component: () => <p id="extra">extra page</p>,
});
"#;

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

fn file_routed(name: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let root = &app.0;
    let routes = root.join("src/routes");
    std::fs::remove_dir_all(&routes).unwrap();
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::write(routes.join("__root.tsx"), ROOT).unwrap();
    std::fs::write(routes.join("index.tsx"), INDEX).unwrap();
    std::fs::write(root.join("src/routeTree.ts"), "export {};\n").unwrap();
    std::fs::write(root.join("src/router.tsx"), ROUTER).unwrap();
    Some(app)
}

/// The client bundle the dev server serves: its entry and every chunk the
/// entry names, concatenated.
fn client_bundle(dev: &Server) -> String {
    let (status, entry) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", dev.output());
    let mut text = entry.clone();
    // Every chunk the entry reaches: the ones it imports by name, and the
    // modules behind its `import()`s, which a hot bundle asks the dev server
    // for by module id (as a page does), with the chunks those import in turn.
    let mut seen = std::collections::HashSet::new();
    let mut queue = vec![entry];
    while let Some(code) = queue.pop() {
        for chunk in code
            .split(['"', '\''])
            // `./x.js` beside a chunk; `../x.js` from the answer to an
            // `import()`, which a boot restored from the cache gives as the
            // restored chunk's name.
            .filter_map(|s| s.strip_prefix("./").or_else(|| s.strip_prefix("../")))
            .filter(|s| s.ends_with(".js"))
        {
            if seen.insert(chunk.to_string()) {
                let (_, code) = dev.request("GET", &format!("/@oj-start/{chunk}"), &[]);
                text.push_str(&code);
                queue.push(code);
            }
        }
        for proxy in lazy_proxies(&code) {
            if seen.insert(proxy.clone()) {
                let path = format!(
                    "/@oj-start/hmr/lazy?id={}&clientId=test",
                    percent_encode(&proxy)
                );
                let (status, code) = dev.request("GET", &path, &[]);
                assert_eq!(status, 200, "the module behind {proxy}\n{}", dev.output());
                text.push_str(&code);
                queue.push(code);
            }
        }
    }
    text
}

/// The proxy ids of the `import()`s in `code`, as a hot bundle renders them:
/// `requestLazy("<module>", "<proxy id>")`.
fn lazy_proxies(code: &str) -> Vec<String> {
    code.split("requestLazy(")
        .skip(1)
        .filter_map(|call| {
            let args = call.split_once(')')?.0;
            let (_, proxy) = args.split_once(", ")?;
            let end = proxy.rfind('"')?;
            serde_json::from_str::<String>(&proxy[..=end]).ok()
        })
        .collect()
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// One `oj dev` boot of `root`: its client bundle and its log.
fn boot(root: &Path) -> (String, String) {
    boot_with(root, &[], &[])
}

/// [`boot`] with extra arguments and environment variables.
fn boot_with(root: &Path, args: &[&str], env: &[(&str, &str)]) -> (String, String) {
    let dev = Server::oj_dev_with_args(root, args, env);
    let (status, _) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "the page\n{}", dev.output());
    let bundle = client_bundle(&dev);
    (bundle, dev.output())
}

#[test]
fn a_route_added_between_boots_is_in_the_restored_client_bundle() {
    let Some(app) = file_routed("client-cache-routes") else {
        return;
    };
    let root = &app.0;

    let (bundle, log) = boot(root);
    assert!(!bundle.contains("\"/extra\""), "{log}");
    assert!(log.contains("client bundle cached"), "{log}");

    // A route created while no server runs.
    std::fs::write(root.join("src/routes/extra.tsx"), EXTRA).unwrap();
    let (bundle, log) = boot(root);
    assert!(
        bundle.contains("\"/extra\""),
        "the client bundle lacks the new route\n{log}"
    );
    assert!(
        !log.contains("client bundle restored from cache"),
        "the old bundle was restored\n{log}"
    );

    // Nothing changed since: the bundle restores, the route still in it.
    let (bundle, log) = boot(root);
    assert!(
        log.contains("client bundle restored from cache"),
        "an unchanged app rebuilt its client bundle\n{log}"
    );
    assert!(bundle.contains("\"/extra\""), "{log}");
}

/// The home route renders what the client sees of the env and the config.
const ENV_INDEX: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => (
    <p id="seen">
      {[import.meta.env.FIXTURE_FILE, import.meta.env.FIXTURE_SHELL, __FIXTURE_DEFINE__].join("|")}
    </p>
  ),
});
"#;

/// An env var from a `.env` file in `envDir`, one from the shell admitted by
/// `envPrefix`, and a define in a `--config` file each change the client
/// bundle: a boot after any of them changed rebuilds it with the new value,
/// and a boot with nothing changed restores it.
#[test]
fn the_env_and_a_config_file_key_the_restored_client_bundle() {
    let Some(app) = file_routed("client-cache-env") else {
        return;
    };
    let root = &app.0;
    std::fs::write(root.join("src/routes/index.tsx"), ENV_INDEX).unwrap();
    // The config under a name the store's epoch does not list, given by
    // --config, with the env files in a directory of their own.
    let config = std::fs::read_to_string(root.join("vite.config.ts")).unwrap();
    std::fs::remove_file(root.join("vite.config.ts")).unwrap();
    let config = config.replacen("envPrefix: [", "envDir: \"env\",\n  envPrefix: [", 1);
    assert!(config.contains("envDir"), "{config}");
    std::fs::write(root.join("vite.app.config.ts"), &config).unwrap();
    std::fs::create_dir_all(root.join("env")).unwrap();
    std::fs::write(root.join("env/.env"), "FIXTURE_FILE=file-one\n").unwrap();
    let args = ["--config", "vite.app.config.ts"];
    let shell = |v: &'static str| [("FIXTURE_SHELL", v)];

    let (bundle, log) = boot_with(root, &args, &shell("shell-one"));
    assert!(
        bundle.contains("file-one")
            && bundle.contains("shell-one")
            && bundle.contains("fixture-define-marker"),
        "{log}"
    );

    let restored = |log: &str| log.contains("client bundle restored from cache");
    let (bundle, log) = boot_with(root, &args, &shell("shell-one"));
    assert!(
        restored(&log),
        "an unchanged app rebuilt its client bundle\n{log}"
    );
    assert!(bundle.contains("file-one"), "{log}");

    std::fs::write(root.join("env/.env"), "FIXTURE_FILE=file-two\n").unwrap();
    let (bundle, log) = boot_with(root, &args, &shell("shell-one"));
    assert!(
        !restored(&log) && bundle.contains("file-two") && !bundle.contains("file-one"),
        "an envDir file edit kept the old bundle\n{log}"
    );

    let (bundle, log) = boot_with(root, &args, &shell("shell-two"));
    assert!(
        !restored(&log) && bundle.contains("shell-two") && !bundle.contains("shell-one"),
        "a prefixed shell var kept the old bundle\n{log}"
    );

    std::fs::write(
        root.join("vite.app.config.ts"),
        config.replace("fixture-define-marker", "define-two"),
    )
    .unwrap();
    let (bundle, log) = boot_with(root, &args, &shell("shell-two"));
    assert!(
        !restored(&log)
            && bundle.contains("define-two")
            && !bundle.contains("fixture-define-marker"),
        "a --config file edit kept the old bundle\n{log}"
    );

    let (_, log) = boot_with(root, &args, &shell("shell-two"));
    assert!(
        restored(&log),
        "an unchanged app rebuilt its client bundle\n{log}"
    );
}

/// The home route renders the defines the config makes.
const DEFINES_INDEX: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => <p id="seen">{[__FIXTURE_DEFINE__, __FIXTURE_COMPUTED__].join("|")}</p>,
});
"#;

/// A define from a file the config imports, and one the config computes from
/// an env var no `envPrefix` exposes, each change the client bundle. Vite
/// restarts on an edit to a config import (`configFileDependencies`) and
/// evaluates the config anew on every start; the store keys the bundle by
/// the imported files and by the defines the evaluated config came to.
#[test]
fn defines_from_config_imports_and_the_env_key_the_restored_client_bundle() {
    let Some(app) = file_routed("client-cache-defines") else {
        return;
    };
    let root = &app.0;
    std::fs::write(root.join("src/routes/index.tsx"), DEFINES_INDEX).unwrap();
    let config = std::fs::read_to_string(root.join("vite.config.ts")).unwrap();
    let literal = r#"define: { __FIXTURE_DEFINE__: JSON.stringify("fixture-define-marker") },"#;
    assert!(config.contains(literal), "{config}");
    let config = format!(
        "import {{ importedDefine }} from \"./fixture-defines\";\n{}",
        config.replace(
            literal,
            r#"define: {
    __FIXTURE_DEFINE__: JSON.stringify(importedDefine),
    __FIXTURE_COMPUTED__: JSON.stringify(process.env.TSS_FIXTURE_COMPUTED ?? "computed-unset"),
  },"#,
        )
    );
    std::fs::write(root.join("vite.config.ts"), config).unwrap();
    let helper = root.join("fixture-defines.ts");
    std::fs::write(&helper, "export const importedDefine = \"imported-one\";\n").unwrap();
    let env = |v: &'static str| [("TSS_FIXTURE_COMPUTED", v)];
    let restored = |log: &str| log.contains("client bundle restored from cache");

    let (bundle, log) = boot_with(root, &[], &env("computed-one"));
    assert!(
        bundle.contains("imported-one") && bundle.contains("computed-one"),
        "{log}"
    );

    let (_, log) = boot_with(root, &[], &env("computed-one"));
    assert!(
        restored(&log),
        "an unchanged app rebuilt its client bundle\n{log}"
    );

    std::fs::write(&helper, "export const importedDefine = \"imported-two\";\n").unwrap();
    let (bundle, log) = boot_with(root, &[], &env("computed-one"));
    assert!(
        !restored(&log) && bundle.contains("imported-two") && !bundle.contains("imported-one"),
        "an edit to a file the config imports kept the old bundle\n{log}"
    );

    let (bundle, log) = boot_with(root, &[], &env("computed-two"));
    assert!(
        !restored(&log) && bundle.contains("computed-two") && !bundle.contains("computed-one"),
        "a define computed from the env kept the old bundle\n{log}"
    );

    let (_, log) = boot_with(root, &[], &env("computed-two"));
    assert!(
        restored(&log),
        "an unchanged app rebuilt its client bundle\n{log}"
    );
}

/// The home route of the apps below: what it shows comes from `src/text.ts`.
const TEXT_INDEX: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { text } from "../text";

export const Route = createFileRoute("/")({
  component: () => <p id="home">{text}</p>,
});
"#;

/// A plugin that saves files the moment the client bundle reaches
/// `src/late.ts`, the last module of a chain of imports: every module above
/// it has been read by then, and the bundle is not yet written or cached.
/// An editor's save lands there by chance; this one lands there every time,
/// once: `save-armed.json` arms it, with the files to write and to remove,
/// at once or `after` so many milliseconds.
const SAVE_WHILE_BUNDLING: &str = r#"{
      name: "fixture-save-while-bundling",
      transform(_code: string, id: string) {
        if (!id.endsWith("/src/late.ts") || this.environment?.name !== "client") return;
        const armed = `${process.cwd()}/save-armed.json`;
        if (!saveFs.existsSync(armed)) return;
        const plan = JSON.parse(saveFs.readFileSync(armed, "utf8"));
        saveFs.rmSync(armed);
        const save = () => {
          for (const [file, text] of Object.entries(plan.write ?? {})) {
            saveFs.writeFileSync(`${process.cwd()}/${file}`, text as string);
          }
          for (const file of plan.remove ?? []) saveFs.rmSync(`${process.cwd()}/${file}`);
        };
        if (plan.after) setTimeout(save, plan.after);
        else save();
      },
    },
    "#;

/// `src/text.ts`, which the home route shows, importing `import`.
fn text_module(text: &str, import: &str) -> String {
    format!("import {{ late }} from \"./{import}\";\nexport const text = \"{text}\" + late;\n")
}

/// A file-routed app whose home route shows `src/text.ts`, which imports
/// `src/late.ts`, with the saving plugin in its config.
fn app_saving_while_bundling(name: &str) -> Option<start_app::App> {
    let app = file_routed(name)?;
    let root = &app.0;
    std::fs::write(root.join("src/routes/index.tsx"), TEXT_INDEX).unwrap();
    std::fs::write(
        root.join("src/text.ts"),
        text_module("built-before-the-save", "late"),
    )
    .unwrap();
    std::fs::write(root.join("src/late.ts"), "export const late = 1;\n").unwrap();
    let config = std::fs::read_to_string(root.join("vite.config.ts")).unwrap();
    assert!(config.contains("plugins: [\n"), "{config}");
    let config = format!(
        "import saveFs from \"node:fs\";\n{}",
        config.replacen(
            "plugins: [\n",
            &format!("plugins: [\n    {SAVE_WHILE_BUNDLING}"),
            1
        )
    );
    std::fs::write(root.join("vite.config.ts"), config).unwrap();
    Some(app)
}

fn arm(root: &Path, plan: serde_json::Value) {
    std::fs::write(root.join("save-armed.json"), plan.to_string()).unwrap();
}

/// A source file saved while the client bundle is building is not in that
/// bundle, and the bundle must not be cached as if it were. The store keys a
/// bundle by its files' contents once it is written: the save keyed the old
/// code by the new contents, so every later boot restored the old code for
/// the new source (the server rendered the new one) until `.oj-cache` was
/// deleted. Vite keeps a transform only when its module was not invalidated
/// while it ran (`loadAndTransform`), and caches no app module across starts.
///
/// `src/text.ts` is read once, before the bundle can reach the module it
/// imports, so the bundle of the first boot holds it as it was whatever
/// order the bundler works in.
#[test]
fn a_file_saved_while_the_bundle_builds_is_not_cached_as_built() {
    let Some(app) = app_saving_while_bundling("client-cache-saved-mid-build") else {
        return;
    };
    let root = &app.0;
    arm(
        root,
        serde_json::json!({
            "write": { "src/text.ts": text_module("saved-while-bundling", "late") },
        }),
    );

    let (_, log) = boot(root);
    let source = std::fs::read_to_string(root.join("src/text.ts")).unwrap();
    assert!(
        source.contains("saved-while-bundling") && !root.join("save-armed.json").exists(),
        "the module was not saved during the bundle\n{log}"
    );

    // The next boot serves the module as saved, restored or rebuilt.
    let (bundle, log) = boot(root);
    assert!(
        bundle.contains("saved-while-bundling") && !bundle.contains("built-before-the-save"),
        "the client bundle holds the module as it was before the save\n{log}"
    );

    // And the bundle of the saved module is cached like any other.
    let (bundle, log) = boot(root);
    assert!(
        log.contains("client bundle restored from cache"),
        "an unchanged app rebuilt its client bundle\n{log}"
    );
    assert!(bundle.contains("saved-while-bundling"), "{log}");
}

/// The same for a save that lands while a running server writes a new
/// output. The engine that builds the dev bundle watches its modules, and
/// once an output is done it loads a module saved meanwhile again, for the
/// next hot update, through the same plugins. Here an edit to `src/late.ts`
/// starts a rebundle, and `src/text.ts` is saved a moment later, while the
/// engine generates the output from the modules it already holds: the save
/// is queued behind that output, and the engine reloads the module the
/// instant the output is done, ahead of the dev server caching it. The
/// output is judged by what was loaded for it: judged by the engine's
/// modules as loaded by then, the save compared equal to itself and the old
/// output was cached as the new source. The server is stopped as soon as the
/// rebundle is done, as a restart right after a save stops it, before the
/// save's own rebundle can store the right bundle.
#[test]
fn a_file_the_hot_engine_reloads_after_its_output_is_not_cached_as_built() {
    let Some(app) = app_saving_while_bundling("client-cache-saved-hot") else {
        return;
    };
    let root = &app.0;
    {
        let dev = Server::oj_dev(root);
        let (status, _) = dev.request("GET", "/", &[]);
        assert_eq!(status, 200, "the page\n{}", dev.output());
        assert!(client_bundle(&dev).contains("built-before-the-save"));
        arm(
            root,
            serde_json::json!({
                "write": { "src/text.ts": text_module("saved-while-bundling", "late") },
                "after": 150,
            }),
        );
        std::fs::write(root.join("src/late.ts"), "export const late = 2;\n").unwrap();
        assert!(logged(&dev, "oj start: rebuilt", 1), "{}", dev.output());
    }
    let source = std::fs::read_to_string(root.join("src/text.ts")).unwrap();
    assert!(
        source.contains("saved-while-bundling"),
        "the module was not saved during the rebundle"
    );

    let (bundle, log) = boot(root);
    assert!(
        bundle.contains("saved-while-bundling") && !bundle.contains("built-before-the-save"),
        "the client bundle holds the module as it was before the save\n{log}"
    );
}

/// A component deleted, and its import dropped, while the bundle builds: the
/// bundle holds the component, and the closure (existing files only) does
/// not list it, so the bundle was stored under the key of the sources
/// without it and every later boot served the deleted component.
#[test]
fn a_component_deleted_while_the_bundle_builds_is_not_in_the_next_boots_bundle() {
    let Some(app) = app_saving_while_bundling("client-cache-deleted-mid-build") else {
        return;
    };
    let root = &app.0;
    // text.ts -> gone.ts -> late.ts: both are read before late.ts is reached.
    std::fs::write(
        root.join("src/text.ts"),
        text_module("built-before-the-save", "gone"),
    )
    .unwrap();
    std::fs::write(
        root.join("src/gone.ts"),
        "import { late as last } from \"./late\";\nexport const late = \"deleted-component\" + last;\n",
    )
    .unwrap();
    arm(
        root,
        serde_json::json!({
            "write": { "src/text.ts": text_module("saved-without-the-component", "late") },
            "remove": ["src/gone.ts"],
        }),
    );

    let (_, log) = boot(root);
    assert!(
        !root.join("src/gone.ts").exists() && !root.join("save-armed.json").exists(),
        "the component was not deleted during the bundle\n{log}"
    );

    let (bundle, log) = boot(root);
    assert!(
        bundle.contains("saved-without-the-component") && !bundle.contains("deleted-component"),
        "the client bundle holds the deleted component\n{log}"
    );
}

/// How many times the server's log has `needle`, once it has it `times`.
fn logged(dev: &Server, needle: &str, times: usize) -> bool {
    let deadline = std::time::Instant::now() + start_app::WAIT;
    while dev.output().matches(needle).count() < times {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    true
}

/// A component deleted while the server runs, its import dropped with it,
/// leaves the session's later bundles cacheable: the bundle after the
/// deletion holds no module of the deleted file, so the file has no say in
/// whether that bundle is as built, and the next boot restores it.
#[test]
fn a_component_deleted_in_a_session_leaves_later_bundles_cached() {
    let Some(app) = app_saving_while_bundling("client-cache-deleted-in-session") else {
        return;
    };
    let root = &app.0;
    std::fs::write(
        root.join("src/text.ts"),
        text_module("with-the-component", "gone"),
    )
    .unwrap();
    std::fs::write(
        root.join("src/gone.ts"),
        "import { late as last } from \"./late\";\nexport const late = \"deleted-component\" + last;\n",
    )
    .unwrap();

    {
        let dev = Server::oj_dev(root);
        let (status, _) = dev.request("GET", "/", &[]);
        assert_eq!(status, 200, "the page\n{}", dev.output());
        assert!(client_bundle(&dev).contains("deleted-component"));

        std::fs::write(
            root.join("src/text.ts"),
            text_module("without-the-component", "late"),
        )
        .unwrap();
        std::fs::remove_file(root.join("src/gone.ts")).unwrap();
        assert!(logged(&dev, "oj start: rebuilt", 1), "{}", dev.output());
        std::thread::sleep(std::time::Duration::from_millis(500));
        std::fs::write(
            root.join("src/text.ts"),
            text_module("edited-after-the-deletion", "late"),
        )
        .unwrap();
        assert!(logged(&dev, "oj start: rebuilt", 2), "{}", dev.output());
        let bundle = client_bundle(&dev);
        let log = dev.output();
        assert!(
            bundle.contains("edited-after-the-deletion") && !bundle.contains("deleted-component"),
            "{log}"
        );
        assert!(
            !log.contains("client bundle not cached"),
            "a bundle after the deletion was refused the cache\n{log}"
        );
    }

    let (bundle, log) = boot(root);
    assert!(
        log.contains("client bundle restored from cache"),
        "the bundle after the deletion was not cached\n{log}"
    );
    assert!(
        bundle.contains("edited-after-the-deletion") && !bundle.contains("deleted-component"),
        "{log}"
    );
}
