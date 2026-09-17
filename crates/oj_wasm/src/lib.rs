// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj_wasm`: the oj compile pipeline in the browser. A project is a set of
//! in-memory files; `build()` walks the module graph from `/index.html` and
//! returns transformed ES modules as JSON for the host page to serve through
//! an import map of Blob urls.

pub mod project;

use std::collections::BTreeMap;

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct OjProject {
    files: BTreeMap<String, String>,
}

impl Default for OjProject {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl OjProject {
    #[wasm_bindgen(constructor)]
    pub fn new() -> OjProject {
        OjProject { files: BTreeMap::new() }
    }

    #[wasm_bindgen(js_name = writeFile)]
    pub fn write_file(&mut self, path: &str, contents: &str) {
        self.files.insert(project::normalize_abs(path), contents.to_string());
    }

    #[wasm_bindgen(js_name = removeFile)]
    pub fn remove_file(&mut self, path: &str) -> bool {
        self.files.remove(&project::normalize_abs(path)).is_some()
    }

    /// Compile the whole project; returns a `BuildResult` as a JSON string.
    pub fn build(&self) -> String {
        serde_json::to_string(&project::build(&self.files)).expect("build result serializes")
    }
}

#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
