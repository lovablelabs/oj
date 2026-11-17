// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! What each module's file looked like when the bundler asked for it. The
//! bundle cache keys a bundle by the contents of the files it read, hashed
//! once the bundle is written: a file saved in between (after the bundler
//! read it, before the hash) would key the old code by the new contents, and
//! every later boot would restore that bundle for sources it was never built
//! from. A bundle whose files moved since they were loaded is not cached.
//! Vite keeps a transform only when its module was not invalidated while it
//! ran (`loadAndTransform`: `timestamp > mod.lastInvalidationTimestamp`).

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use rolldown_plugin::{HookLoadArgs, HookLoadReturn, Plugin, SharedLoadPluginContext};

/// A file's size and modification time.
type Stat = (u64, SystemTime);

/// By module id, its file and the stat taken ahead of the read.
pub(crate) type Notes = HashMap<String, (PathBuf, Stat)>;

/// The files one written bundle holds, each as it was when loaded.
pub(crate) type BuiltInputs = Vec<(PathBuf, Stat)>;

/// The notes of a running bundler. A hot engine keeps loading after an
/// output (it reloads a saved module for the next patch, through the same
/// plugins), so a bundle is judged by the [`snapshot`](Self::snapshot) taken
/// when its output was produced, never by this table as it is later.
#[derive(Debug)]
pub(crate) struct LoadedInputs {
    root: PathBuf,
    notes: Mutex<Notes>,
}

