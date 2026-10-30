// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};

use oj_typecheck::{Checker, SignatureJob};

/// A temporary app root, removed on drop.
struct App(PathBuf);

impl App {
    /// The layout Lovable templates use: a solution `tsconfig.json`
    /// referencing an app project (`src`) and a node project (the config).
    fn with_references(name: &str) -> App {
        let app = App::empty(name);
        app.write(
            "tsconfig.json",
            r#"{ "files": [], "references": [{ "path": "./tsconfig.app.json" }, { "path": "./tsconfig.node.json" }] }"#,
        );
        app.write(
            "tsconfig.app.json",
            r#"{ "compilerOptions": { "strict": true, "noEmit": true, "lib": ["ES2022", "DOM"], "module": "ESNext", "moduleResolution": "bundler" }, "include": ["src"] }"#,
        );
        app.write(
            "tsconfig.node.json",
            r#"{ "compilerOptions": { "strict": true, "noEmit": true, "lib": ["ES2022"], "module": "ESNext", "moduleResolution": "bundler" }, "include": ["vite.config.ts"] }"#,
        );
        app.write("vite.config.ts", "export default {};\n");
        app
    }

    fn empty(name: &str) -> App {
        let dir = std::env::temp_dir().join(format!("oj-typecheck-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        App(dir.canonicalize().unwrap())
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn open(&self) -> Checker {
        Checker::open(&self.0)
            .unwrap()
            .expect("the app has a tsconfig.json")
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(checker: &mut Checker) -> String {
    checker.check().unwrap().text()
}

/// Records the signatures and waits for the job, which a request would otherwise stop before it stores any.
fn record_and_wait(checker: &Checker) -> SignatureJob {
    checker.record_signatures(&[]);
    checker.wait_for_signatures()
}

/// The file names a job emitted, relative to the app, sorted.
fn emitted(app: &App, job: &SignatureJob) -> Vec<String> {
    let root = format!("{}/", app.0.display());
    let mut names: Vec<String> = job
        .emitted
        .iter()
        .map(|(file, _)| file.trim_start_matches(&root).to_string())
        .collect();
    names.sort();
    names
}

#[test]
fn a_solution_config_checks_every_referenced_project_once() {
    let app = App::with_references("refs");
    app.write("src/a.ts", "export const a: number = 'a';\n");
    app.write("vite.config.ts", "export const port: string = 1;\n");
    assert_eq!(
        text(&mut app.open()),
        "src/a.ts(1,14): error TS2322: Type 'string' is not assignable to type 'number'.\n\
         vite.config.ts(1,14): error TS2322: Type 'number' is not assignable to type 'string'.\n"
    );
}

#[test]
fn an_edit_named_in_changed_replaces_the_old_diagnostics() {
    let app = App::with_references("edit");
    let a = app.write("src/a.ts", "export const a: number = 'a';\n");
    let mut checker = app.open();
    assert_eq!(checker.check().unwrap().error_count(), 1);
    app.write("src/a.ts", "export const a: number = 1;\n");
    // Not yet named: the snapshot still holds the old text.
    assert_eq!(checker.check().unwrap().error_count(), 1);
    checker.changed([a.as_path()]).unwrap();
    let report = checker.check().unwrap();
    assert_eq!(report.text(), "");
    assert_eq!(report.generation, 2);
}

#[test]
fn a_created_file_joins_and_a_deleted_file_leaves_the_program() {
    let app = App::with_references("create");
    app.write("src/a.ts", "export const a = 1;\n");
    let mut checker = app.open();
    let b = app.write("src/b.ts", "export const b: string = 2;\n");
    checker.changed([b.as_path()]).unwrap();
    assert_eq!(
        text(&mut checker),
        "src/b.ts(1,14): error TS2322: Type 'number' is not assignable to type 'string'.\n"
    );
    std::fs::remove_file(&b).unwrap();
    checker.changed([b.as_path()]).unwrap();
    assert_eq!(text(&mut checker), "");
}

#[test]
fn a_change_to_a_file_typescript_never_reads_keeps_the_snapshot() {
    let app = App::with_references("inert");
    app.write("src/a.ts", "export const a = 1;\n");
    let mut checker = app.open();
    let css = app.write("src/a.css", "a {}\n");
    checker.changed([css.as_path()]).unwrap();
    assert_eq!(checker.generation(), 1);
}

#[test]
fn a_chained_message_prints_its_detail_lines_indented_as_tsc_does() {
    let app = App::with_references("chain");
    app.write(
        "src/a.ts",
        "export const f: (a: string) => void = (a: number) => {};\n",
    );
    assert_eq!(
        text(&mut app.open()),
        "src/a.ts(1,14): error TS2322: Type '(a: number) => void' is not assignable to type '(a: string) => void'.\n\
         \x20 Types of parameters 'a' and 'a' are incompatible.\n\
         \x20   Type 'string' is not assignable to type 'number'.\n"
    );
}

#[test]
fn a_root_without_tsconfig_has_nothing_to_check() {
    let app = App::empty("none");
    app.write("src/a.ts", "export const a: number = 'a';\n");
    assert!(Checker::open(&app.0).unwrap().is_none());
}

#[test]
fn a_single_config_without_references_is_checked_itself() {
    let app = App::empty("single");
    app.write(
        "tsconfig.json",
        r#"{ "compilerOptions": { "strict": true, "noEmit": true }, "include": ["src"] }"#,
    );
    app.write("src/a.ts", "export function f(x) { return x; }\n");
    assert_eq!(
        text(&mut app.open()),
        "src/a.ts(1,19): error TS7006: Parameter 'x' implicitly has an 'any' type.\n"
    );
}

#[test]
fn a_relative_changed_path_resolves_against_the_root() {
    let app = App::with_references("relative");
    app.write("src/a.ts", "export const a = 1;\n");
    let mut checker = app.open();
    app.write("src/a.ts", "export const a: string = 1;\n");
    checker.changed([Path::new("src/a.ts")]).unwrap();
    assert_eq!(checker.check().unwrap().error_count(), 1);
}

#[test]
fn an_edited_tsconfig_reaches_the_checker() {
    let app = App::with_references("config-edit");
    app.write("src/a.ts", "export function f(x) { return x; }\n");
    let mut checker = app.open();
    assert_eq!(checker.check().unwrap().error_count(), 1);
    let config = app.write(
        "tsconfig.app.json",
        r#"{ "compilerOptions": { "strict": false, "noEmit": true, "lib": ["ES2022", "DOM"], "module": "ESNext", "moduleResolution": "bundler" }, "include": ["src"] }"#,
    );
    checker.changed([config.as_path()]).unwrap();
    assert_eq!(text(&mut checker), "");
}

#[test]
fn a_reference_added_to_the_root_config_is_checked() {
    let app = App::empty("add-ref");
    app.write(
        "tsconfig.json",
        r#"{ "files": [], "references": [{ "path": "./tsconfig.app.json" }] }"#,
    );
    app.write(
        "tsconfig.app.json",
        r#"{ "compilerOptions": { "strict": true }, "include": ["src"] }"#,
    );
    app.write("src/a.ts", "export const a = 1;\n");
    app.write("tools/t.ts", "export const t: string = 1;\n");
    let mut checker = app.open();
    assert_eq!(text(&mut checker), "");
    app.write(
        "tsconfig.tools.json",
        r#"{ "compilerOptions": { "strict": true }, "include": ["tools"] }"#,
    );
    let root = app.write(
        "tsconfig.json",
        r#"{ "files": [], "references": [{ "path": "./tsconfig.app.json" }, { "path": "./tsconfig.tools.json" }] }"#,
    );
    checker.changed([root.as_path()]).unwrap();
    assert_eq!(
        text(&mut checker),
        "tools/t.ts(1,14): error TS2322: Type 'number' is not assignable to type 'string'.\n"
    );
}

#[test]
fn a_syntax_error_hides_the_type_errors_as_tsc_does() {
    let app = App::with_references("syntax");
    app.write("src/a.ts", "export const a: number = 'a';\nconst b = ;\n");
    app.write("src/c.ts", "export const c: number = 'c';\n");
    assert_eq!(
        text(&mut app.open()),
        "src/a.ts(2,11): error TS1109: Expression expected.\n"
    );
}

#[test]
fn declaration_errors_are_reported_when_the_project_emits_declarations() {
    let app = App::empty("declarations");
    app.write(
        "tsconfig.json",
        r#"{ "compilerOptions": { "strict": true, "declaration": true, "isolatedDeclarations": true }, "include": ["src"] }"#,
    );
    app.write("src/a.ts", "class P {}\nexport const g = () => new P();\n");
    assert_eq!(
        text(&mut app.open()),
        "src/a.ts(2,24): error TS9013: Expression type can't be inferred with --isolatedDeclarations.\n"
    );
}

#[test]
fn a_solution_configs_own_errors_are_reported() {
    let app = App::with_references("solution-errors");
    app.write(
        "tsconfig.json",
        r#"{ "files": [], "compilerOptions": {"stricty": true}, "references": [{ "path": "./tsconfig.app.json" }] }"#,
    );
    app.write("src/a.ts", "export const a = 1;\n");
    assert_eq!(
        text(&mut app.open()),
        "tsconfig.json(1,36): error TS5025: Unknown compiler option 'stricty'. Did you mean 'strict'?\n"
    );
}

#[test]
fn a_missing_reference_is_reported_and_the_others_still_checked() {
    let app = App::empty("missing-ref");
    app.write(
        "tsconfig.json",
        r#"{ "files": [], "references": [{ "path": "./tsconfig.app.json" }, { "path": "./packages/missing" }] }"#,
    );
    app.write(
        "tsconfig.app.json",
        r#"{ "compilerOptions": { "strict": true }, "include": ["src"] }"#,
    );
    app.write("src/a.ts", "export const a: number = 'a';\n");
    assert_eq!(
        text(&mut app.open()),
        format!(
            "src/a.ts(1,14): error TS2322: Type 'string' is not assignable to type 'number'.\n\
             error TS6053: File '{}/packages/missing/tsconfig.json' not found.\n",
            app.0.display()
        )
    );
}

#[test]
fn a_reference_to_a_directory_opens_its_tsconfig() {
    let app = App::empty("dir-ref");
    app.write(
        "tsconfig.json",
        r#"{ "files": [], "references": [{ "path": "./web" }] }"#,
    );
    app.write(
        "web/tsconfig.json",
        r#"{ "compilerOptions": { "strict": true }, "include": ["src"] }"#,
    );
    app.write("web/src/a.ts", "export const a: number = 'a';\n");
    assert_eq!(
        text(&mut app.open()),
        "web/src/a.ts(1,14): error TS2322: Type 'string' is not assignable to type 'number'.\n"
    );
}

#[test]
fn a_file_two_projects_include_is_reported_once() {
    let app = App::empty("shared");
    app.write(
        "tsconfig.json",
        r#"{ "files": [], "references": [{ "path": "./a.json" }, { "path": "./b.json" }] }"#,
    );
    app.write(
        "a.json",
        r#"{ "compilerOptions": { "strict": true }, "include": ["shared", "src"] }"#,
    );
    app.write(
        "b.json",
        r#"{ "compilerOptions": { "strict": true }, "include": ["shared"] }"#,
    );
    app.write("shared/s.ts", "export const s: number = 's';\n");
    app.write("src/x.ts", "export {};\n");
    assert_eq!(
        text(&mut app.open()),
        "shared/s.ts(1,14): error TS2322: Type 'string' is not assignable to type 'number'.\n"
    );
}

#[test]
fn a_diagnostic_without_a_file_prints_without_a_location() {
    let app = App::empty("global");
    app.write(
        "tsconfig.json",
        r#"{ "compilerOptions": { "strict": true, "types": ["missing"] }, "include": ["src"] }"#,
    );
    app.write("src/a.ts", "export const a = 1;\n");
    assert_eq!(
        text(&mut app.open()),
        "error TS2688: Cannot find type definition file for 'missing'.\n\
         \x20 The file is in the program because:\n\
         \x20   Entry point of type library 'missing' specified in compilerOptions\n"
    );
}

#[test]
fn a_moved_directory_takes_its_files_and_their_errors_along() {
    let app = App::with_references("move-dir");
    app.write("src/comp/bad.ts", "export const b: number = 'b';\n");
    let mut checker = app.open();
    assert_eq!(checker.check().unwrap().error_count(), 1);
    std::fs::rename(app.0.join("src/comp"), app.0.join("src/ui")).unwrap();
    // What the watcher reports: the directory that left, and the file that arrived.
    checker
        .changed([
            app.0.join("src/comp").as_path(),
            app.0.join("src/ui/bad.ts").as_path(),
        ])
        .unwrap();
    assert_eq!(
        text(&mut checker),
        "src/ui/bad.ts(1,14): error TS2322: Type 'string' is not assignable to type 'number'.\n"
    );
    std::fs::remove_dir_all(app.0.join("src/ui")).unwrap();
    checker.changed([app.0.join("src/ui").as_path()]).unwrap();
    assert_eq!(text(&mut checker), "");
}

#[test]
fn an_install_noted_by_package_json_resolves_the_new_package() {
    let app = App::with_references("install");
    app.write("package.json", r#"{ "name": "app" }"#);
    app.write(
        "src/a.ts",
        "import { v } from 'fakepkg';\nexport const a: number = v;\n",
    );
    let mut checker = app.open();
    assert!(text(&mut checker).contains("TS2307"));
    app.write(
        "node_modules/fakepkg/package.json",
        r#"{ "name": "fakepkg", "types": "index.d.ts" }"#,
    );
    app.write(
        "node_modules/fakepkg/index.d.ts",
        "export declare const v: number;\n",
    );
    let lock = app.write("package-lock.json", "{}");
    checker.changed([lock.as_path()]).unwrap();
    assert_eq!(text(&mut checker), "");
}

#[test]
fn a_root_reached_through_a_symlink_reports_paths_relative_to_it() {
    let app = App::with_references("symlink");
    app.write("src/a b.ts", "export const a: number = 'a';\n");
    let link = std::env::temp_dir().join(format!("oj-typecheck-link-{}", std::process::id()));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&app.0, &link).unwrap();
    let mut checker = Checker::open(&link).unwrap().unwrap();
    let report = text(&mut checker);
    let _ = std::fs::remove_file(&link);
    assert_eq!(
        report,
        "src/a b.ts(1,14): error TS2322: Type 'string' is not assignable to type 'number'.\n"
    );
}

/// A one-project app: `src/a.ts` exports `shape`, `src/b.ts` uses it.
fn importer_app(name: &str, a: &str) -> (App, PathBuf) {
    let app = App::empty(name);
    app.write(
        "tsconfig.json",
        r#"{ "compilerOptions": { "strict": true, "noEmit": true, "lib": ["ES2022"], "module": "ESNext", "moduleResolution": "bundler" }, "include": ["src"] }"#,
    );
    let a = app.write("src/a.ts", a);
    app.write(
        "src/b.ts",
        "import { shape } from './a';\nexport const n: number = shape.size;\n",
    );
    (app, a)
}

/// What a checker that opens the app now reports.
fn fresh(app: &App) -> String {
    text(&mut app.open())
}

#[test]
fn an_exported_type_change_rechecks_the_files_that_import_it() {
    for record in [false, true] {
        let (app, a) = importer_app(
            &format!("export-change-{record}"),
            "export const shape = { size: 1 };\n",
        );
        let mut checker = app.open();
        assert_eq!(text(&mut checker), "");
        if record {
            let job = record_and_wait(&checker);
            assert_eq!(emitted(&app, &job), ["src/a.ts", "src/b.ts"]);
        }
        app.write("src/a.ts", "export const shape = { size: 'one' };\n");
        checker.changed([a.as_path()]).unwrap();
        let report = text(&mut checker);
        assert_eq!(
            report,
            "src/b.ts(2,14): error TS2322: Type 'string' is not assignable to type 'number'.\n",
            "signatures recorded: {record}"
        );
        assert_eq!(report, fresh(&app));
    }
}

#[test]
fn an_edit_that_keeps_the_exported_types_keeps_the_importers_errors() {
    let (app, a) = importer_app("body-edit", "export const shape = { size: 'one' };\n");
    let mut checker = app.open();
    let before = text(&mut checker);
    assert!(before.contains("src/b.ts"), "{before}");
    assert_eq!(
        emitted(&app, &record_and_wait(&checker)),
        ["src/a.ts", "src/b.ts"]
    );
    for k in 0..3 {
        app.write(
            "src/a.ts",
            &format!("export const shape = {{ size: 'one' }};\n// edit {k}\n"),
        );
        checker.changed([a.as_path()]).unwrap();
        assert_eq!(text(&mut checker), before, "edit {k}");
        record_and_wait(&checker);
    }
}

#[test]
fn global_errors_stay_reported_after_edits() {
    let app = App::empty("globals");
    app.write(
        "tsconfig.json",
        r#"{ "compilerOptions": { "noLib": true, "noEmit": true }, "include": ["src"] }"#,
    );
    let a = app.write("src/a.ts", "export const a = 1;\n");
    app.write("src/b.ts", "export const b = 2;\n");
    let mut checker = app.open();
    let before = text(&mut checker);
    assert!(
        before.contains("error TS2318: Cannot find global type 'Array'."),
        "{before}"
    );
    record_and_wait(&checker);
    // A body edit re-checks one file; rewriting the same text re-checks none.
    app.write("src/a.ts", "export const a = 1;\n// edit\n");
    checker.changed([a.as_path()]).unwrap();
    assert_eq!(text(&mut checker), before);
    checker.changed([a.as_path()]).unwrap();
    assert_eq!(text(&mut checker), before);
    assert_eq!(before, fresh(&app));
}

#[test]
fn an_error_pointing_into_an_edited_file_is_still_reported() {
    // b.ts's error carries related information in a.ts ("'size' is declared here"); a.ts is then edited
    // without changing its exported types, so b.ts is not checked again and keeps that diagnostic.
    let (app, a) = importer_app(
        "related",
        "export interface Shape { size: number }\nexport const shape: Shape = { size: 1 };\n",
    );
    app.write(
        "src/b.ts",
        "import type { Shape } from './a';\nexport const s: Shape = {};\n",
    );
    let mut checker = app.open();
    let before = text(&mut checker);
    assert!(before.contains("TS2741"), "{before}");
    assert!(!record_and_wait(&checker).emitted.is_empty());
    for k in 0..4 {
        app.write(
            "src/a.ts",
            &format!(
                "export interface Shape {{ size: number }}\n// edit {k}\nexport const shape: Shape = {{ size: {k} }};\n"
            ),
        );
        checker.changed([a.as_path()]).unwrap();
        assert_eq!(text(&mut checker), before, "edit {k}");
    }
}

#[test]
fn every_edit_reports_what_a_fresh_check_does() {
    let app = App::empty("differential");
    app.write(
        "tsconfig.json",
        r#"{ "compilerOptions": { "strict": true, "noEmit": true, "lib": ["ES2022"], "module": "ESNext", "moduleResolution": "bundler" }, "include": ["src"] }"#,
    );
    app.write(
        "src/base.ts",
        "export type Id = number;\nexport const zero: Id = 0;\n",
    );
    app.write(
        "src/mid.ts",
        "import { type Id, zero } from './base';\nexport const next = (id: Id): Id => id + 1;\nexport const one = next(zero);\n",
    );
    app.write(
        "src/top.ts",
        "import { one, next } from './mid';\nexport const two: number = next(one);\n",
    );
    let edits: &[(&str, &str)] = &[
        (
            "src/base.ts",
            "export type Id = string;\nexport const zero: Id = '0';\n",
        ),
        (
            "src/mid.ts",
            "import { type Id, zero } from './base';\nexport const next = (id: Id): Id => id + '1';\nexport const one = next(zero);\n",
        ),
        (
            "src/top.ts",
            "import { one, next } from './mid';\nexport const two: string = next(one);\n",
        ),
        (
            "src/base.ts",
            "export type Id = number;\nexport const zero: Id = 0;\n",
        ),
        (
            "src/mid.ts",
            "import { type Id, zero } from './base';\nexport const next = (id: Id): Id => id + 1;\nexport const one = next(zero);\n// tail\n",
        ),
        (
            "src/top.ts",
            "import { one, next } from './mid';\nexport const two: number = next(one);\n",
        ),
    ];
    let mut checker = app.open();
    assert_eq!(text(&mut checker), fresh(&app));
    for (k, (file, body)) in edits.iter().enumerate() {
        let path = app.write(file, body);
        checker.changed([path.as_path()]).unwrap();
        assert_eq!(text(&mut checker), fresh(&app), "edit {k}: {file}");
        record_and_wait(&checker);
    }
}

#[test]
fn an_edit_that_interrupts_the_signature_job_still_reports_what_a_fresh_check_does() {
    let app = App::empty("interrupt");
    app.write(
        "tsconfig.json",
        r#"{ "compilerOptions": { "strict": true, "noEmit": true, "lib": ["ES2022"], "module": "ESNext", "moduleResolution": "bundler", "isolatedModules": true }, "include": ["src"] }"#,
    );
    // A chain of modules, each re-exporting a type built on the one before, so the job has work to stop in.
    app.write(
        "src/m0.ts",
        "export type T0 = { id: number };\nexport const v0: T0 = { id: 0 };\n",
    );
    for i in 1..300 {
        app.write(
            &format!("src/m{i}.ts"),
            &format!(
                "import {{ type T{p}, v{p} }} from './m{p}';\nexport type T{i} = T{p} & {{ n{i}: number }};\nexport const v{i}: T{i} = {{ ...v{p}, n{i}: {i} }};\n",
                p = i - 1
            ),
        );
    }
    let mut checker = app.open();
    assert_eq!(text(&mut checker), "");
    let edits = [
        "export type T0 = { id: string };\nexport const v0: T0 = { id: 0 };\n",
        "export type T0 = { id: string };\nexport const v0: T0 = { id: '0' };\n",
        "export type T0 = { id: number };\nexport const v0: T0 = { id: 0 };\n",
    ];
    for (k, body) in edits.iter().enumerate() {
        // Started, then stopped by the very next request.
        checker.record_signatures(&[]);
        let path = app.write("src/m0.ts", body);
        checker.changed([path.as_path()]).unwrap();
        assert_eq!(text(&mut checker), fresh(&app), "edit {k}");
    }
}

#[test]
fn a_file_the_signature_job_skips_still_checks_as_a_fresh_check_does() {
    let app = App::empty("skip");
    app.write(
        "tsconfig.json",
        r#"{ "compilerOptions": { "strict": true, "noEmit": true, "lib": ["ES2022"], "module": "ESNext", "moduleResolution": "bundler", "isolatedModules": true }, "include": ["src"] }"#,
    );
    // A generated tree that imports every module, as a TanStack route tree does.
    let mut tree = String::new();
    for i in 0..20 {
        app.write(
            &format!("src/r{i}.ts"),
            &format!("export const route{i} = {{ id: {i} }};\n"),
        );
        tree.push_str(&format!("import {{ route{i} }} from './r{i}';\n"));
    }
    tree.push_str("export const tree = [");
    tree.push_str(
        &(0..20)
            .map(|i| format!("route{i}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    tree.push_str("] as const;\nexport const firstId: number = tree[0].id;\n");
    let route_tree = app.write("src/routeTree.gen.ts", &tree);
    let mut checker = app.open();
    assert_eq!(text(&mut checker), "");
    for (k, body) in [
        "export const route0 = { id: 'zero' };\n",
        "export const route0 = { id: 0 };\n",
    ]
    .iter()
    .enumerate()
    {
        checker.record_signatures(std::slice::from_ref(&route_tree));
        let job = emitted(&app, &checker.wait_for_signatures());
        assert!(
            !job.contains(&"src/routeTree.gen.ts".to_string()),
            "edit {k}: {job:?}"
        );
        if k == 0 {
            assert_eq!(job.len(), 20, "every route but the tree: {job:?}");
        }
        let path = app.write("src/r0.ts", body);
        checker.changed([path.as_path()]).unwrap();
        assert_eq!(text(&mut checker), fresh(&app), "edit {k}");
    }
}

#[test]
fn a_project_the_edit_left_alone_holds_the_current_snapshot() {
    // An idle project's program must not keep the snapshot it was made from, which holds every project's program
    // of that time.
    let app = App::with_references("idle-project");
    let a = app.write("src/a.ts", "export const a = 1;\n");
    let mut checker = app.open();
    assert_eq!(text(&mut checker), "");
    for k in 0..2 {
        app.write("src/a.ts", &format!("export const a = 1;\n// edit {k}\n"));
        checker.changed([a.as_path()]).unwrap();
        assert_eq!(text(&mut checker), "");
        let programs = checker.incremental_programs();
        assert_eq!(programs.len(), 2, "{programs:?}");
        assert!(
            programs.iter().all(|(_, current)| *current),
            "edit {k}: {programs:?}"
        );
    }
}

#[test]
fn a_file_slow_to_emit_is_left_out_of_later_jobs() {
    let app = App::empty("slow");
    app.write(
        "tsconfig.json",
        r#"{ "compilerOptions": { "strict": true, "noEmit": true, "lib": ["ES2022"], "module": "ESNext", "moduleResolution": "bundler", "isolatedModules": true }, "include": ["src"] }"#,
    );
    // f0's importers: a change to its type leaves their signatures to the next job.
    app.write("src/f0.ts", "export const f0 = 0;\n");
    for i in 1..8 {
        app.write(
            &format!("src/f{i}.ts"),
            &format!("import {{ f0 }} from './f0';\nexport const f{i} = f0;\n"),
        );
    }
    // One file whose declaration is far larger than the others'.
    let props: String = (0..4000)
        .map(|i| format!("  p{i}: {{ id: {i}, name: 'n{i}', tags: ['a', 'b'] }},\n"))
        .collect();
    app.write(
        "src/heavy.ts",
        &format!("export const heavy = {{\n{props}}};\n"),
    );
    let mut checker = app.open();
    assert_eq!(text(&mut checker), "");

    // Each file is timed by its own emit: the heavy one is the slowest, whichever file comes first.
    let timed = {
        checker.record_signatures_slow_after(&[], std::time::Duration::from_secs(3600));
        checker.wait_for_signatures()
    };
    assert_eq!(timed.emitted.len(), 9, "{:?}", timed.emitted);
    assert!(timed.slow.is_empty(), "{:?}", timed.slow);
    let slowest = timed.emitted.iter().max_by_key(|(_, took)| *took).unwrap();
    assert!(slowest.0.ends_with("/src/heavy.ts"), "{:?}", timed.emitted);

    // A file at or over the threshold is remembered, and no later job emits it again. f7 sits this job out, so it
    // is not slow.
    let a = app.write("src/f0.ts", "export const f0 = 'zero';\n");
    checker.changed([a.as_path()]).unwrap();
    assert_eq!(text(&mut checker), fresh(&app));
    let f7 = app.0.join("src/f7.ts");
    checker.record_signatures_slow_after(std::slice::from_ref(&f7), std::time::Duration::ZERO);
    let job = checker.wait_for_signatures();
    let names: Vec<&str> = job.emitted.iter().map(|(f, _)| f.as_str()).collect();
    assert!(!names.is_empty());
    assert!(
        names.iter().all(|f| job.slow.iter().any(|s| s == f)),
        "{job:?}"
    );
    let b = app.write("src/f0.ts", "export const f0 = false;\n");
    checker.changed([b.as_path()]).unwrap();
    assert_eq!(text(&mut checker), fresh(&app));
    checker.record_signatures_slow_after(&[], std::time::Duration::from_secs(3600));
    let again = checker.wait_for_signatures();
    assert!(
        again.emitted.iter().any(|(f, _)| f.ends_with("/src/f7.ts")),
        "{again:?}"
    );
    assert!(
        again
            .emitted
            .iter()
            .all(|(f, _)| !names.contains(&f.as_str())),
        "{again:?}"
    );
}
