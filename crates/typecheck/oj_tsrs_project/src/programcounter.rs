use std::collections::hash_map::Entry;
use std::sync::Mutex;

use rustc_hash::FxHashMap;
use tsrs_compiler::Program;

// Programs are keyed by address.
#[derive(Default)]
pub struct ProgramCounter {
    mu: Mutex<FxHashMap<usize, usize>>,
}

fn key(program: &'static Program) -> usize {
    std::ptr::from_ref::<Program>(program) as usize
}

impl ProgramCounter {
    // programcounter.go:16
    // Ref increments the reference count for a program. If the program is not
    // yet tracked, it is added with a reference count of 1.
    pub(crate) fn ref_(&self, program: &'static Program) {
        *self.mu.lock().unwrap().entry(key(program)).or_insert(0) += 1;
    }

    // programcounter.go:25
    pub(crate) fn deref(&self, program: &'static Program) -> bool {
        let mut refs = self.mu.lock().unwrap();
        let Entry::Occupied(mut entry) = refs.entry(key(program)) else {
            return false;
        };
        let count = entry.get().checked_sub(1).expect("program reference count went below zero");
        if count == 0 {
            entry.remove();
            return true;
        }
        *entry.get_mut() = count;
        false
    }

    // programcounter.go:44
    pub fn len(&self) -> usize {
        self.mu.lock().unwrap().len()
    }
}
