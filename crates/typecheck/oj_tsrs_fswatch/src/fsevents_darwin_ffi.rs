// fsevents_darwin_ffi.go: macOS CoreFoundation / CoreServices / libdispatch FFI.
//
// Go reaches these C functions without cgo (`//go:cgo_import_dynamic` + assembly trampolines) and delivers the
// FSEvents callback through an assembly function that copies the payload into a pipe read by a Go goroutine,
// because Go code cannot run on a foreign (GCD) thread without cgo. Rust calls the same C API directly through
// `extern "C"` declarations (the frameworks are linked with `#[link(kind = "framework")]`), and the callback is
// an `extern "C" fn` that runs the event classification (`fs_events_callback`) on the stream's serial GCD queue
// thread itself; the pipe and event-loop goroutine are not needed. Teardown keeps Go's order (stop, invalidate,
// wait for the queue to drain, release), which still guarantees no callback runs after `teardown_stream`.
//
// All `unsafe` of the FSEvents backend is confined to this file: the C declarations, the wrappers below (each
// documents the invariant it relies on) and the callback.

use std::ffi::{CString, c_char, c_void};

use crate::fsevents_darwin::{FseventsWatchSnapshot, fs_events_callback};

type CFTypeRef = *const c_void;
type CFIndex = isize;
type Boolean = u8;
type FSEventStreamRef = *mut c_void;
type DispatchQueueT = *mut c_void;

// FSEventStreamContext mirrors the C struct of the same name.
#[repr(C)]
struct FSEventStreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

