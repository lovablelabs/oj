use std::cell::Cell;

#[derive(Clone, Copy)]
pub enum PathBuffer {
    Extension,
    Suffix,
    PackageJson,
}

thread_local! {
    static PATHS: [Cell<String>; 3] = const { [const { Cell::new(String::new()) }; 3] };
}

// Like oxc-resolver's scratch paths, keep capacity across requests on each worker. Take the buffer out before
// calling the host: a custom filesystem may resolve another module on this same thread. The nested request
// then owns a different buffer, and neither call holds a TLS borrow across filesystem or resolver callbacks.
pub fn with_path_buffer<T>(kind: PathBuffer, f: impl FnOnce(&mut String) -> T) -> T {
    PATHS.with(|paths| {
        let slot = &paths[kind as usize];
        let mut path = slot.take();
        path.clear();
        if path.capacity() == 0 {
            path.reserve(256);
        }
        let result = f(&mut path);
        if path.capacity() <= 64 * 1024 {
            path.clear();
            slot.set(path);
        }
        result
    })
}
