use std::sync::Mutex;

use super::interfaces::{Cloneable, Shared, Value};

// Go `Box[T]` has no synchronization; the mutex only provides interior mutability and is never held while a
// callback runs (a callback may use the box again, as in Go).
pub struct Box<T> {
    st: Mutex<BoxState<T>>,
}

struct BoxState<T> {
    original: Option<Shared<T>>,
    value: Option<Shared<T>>,
    dirty: bool,
    delete: bool,
}

// box.go:10
pub fn new_box<T>(original: Option<Shared<T>>) -> Box<T> {
    Box {
        st: Mutex::new(BoxState {
            original: original.clone(),
            value: original,
            dirty: false,
            delete: false,
        }),
    }
}

impl<T: Cloneable + Clone> Box<T> {
    // box.go:30
    pub fn set(&self, value: Option<Shared<T>>) {
        let mut st = self.st.lock().unwrap();
        st.value = value;
        st.delete = false;
        st.dirty = true;
    }

    // box.go:60
    pub fn finalize(&self) -> (Option<Shared<T>>, bool) {
        let value = self.value();
        let st = self.st.lock().unwrap();
        (value, st.dirty || st.delete)
    }
}

impl<T: Cloneable + Clone> Value<T> for Box<T> {
    // box.go:14
    fn value(&self) -> Option<Shared<T>> {
        let st = self.st.lock().unwrap();
        if st.delete {
            return None;
        }
        st.value.clone()
    }

    // box.go:22
    fn original(&self) -> Option<Shared<T>> {
        self.st.lock().unwrap().original.clone()
    }

    // box.go:26
    fn dirty(&self) -> bool {
        self.st.lock().unwrap().dirty
    }

    // box.go:36
    fn change(&self, apply: &mut dyn FnMut(&mut T)) {
        let mut value = {
            let mut st = self.st.lock().unwrap();
            if !st.dirty {
                st.value = Some(st.value.as_ref().unwrap().clone_shared());
                st.dirty = true;
            }
            st.value.take().unwrap()
        };
        apply(Shared::make_mut(&mut value));
        self.st.lock().unwrap().value = Some(value);
    }

    // box.go:44
    fn change_if(&self, cond: &mut dyn FnMut(&T) -> bool, apply: &mut dyn FnMut(&mut T)) -> bool {
        let value = self.st.lock().unwrap().value.clone();
        let Some(value) = value else {
            return false;
        };
        if cond(&value) {
            drop(value);
            self.change(apply);
            return true;
        }
        false
    }

    // box.go:52
    fn delete(&self) {
        self.st.lock().unwrap().delete = true;
    }

    // box.go:56
    fn locked(&self, f: &mut dyn FnMut(&dyn Value<T>)) {
        f(self);
    }
}
