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
fn changes_to_files_no_program_reads_keep_the_snapshot() {
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
