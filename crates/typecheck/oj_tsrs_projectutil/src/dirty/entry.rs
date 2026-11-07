use super::interfaces::Shared;

// Go `mapEntry[K, V]`, embedded in `MapEntry` and `SyncMapEntry` (their state lives behind each entry's lock).
pub struct EntryState<T> {
    pub(crate) original: Option<Shared<T>>,
    pub(crate) value: Option<Shared<T>>,
    pub(crate) dirty: bool,
    pub(crate) delete: bool,
}

impl<T> EntryState<T> {
    // entry.go:15
    pub(crate) fn original(&self) -> Option<Shared<T>> {
        self.original.clone()
    }

    // entry.go:19
    pub(crate) fn value(&self) -> Option<Shared<T>> {
        if self.delete {
            return None;
        }
        self.value.clone()
    }

    // entry.go:27
    pub(crate) fn dirty(&self) -> bool {
        self.dirty
    }
}
