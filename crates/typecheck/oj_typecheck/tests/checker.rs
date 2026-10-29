// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};

use oj_typecheck::Checker;

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
