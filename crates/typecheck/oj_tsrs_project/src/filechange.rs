use tsrs_core::collections::Set;
use tsrs_lsproto as lsproto;

const EXCESSIVE_CHANGE_THRESHOLD: usize = 1000;

pub trait FileChangeExpander {
    fn expand_file_changes(&self, summary: FileChangeSummary) -> FileChangeSummary;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum FileChangeKind {
    #[default]
    Open,
    Close,
    Change,
    Save,
    WatchCreate,
    WatchChange,
    WatchDelete,
}

impl FileChangeKind {
    // filechange.go:26
    pub(crate) fn is_watch_kind(self) -> bool {
        self == FileChangeKind::WatchCreate
            || self == FileChangeKind::WatchChange
            || self == FileChangeKind::WatchDelete
    }
}

#[derive(Clone, Debug, Default)]
pub struct FileChange {
    pub kind: FileChangeKind,
    pub uri: lsproto::DocumentUri,
    pub version: i32,                         // Only set for Open/Change
    pub content: String,                      // Only set for Open
    pub language_kind: lsproto::LanguageKind, // Only set for Open
    pub changes: Vec<lsproto::TextDocumentContentChangePartialOrWholeDocument>, // Only set for Change
}

#[derive(Clone, Debug, Default)]
pub struct FileChangeSummary {
    // Only one file can be opened at a time per request
    pub opened: lsproto::DocumentUri,
    // Reopened is set if a close and open occurred for the same file in a single batch of changes.
    pub(crate) reopened: lsproto::DocumentUri,
    pub closed: Set<lsproto::DocumentUri>,
    pub changed: Set<lsproto::DocumentUri>,
    // Only set when file watching is enabled
    pub created: Set<lsproto::DocumentUri>,
    // Only set when file watching is enabled
    pub deleted: Set<lsproto::DocumentUri>,

    // IncludesWatchChangeOutsideNodeModules is true if the summary includes a create, change, or delete watch
    // event of a file outside a node_modules directory.
    pub includes_watch_change_outside_node_modules: bool,
    // InvalidateAll indicates that all cached file state should be discarded.
    pub invalidate_all: bool,
}

impl FileChangeSummary {
    // filechange.go:58 (Go's value receiver copies the struct and clones the sets; Rust `clone` does both)
    #[must_use]
    pub(crate) fn clone_summary(&self) -> FileChangeSummary {
        self.clone()
    }

    // filechange.go:66
    pub fn is_empty(&self) -> bool {
        !self.invalidate_all
            && self.opened.0.is_empty()
            && self.reopened.0.is_empty()
            && self.closed.len() == 0
            && self.changed.len() == 0
            && self.created.len() == 0
            && self.deleted.len() == 0
    }

    // filechange.go:70
    pub(crate) fn has_excessive_watch_events(&self) -> bool {
        self.invalidate_all
            || self.created.len() + self.deleted.len() + self.changed.len()
                > EXCESSIVE_CHANGE_THRESHOLD
    }

    // filechange.go:74
    pub(crate) fn has_excessive_non_create_watch_events(&self) -> bool {
        self.invalidate_all || self.deleted.len() + self.changed.len() > EXCESSIVE_CHANGE_THRESHOLD
    }
}

// filechange.go:79
// mergeFileChangeSummary merges src into dst, combining their change sets.
pub(crate) fn merge_file_change_summary(dst: &mut FileChangeSummary, src: &FileChangeSummary) {
    if src.is_empty() {
        return;
    }
    if src.invalidate_all {
        dst.invalidate_all = true;
    }
    // pure set union into dst; Go ranges the set too
    dst.changed.m.extend(src.changed.keys().iter().map(std::clone::Clone::clone));
    // pure set union into dst; Go ranges the set too
    dst.created.m.extend(src.created.keys().iter().map(std::clone::Clone::clone));
    // pure set union into dst; Go ranges the set too
    dst.deleted.m.extend(src.deleted.keys().iter().map(std::clone::Clone::clone));
    if src.includes_watch_change_outside_node_modules {
        dst.includes_watch_change_outside_node_modules = true;
    }
}
