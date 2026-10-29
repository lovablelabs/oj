use std::cell::{Cell, RefCell};

// Sequential stand-in for Go's WorkGroup: queued functions run on the calling thread in
// RunAndWait, last-queued first (like Go's single-threaded work group). Functions may queue more work.
pub struct WorkGroup<'a> {
    done: Cell<bool>,
    fns: RefCell<Vec<Box<dyn FnOnce() + 'a>>>,
}

pub fn new_work_group<'a>(_single_threaded: bool) -> WorkGroup<'a> {
    WorkGroup { done: Cell::new(false), fns: RefCell::new(Vec::new()) }
}

impl<'a> WorkGroup<'a> {
    pub fn queue(&self, f: impl FnOnce() + 'a) {
        assert!(!self.done.get(), "Queue called after RunAndWait returned");
        self.fns.borrow_mut().push(Box::new(f));
    }

    pub fn run_and_wait(&self) {
        loop {
            let f = self.fns.borrow_mut().pop();
            match f {
                None => break,
                Some(f) => f(),
            }
        }
        self.done.set(true);
    }
}
