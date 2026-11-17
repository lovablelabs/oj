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
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use rolldown_plugin::{HookLoadArgs, HookLoadReturn, Plugin, SharedLoadPluginContext};

/// A file's size and modification time.
type Stat = (u64, SystemTime);

#[derive(Debug, Default)]
pub(crate) struct LoadedInputs {
    /// By module id, the stat of its file taken ahead of the read.
    stats: Mutex<HashMap<String, Stat>>,
}

impl LoadedInputs {
    /// Forgets every module: a build that starts over loads them all again.
    pub(crate) fn clear(&self) {
        self.lock().clear();
    }

    /// Notes the file of `id` as it is now, before the bundler reads it.
    pub(crate) fn record(&self, id: &str) {
        match stat(id) {
            Some(stat) => self.lock().insert(id.to_string(), stat),
            None => self.lock().remove(id),
        };
    }

    /// Whether the files of `ids` are still as they were when loaded. An id
    /// that never loaded from a file (a virtual module, a watch file) has no
    /// say.
    pub(crate) fn unchanged<'a>(&self, ids: impl Iterator<Item = &'a str>) -> bool {
        let stats = self.lock();
        ids.filter_map(|id| Some((id, stats.get(id)?)))
            .all(|(id, loaded)| stat(id).as_ref() == Some(loaded))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Stat>> {
        self.stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The stat of the file a module id names (its query aside, and the prefix
/// of oj's own wrapped ids, as `closure_files` reads them).
fn stat(id: &str) -> Option<Stat> {
    let id = match id.strip_prefix("\0oj-") {
        Some(rest) => rest.split_once(':')?.1,
        None => id,
    };
    let file = id.split('?').next().unwrap_or(id);
    let meta = std::fs::metadata(Path::new(file)).ok()?;
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

    // The stat a module had when it was loaded stands for the bytes the
    // bundle holds: a save after it (another size here, so the check does not
    // lean on the clock) means the bundle is no longer what the file says.
    #[test]
    fn a_file_saved_after_it_was_loaded_is_no_longer_as_built() {
        let dir = std::env::temp_dir().join(format!("oj-loaded-inputs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.ts").to_string_lossy().into_owned();
        let b = dir.join("b.ts").to_string_lossy().into_owned();
        std::fs::write(&a, "export const a = 1;\n").unwrap();
        std::fs::write(&b, "export const b = 1;\n").unwrap();
        let split = format!("{a}?tsr-split=component");
        let virtual_id = "\0virtual:thing";

        let loaded = LoadedInputs::default();
        for id in [a.as_str(), b.as_str(), split.as_str(), virtual_id] {
            loaded.record(id);
        }
        let ids = [a.as_str(), b.as_str(), split.as_str(), virtual_id];
        assert!(loaded.unchanged(ids.into_iter()));

        std::fs::write(&b, "export const b = 'saved later';\n").unwrap();
        assert!(!loaded.unchanged(ids.into_iter()), "b.ts moved");
        assert!(
            loaded.unchanged([a.as_str(), split.as_str()].into_iter()),
            "a bundle without b.ts is as built"
        );

        // Loaded again (a rebuild), it is the bundle's own once more.
        loaded.record(&b);
        assert!(loaded.unchanged(ids.into_iter()));

        std::fs::remove_file(&a).unwrap();
        assert!(!loaded.unchanged(ids.into_iter()), "a.ts is gone");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