impl LoadedInputs {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            notes: Mutex::default(),
        }
    }

    /// Forgets every module: a build that starts over loads them all again.
    pub(crate) fn clear(&self) {
        self.lock().clear();
    }

    /// Notes the file of `id` as it is now, before the bundler reads it. An
    /// id that names no file (a virtual module) is not noted.
    pub(crate) fn record(&self, id: &str) {
        let noted = super::dev::module_file(&self.root, id).and_then(|file| {
            let stat = stat(&file)?;
            Some((file, stat))
        });
        match noted {
            Some(noted) => self.lock().insert(id.to_string(), noted),
            None => self.lock().remove(id),
        };
    }

    /// The notes as they are now: taken when an output is produced, they
    /// are what that output was built from.
    pub(crate) fn snapshot(&self) -> Notes {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Notes> {
        self.notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The noted files of `modules`, the modules of the chunks a bundle wrote.
/// A module with no note (a virtual one) has no say, and neither has a file
/// the bundle cache does not key by (`left_out`: oj's own cache files, which
/// oj rewrites itself, and the generated route tree, loaded from the client
/// tree in its place).
pub(crate) fn built_inputs<'a>(
    notes: &Notes,
    modules: impl Iterator<Item = &'a str>,
    left_out: impl Fn(&Path) -> bool,
) -> BuiltInputs {
    let mut built: BuiltInputs = modules
        .filter_map(|id| notes.get(id))
        .filter(|(file, _)| !left_out(file))
        .cloned()
        .collect();
    built.sort();
    built.dedup();
    built
}

/// Whether every file is still as it was when the bundler loaded it.
pub(crate) fn unchanged(built: &[(PathBuf, Stat)]) -> bool {
    built
        .iter()
        .all(|(file, loaded)| stat(file).as_ref() == Some(loaded))
}

fn stat(file: &Path) -> Option<Stat> {
    let meta = std::fs::metadata(file).ok()?;
    meta.is_file()
        .then_some((meta.len(), meta.modified().ok()?))
}

/// Records every module the bundler loads, ahead of whichever plugin (or
/// rolldown itself) reads it. It loads nothing.
#[derive(Debug)]
pub(crate) struct RecordLoadedInputs(pub std::sync::Arc<LoadedInputs>);

impl Plugin for RecordLoadedInputs {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:start-loaded-inputs")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::Load
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        self.0.record(args.id);
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("oj-loaded-inputs-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        dir
    }

    fn nothing_left_out(_: &Path) -> bool {
        false
    }

    // The stat a module had when it was loaded stands for the bytes the
    // bundle holds: a save after it (another size here, so the check does not
    // lean on the clock) means the bundle is no longer what the file says.
    #[test]
    fn a_file_saved_after_it_was_loaded_is_no_longer_as_built() {
        let root = dir("saved");
        let a = root.join("src/a.ts").to_string_lossy().into_owned();
        let b = root.join("src/b.ts").to_string_lossy().into_owned();
        std::fs::write(&a, "export const a = 1;\n").unwrap();
        std::fs::write(&b, "export const b = 1;\n").unwrap();
        let split = format!("{a}?tsr-split=component");
        let ids = [a.as_str(), b.as_str(), split.as_str(), "\0virtual:thing"];

        let loaded = LoadedInputs::new(&root);
        for id in ids {
            loaded.record(id);
        }
        let built =
            |ids: &[&str]| built_inputs(&loaded.snapshot(), ids.iter().copied(), nothing_left_out);
        let all = built(&ids);
        assert_eq!(all.len(), 2, "one entry per file: {all:?}");
        assert!(unchanged(&all));

        std::fs::write(&b, "export const b = 'saved later';\n").unwrap();
        assert!(!unchanged(&all), "b.ts moved");
        assert!(
            unchanged(&built(&[a.as_str(), split.as_str()])),
            "a bundle without b.ts is as built"
        );

        std::fs::remove_file(&a).unwrap();
        assert!(!unchanged(&built(&[a.as_str()])), "a.ts is gone");
        std::fs::remove_dir_all(&root).unwrap();
    }

    // A hot engine loads a saved module again right after it produced an
    // output, for the next patch. The output is judged by the notes taken
    // with it: by the table as it is afterwards, the save would compare
    // equal to itself and the old output would be cached as the new source.
    #[test]
    fn a_module_loaded_again_after_the_output_does_not_vouch_for_it() {
        let root = dir("reloaded");
        let a = root.join("src/a.ts").to_string_lossy().into_owned();
        std::fs::write(&a, "export const a = 1;\n").unwrap();
        let loaded = LoadedInputs::new(&root);
        loaded.record(&a);
        let with_output = loaded.snapshot();

        std::fs::write(&a, "export const a = 'saved after the output';\n").unwrap();
        loaded.record(&a);
        let ids = [a.as_str()];
        assert!(
            !unchanged(&built_inputs(
                &with_output,
                ids.into_iter(),
                nothing_left_out
            )),
            "the output predates the save"
        );
        assert!(unchanged(&built_inputs(
            &loaded.snapshot(),
            ids.into_iter(),
            nothing_left_out
        )));

        // A build that starts over forgets the table, not an output's notes.
        loaded.clear();
        assert!(!unchanged(&built_inputs(
            &with_output,
            ids.into_iter(),
            nothing_left_out
        )));
        std::fs::remove_dir_all(&root).unwrap();
    }

    // Ids are read as the closure reads them: relative to the app root, a
    // wrapped id by the file it wraps, a virtual id not at all. And a file
    // the cache does not key by is not judged: oj rewrites its own cache
    // files (manifest.ts) after the bundle loaded them.
    #[test]
    fn ids_are_files_as_the_closure_reads_them_and_unkeyed_files_have_no_say() {
        let root = dir("ids");
        let cache = root.join(".oj-cache/start");
        std::fs::create_dir_all(&cache).unwrap();
        let manifest = cache.join("manifest.ts");
        let tree = root.join("src/routeTree.gen.ts");
        std::fs::write(root.join("src/a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(&manifest, "export const m = 1;\n").unwrap();
        std::fs::write(&tree, "export const t = 1;\n").unwrap();
        // A file of the virtual id's bare name, in the working directory's
        // place: it is not the module.
        std::fs::write(root.join("virtual-thing"), "x").unwrap();
        let wrapped = format!("\0oj-raw:{}", root.join("src/a.ts").display());
        let manifest_id = manifest.to_string_lossy().into_owned();
        let tree_id = tree.to_string_lossy().into_owned();
        let ids = [
            "src/a.ts",
            wrapped.as_str(),
            "\0virtual-thing",
            manifest_id.as_str(),
            tree_id.as_str(),
        ];

        let loaded = LoadedInputs::new(&root);
        for id in ids {
            loaded.record(id);
        }
        let notes = loaded.snapshot();
        assert_eq!(notes.len(), 4, "the virtual id is not noted: {notes:?}");
        let left_out = crate::start_bundle::dev::left_out_of_closure(&cache);
        let built = built_inputs(&notes, ids.into_iter(), &left_out);
        assert_eq!(
            built.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>(),
            vec![root.join("src/a.ts")]
        );

        std::fs::write(&manifest, "export const m = 'rewritten by oj';\n").unwrap();
        std::fs::write(&tree, "export const t = 'regenerated';\n").unwrap();
        assert!(
            unchanged(&built),
            "unkeyed files moved, the bundle's did not"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