type FSEventStreamCallback = extern "C" fn(
    stream: FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    event_ids: *const u64,
);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    #[link_name = "CFRelease"]
    fn cf_release(cf: CFTypeRef);
    #[link_name = "CFStringCreateWithCString"]
    fn cf_string_create_with_c_string(
        alloc: CFTypeRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFTypeRef;
    #[link_name = "CFArrayCreate"]
    fn native_cf_array_create(
        alloc: CFTypeRef,
        values: *const CFTypeRef,
        num_values: CFIndex,
        callbacks: *const c_void,
    ) -> CFTypeRef;
    #[link_name = "CFArrayGetValueAtIndex"]
    fn cf_array_get_value_at_index(array: CFTypeRef, idx: CFIndex) -> CFTypeRef;
    #[link_name = "CFStringCreateMutableCopy"]
    fn native_cf_string_create_mutable_copy(
        alloc: CFTypeRef,
        max_length: CFIndex,
        string: CFTypeRef,
    ) -> CFTypeRef;
    #[link_name = "CFStringNormalize"]
    fn native_cf_string_normalize(string: CFTypeRef, the_form: CFIndex);
    #[link_name = "CFStringFold"]
    fn cf_string_fold(string: CFTypeRef, the_flags: usize, the_locale: CFTypeRef);
    #[link_name = "CFStringGetLength"]
    fn cf_string_get_length(string: CFTypeRef) -> CFIndex;
    #[link_name = "CFStringGetMaximumSizeForEncoding"]
    fn cf_string_get_maximum_size_for_encoding(length: CFIndex, encoding: u32) -> CFIndex;
    #[link_name = "CFStringGetCString"]
    fn cf_string_get_c_string(
        string: CFTypeRef,
        buffer: *mut c_char,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> Boolean;
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    #[link_name = "FSEventStreamCreate"]
    fn fs_event_stream_create(
        allocator: CFTypeRef,
        callback: FSEventStreamCallback,
        context: *const FSEventStreamContext,
        paths_to_watch: CFTypeRef,
        since_when: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    #[link_name = "FSEventStreamSetDispatchQueue"]
    fn fs_event_stream_set_dispatch_queue(stream: FSEventStreamRef, q: DispatchQueueT);
    #[link_name = "FSEventStreamStart"]
    fn fs_event_stream_start(stream: FSEventStreamRef) -> Boolean;
    #[link_name = "FSEventStreamFlushSync"]
    fn fs_event_stream_flush_sync(stream: FSEventStreamRef);
    #[link_name = "FSEventsGetCurrentEventId"]
    fn native_fs_events_get_current_event_id() -> u64;
    #[link_name = "FSEventStreamStop"]
    fn fs_event_stream_stop(stream: FSEventStreamRef);
    #[link_name = "FSEventStreamInvalidate"]
    fn fs_event_stream_invalidate(stream: FSEventStreamRef);
    #[link_name = "FSEventStreamRelease"]
    fn fs_event_stream_release(stream: FSEventStreamRef);
}

// libSystem (always linked).
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueueT;
    fn dispatch_queue_attr_make_with_qos_class(
        attr: *const c_void,
        qos_class: u32,
        relative_priority: i32,
    ) -> *const c_void;
    fn dispatch_release(object: *mut c_void);
    fn dispatch_sync_f(
        queue: DispatchQueueT,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const CF_STRING_NORMALIZATION_FORM_C: CFIndex = 2; // kCFStringNormalizationFormC
// kFSEventStreamCreateFlagUseCFTypes (0x1) | kFSEventStreamCreateFlagFileEvents (0x10); Go hardcodes it in the
// assembly trampolines.
const FS_EVENT_STREAM_CREATE_FLAGS: u32 = 0x11;

// An owned CoreFoundation object reference (released on drop).
pub struct CFRef(CFTypeRef);

impl Drop for CFRef {
    fn drop(&mut self) {
        // SAFETY: `CFRef` is only constructed from a non-NULL object returned by a CF "Create"/"Copy" function,
        // which the caller owns exactly once.
        unsafe { cf_release(self.0) }
    }
}

// Go `cfStringCreate(0, cstr, UTF8)`; None when CFStringCreateWithCString returns NULL (or `s` contains NUL).
fn cf_string_create(s: &str) -> Option<CFRef> {
    let cstr = CString::new(s).ok()?;
    // SAFETY: `cstr` is NUL-terminated and outlives the call; NULL allocator = default.
    let r = unsafe {
        cf_string_create_with_c_string(std::ptr::null(), cstr.as_ptr(), CF_STRING_ENCODING_UTF8)
    };
    (!r.is_null()).then_some(CFRef(r))
}

// Go `cfArrayCreate(0, values, n, 0)`: NULL callbacks, so the array does not retain its values; the caller keeps
// `values` alive while the array is used (Go's `defer cfRelease` order).
fn cf_array_create(values: &[CFRef]) -> Option<CFRef> {
    let ptrs: Vec<CFTypeRef> = values.iter().map(|v| v.0).collect();
    // SAFETY: `ptrs` holds `ptrs.len()` valid CF objects for the duration of the call.
    let r = unsafe {
        native_cf_array_create(
            std::ptr::null(),
            ptrs.as_ptr(),
            ptrs.len() as CFIndex,
            std::ptr::null(),
        )
    };
    (!r.is_null()).then_some(CFRef(r))
}

fn cf_string_create_mutable_copy(src: CFTypeRef) -> Option<CFRef> {
    // SAFETY: `src` is a valid CFString (callers pass objects they own or borrow for the call).
    let r = unsafe { native_cf_string_create_mutable_copy(std::ptr::null(), 0, src) };
    (!r.is_null()).then_some(CFRef(r))
}

fn cf_string_normalize(mut_str: &CFRef, form: CFIndex) {
    // SAFETY: `mut_str` is a CFMutableString created by `cf_string_create_mutable_copy`.
    unsafe { native_cf_string_normalize(mut_str.0, form) }
}

pub const NATIVE_PATH_FOLDING: bool = true;

// fsevents_darwin_ffi.go:206
// foldNativePath is a comparison form, never a displayed or opened path.
// Case folding expands sharp s and ligatures without making diacritics,
// dotless i, circled letters, or character widths interchangeable.
pub(crate) fn fold_native_path(s: &str) -> String {
    const CF_COMPARE_CASE_INSENSITIVE: usize = 1;

    if is_ascii(s) {
        return s.to_ascii_lowercase();
    }
    if s.contains('\0') {
        return String::new();
    }
    let Some(src) = cf_string_create(s) else {
        panic!("fswatch: cannot create CFString for path folding");
    };
    let Some(mut_) = cf_string_create_mutable_copy(src.0) else {
        panic!("fswatch: cannot copy CFString for path folding");
    };
    // Normalize before folding as well: a decomposed capital I with dot
    // must have the same comparison form as precomposed dotted capital I.
    cf_string_normalize(&mut_, CF_STRING_NORMALIZATION_FORM_C);
    // SAFETY: `mut_` is a CFMutableString; NULL locale = canonical folding.
    unsafe { cf_string_fold(mut_.0, CF_COMPARE_CASE_INSENSITIVE, std::ptr::null()) };
    cf_string_normalize(&mut_, CF_STRING_NORMALIZATION_FORM_C);
    let folded = cf_string_to_go(mut_.0);
    assert!(!folded.is_empty(), "fswatch: cannot extract folded CFString");
    folded
}

// fsevents_darwin_ffi.go:271
// isASCII reports whether every byte in s is below 0x80. Pure-ASCII paths
// are identical in every Unicode normalization form, so we can skip the
// CoreFoundation round-trip entirely, which is the overwhelming common case.
fn is_ascii(s: &str) -> bool {
    s.is_ascii()
}

// fsevents_darwin_ffi.go:284
// cfStringToNFC returns the CFString at src as a NFC-normalized Go string.
// If normalization fails, it falls back to the unnormalized UTF-8 contents.
// Returns "" only if both the normalized and unnormalized conversions fail
// (e.g. src is not a CFString, or allocation fails).
fn cf_string_to_nfc(src: CFTypeRef) -> String {
    if src.is_null() {
        return String::new();
    }
    let s = cf_string_normalized_to_go(src);
    if !s.is_empty() {
        return s;
    }
    cf_string_to_go(src)
}

// fsevents_darwin_ffi.go:296
// cfStringNormalizedToGo returns the CFString at src as a NFC-normalized Go
// string, or "" on any failure.
fn cf_string_normalized_to_go(src: CFTypeRef) -> String {
    let Some(mut_) = cf_string_create_mutable_copy(src) else {
        return String::new();
    };
    cf_string_normalize(&mut_, CF_STRING_NORMALIZATION_FORM_C);
    cf_string_to_go(mut_.0)
}

// fsevents_darwin_ffi.go:309
// cfStringToGo extracts the UTF-8 contents of the CFString at src as a Go
// string, or "" on failure.
fn cf_string_to_go(src: CFTypeRef) -> String {
    // SAFETY: `src` is a valid CFString for the duration of these calls; `buf` has `buf_size` bytes.
    unsafe {
        let length = cf_string_get_length(src);
        let buf_size = cf_string_get_maximum_size_for_encoding(length, CF_STRING_ENCODING_UTF8) + 1;
        let mut buf = vec![0u8; buf_size as usize];
        if cf_string_get_c_string(
            src,
            buf.as_mut_ptr().cast::<c_char>(),
            buf_size,
            CF_STRING_ENCODING_UTF8,
        ) == 0
        {
            return String::new();
        }
        // CFStringGetCString writes a NUL terminator; trim it.
        let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(n);
        tsrs_core::utf8::into_string(buf).unwrap_or_default()
    }
}

// fsevents_darwin_ffi.go:330
// normalizeNFC returns s in Unicode NFC (canonical composed) form. ASCII
// inputs are returned unchanged. Non-ASCII inputs go through CoreFoundation;
// if any step fails (e.g. invalid UTF-8 from a corrupt path), the original
// string is returned so the caller still sees *something* rather than nothing.
pub(crate) fn normalize_nfc(s: &str) -> String {
    if is_ascii(s) {
        return s.to_string();
    }
    let Some(src) = cf_string_create(s) else {
        return s.to_string();
    };
    let normalized = cf_string_to_nfc(src.0);
    if normalized.is_empty() {
        return s.to_string();
    }
    normalized
}

// Go `fsEventsGetCurrentEventID`.
pub(crate) fn fs_events_get_current_event_id() -> u64 {
    // SAFETY: no arguments, no preconditions.
    unsafe { native_fs_events_get_current_event_id() }
}

// The borrowed arguments of one FSEvents callback (Go's retained/copied `fsEventsCallbackPayload`; here the
// callback runs synchronously, so nothing is retained or freed).
pub(crate) struct FsEventsCallbackPayload {
    pub(crate) num_events: usize,
    paths: CFTypeRef,
    flags: *const u32,
    ids: *const u64,
}

impl FsEventsCallbackPayload {
    pub(crate) fn is_empty(&self) -> bool {
        self.paths.is_null() || self.flags.is_null() || self.ids.is_null()
    }

    pub(crate) fn flag(&self, i: usize) -> u32 {
        // SAFETY: FSEvents passes `num_events` flags; callers index below `num_events`.
        unsafe { *self.flags.add(i) }
    }

    pub(crate) fn id(&self, i: usize) -> u64 {
        // SAFETY: as `flag`.
        unsafe { *self.ids.add(i) }
    }

    // Go `cfStringToNFC(cfArrayGetValueAtIndex(paths, i))`.
    pub(crate) fn path_nfc(&self, i: usize) -> String {
        // SAFETY: with kFSEventStreamCreateFlagUseCFTypes `paths` is a CFArray of `num_events` CFStrings, valid for
        // the duration of the callback.
        let path_ref = unsafe { cf_array_get_value_at_index(self.paths, i as CFIndex) };
        cf_string_to_nfc(path_ref)
    }
}

// Keep the borrowed CF objects and arrays alive while a test exercises the real event classifier.
pub fn dispatch_test_event(cb: &StreamCallback, path: &str, flags: u32) {
    let path = cf_string_create(path).unwrap();
    let paths = cf_array_create(std::slice::from_ref(&path)).unwrap();
    let id = 1;
    let payload = FsEventsCallbackPayload {
        num_events: 1,
        paths: paths.0,
        flags: &raw const flags,
        ids: &raw const id,
    };
    fs_events_callback(cb, &payload);
}

// streamCallback is the per-stream state the C callback receives as FSEventStreamContext.info.
pub struct StreamCallback {
    queue: DispatchQueueT, // per-stream serial dispatch queue
    pub(crate) watches: Vec<FseventsWatchSnapshot>,
}

// SAFETY: the dispatch queue handle is a thread-safe libdispatch object; `watches` is immutable after creation.
unsafe impl Send for StreamCallback {}
// SAFETY: as for Send: the queue handle is thread-safe and `watches` is never mutated after creation.
unsafe impl Sync for StreamCallback {}

// fsevents_darwin_ffi.go:536
// newStreamCallback allocates a streamCallback with its own per-stream serial dispatch queue. The per-stream
// serial queue serializes this stream's callbacks and prevents cross-stream head-of-line blocking that a
// process-wide serial queue would cause.
//
// tsrs-only: the queue has an explicit QoS class. Go passes a NULL attribute, which leaves the QoS unspecified; while
// every core is busy (a watch cycle's checkers and emit) such a queue is starved, fseventsd's buffer for the client
// fills, and it drops events (kFSEventStreamEventFlagUserDropped | MustScanSubDirs), which the watch turns into an
// overflow and a full rebuild. Writing 10k files with all cores busy dropped events on every run with a NULL
// attribute and on none with QOS_CLASS_DEFAULT or QOS_CLASS_USER_INITIATED. The callback is short (tens of
// microseconds per batch), so running it ahead of the compiler's threads costs nothing measurable.
pub fn new_stream_callback(watches: &[FseventsWatchSnapshot]) -> Option<Box<StreamCallback>> {
    const QOS_CLASS_USER_INITIATED: u32 = 0x19;

    let label = b"typescript.fswatch.fsevents.stream\0";
    // SAFETY: NULL = DISPATCH_QUEUE_SERIAL; the returned attribute is a static libdispatch object (not released).
    let attr = unsafe {
        dispatch_queue_attr_make_with_qos_class(std::ptr::null(), QOS_CLASS_USER_INITIATED, 0)
    };
    // SAFETY: `label` is NUL-terminated; `attr` is a serial queue attribute.
    let queue = unsafe { dispatch_queue_create(label.as_ptr().cast::<c_char>(), attr) };
    if queue.is_null() {
        return None;
    }
    Some(Box::new(StreamCallback { queue, watches: watches.to_vec() }))
}

extern "C" fn dispatch_noop(_: *mut c_void) {}

impl StreamCallback {
    // fsevents_darwin_ffi.go:565
    fn wait_dispatch_queue(&self) {
        if !self.queue.is_null() {
            // SAFETY: `queue` is a live queue owned by this callback; the work function does nothing.
            unsafe { dispatch_sync_f(self.queue, std::ptr::null_mut(), dispatch_noop) }
        }
    }

    // fsevents_darwin_ffi.go:572
    // close releases resources.
    fn close(&mut self) {
        if !self.queue.is_null() {
            // SAFETY: the queue was created by `new_stream_callback` and is released exactly once.
            unsafe { dispatch_release(self.queue) };
            self.queue = std::ptr::null_mut();
        }
    }
}

extern "C" fn fs_events_callback_c(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    event_ids: *const u64,
) {
    // SAFETY: `info` is the `streamCallback` passed to FSEventStreamCreate; it stays alive until
    // `teardown_stream` has waited for the stream's queue, so no callback outlives it.
    let cb = unsafe { &*(info as *const StreamCallback) };
    let payload = FsEventsCallbackPayload {
        num_events,
        paths: event_paths.cast_const(),
        flags: event_flags,
        ids: event_ids,
    };
    // A panic must not unwind into CoreServices.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fs_events_callback(cb, &payload);
    }));
}

