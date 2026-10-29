use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::Arc;

// Go `Cloneable[T]`: the type's own `Clone()` method, which may differ from a field-by-field copy (e.g.
// `Project.Clone` resets `ProgramUpdateKind`).
pub trait Cloneable {
    #[must_use]
    fn clone_value(&self) -> Self;
}

// The Go pointer `*T` of a dirty-map value. Copying a `Shared` copies the pointer (equality is by address,
// like Go pointers); `clone_value` is Go's `Clone()`. Mutation goes through `make_mut`: a value that is still
// referenced elsewhere is copied first (copy-on-write), where Go would mutate the shared pointee in place. The
// builders only mutate values they have just cloned, so the copy is taken only when a caller still holds an
// older handle, which then keeps seeing the value it read (Go: a data race on that handle).
pub struct Shared<T>(Arc<T>);

impl<T> Shared<T> {
    pub fn new(value: T) -> Shared<T> {
        Shared(Arc::new(value))
    }

    pub fn ptr_eq(a: &Shared<T>, b: &Shared<T>) -> bool {
        Arc::ptr_eq(&a.0, &b.0)
    }

    pub fn arc(&self) -> &Arc<T> {
        &self.0
    }

    pub(crate) fn make_mut(this: &mut Shared<T>) -> &mut T
    where
        T: Clone,
    {
        Arc::make_mut(&mut this.0)
    }
}

impl<T: Cloneable> Shared<T> {
    // Go `v.Clone()` on the pointer.
    #[must_use]
    pub(crate) fn clone_shared(&self) -> Shared<T> {
        Shared(Arc::new(self.0.clone_value()))
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Shared<T> {
        Shared(Arc::clone(&self.0))
    }
}

impl<T> Deref for Shared<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> PartialEq for Shared<T> {
    fn eq(&self, other: &Shared<T>) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<T> Eq for Shared<T> {}

impl<T> Hash for Shared<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

// Go `Value[T]` (T = *X): implemented by map entries, boxes and the locked view of a sync map entry. Callbacks
// receive the pointee; `cond` is never called for a nil (deleted) value and the result is then false.
pub trait Value<T> {
    fn value(&self) -> Option<Shared<T>>;
    fn original(&self) -> Option<Shared<T>>;
    fn dirty(&self) -> bool;
    fn change(&self, apply: &mut dyn FnMut(&mut T));
    fn change_if(&self, cond: &mut dyn FnMut(&T) -> bool, apply: &mut dyn FnMut(&mut T)) -> bool;
    fn delete(&self);
    fn locked(&self, f: &mut dyn FnMut(&dyn Value<T>));
}