// An FSEventStream handle (Go keeps it as an atomic uintptr).
pub struct FsEventStream(FSEventStreamRef);

// SAFETY: FSEventStream functions may be called from any thread once the stream is scheduled on a dispatch queue.
unsafe impl Send for FsEventStream {}
// SAFETY: as for Send: the stream functions are thread-safe once it is scheduled on a dispatch queue.
unsafe impl Sync for FsEventStream {}

#[derive(Clone, Copy)]
pub enum StreamStartError {
    CFStringCreateNull,
    CFArrayCreateNull,
    StreamCreateNull,
    StreamStartFailed,
}

// The FFI part of fsevents_darwin.go `startStream`: create the CF path array and the stream, schedule it on the
// callback's queue, start and flush it. On failure the callback is closed (Go: `cb.close()`).
pub(crate) fn create_and_start_stream(
    paths: &[String],
    cb: &mut Box<StreamCallback>,
) -> Result<FsEventStream, StreamStartError> {
    // kFSEventStreamEventIdSinceNow == ((FSEventStreamEventId)0xFFFFFFFFFFFFFFFFULL)
    const EVENT_ID_SINCE_NOW: u64 = 0xFFFF_FFFF_FFFF_FFFF;

    let mut cf_strings = Vec::with_capacity(paths.len());
    cf_strings = match paths.iter().try_fold(cf_strings, |mut cf_strings, path| {
        match cf_string_create(path) {
            Some(s) => cf_strings.push(s),
            None => return std::ops::ControlFlow::Break(Err(StreamStartError::CFStringCreateNull)),
        }

        std::ops::ControlFlow::Continue(cf_strings)
    }) {
        std::ops::ControlFlow::Continue(value) => value,
        std::ops::ControlFlow::Break(value) => return value,
    };
    let Some(paths_to_watch) = cf_array_create(&cf_strings) else {
        return Err(StreamStartError::CFArrayCreateNull);
    };

    let info: *mut StreamCallback = &raw mut **cb;
    let ctx = FSEventStreamContext {
        version: 0,
        info: info.cast::<c_void>(),
        retain: std::ptr::null(),
        release: std::ptr::null(),
        copy_description: std::ptr::null(),
    };
    // SAFETY: `ctx` is copied by FSEventStreamCreate; `paths_to_watch` is a valid CFArray of CFStrings; `info`
    // points into the boxed callback, whose address is stable until the stream is torn down.
    let stream = unsafe {
        fs_event_stream_create(
            std::ptr::null(),
            fs_events_callback_c,
            &raw const ctx,
            paths_to_watch.0,
            EVENT_ID_SINCE_NOW,
            0.001,
            FS_EVENT_STREAM_CREATE_FLAGS,
        )
    };
    drop(paths_to_watch);
    drop(cf_strings);
    if stream.is_null() {
        cb.close();
        return Err(StreamStartError::StreamCreateNull);
    }

    // SAFETY: `stream` was just created; `cb.queue` is a live serial queue.
    unsafe {
        fs_event_stream_set_dispatch_queue(stream, cb.queue);
        if fs_event_stream_start(stream) == 0 {
            fs_event_stream_invalidate(stream);
            fs_event_stream_release(stream);
            cb.close();
            return Err(StreamStartError::StreamStartFailed);
        }
        fs_event_stream_flush_sync(stream);
    }
    Ok(FsEventStream(stream))
}

// fsevents_darwin.go:350
// teardownStream performs the full FSEventStream cleanup. Stop and Invalidate
// prevent new callbacks, waitDispatchQueue waits for callbacks already queued
// on the stream's serial dispatch queue.
#[expect(
    clippy::needless_pass_by_value,
    reason = "takes the stream by value: it is torn down exactly once and must not be used after"
)]
pub fn teardown_stream(stream: FsEventStream, cb: Option<&mut Box<StreamCallback>>) {
    // SAFETY: `stream` is a started stream that is torn down exactly once (the caller swapped it out).
    unsafe {
        fs_event_stream_stop(stream.0);
        match cb {
            Some(cb) => {
                fs_event_stream_invalidate(stream.0);
                cb.wait_dispatch_queue();
                cb.close();
            }
            None => fs_event_stream_invalidate(stream.0),
        }
        fs_event_stream_release(stream.0);
    }
}

pub fn dispatch_test_events(cb: &StreamCallback, events: &[(&str, u32)]) {
    let path_refs: Vec<_> =
        events.iter().map(|(path, _)| cf_string_create(path).unwrap()).collect();
    let paths = cf_array_create(&path_refs).unwrap();
    let flags: Vec<_> = events.iter().map(|(_, flags)| *flags).collect();
    let ids: Vec<_> = (1..=events.len() as u64).collect();
    let payload = FsEventsCallbackPayload {
        num_events: events.len(),
        paths: paths.0,
        flags: flags.as_ptr(),
        ids: ids.as_ptr(),
    };
    fs_events_callback(cb, &payload);
}
