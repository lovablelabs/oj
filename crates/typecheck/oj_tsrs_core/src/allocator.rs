//! Startup policy shared by the binaries that use mimalloc as their global allocator.

/// Restores transparent huge pages for the arena after mimalloc's startup constructor disables them.
/// Call at the start of each mimalloc binary's `main`, before spawning worker threads.
#[cfg(all(target_os = "linux", not(miri)))]
pub fn init_mimalloc() {
    // mimalloc-safe's `no_thp` feature sets `allow_thp = 0`, which disables THP for the whole process in
    // `_mi_prim_mem_init`. Keep that allocator option to avoid huge-page amplification on its heap, but
    // restore THP for the arena's `MADV_HUGEPAGE` chunks (notes/mem-no-thp.md). An explicit environment
    // override keeps mimalloc's choice.
    if std::env::var_os("MIMALLOC_ALLOW_THP").is_some() {
        return;
    }
    let zero: libc::c_ulong = 0;
    // SAFETY: PR_SET_THP_DISABLE takes integer arguments only and changes nothing but this process's THP flag.
    unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, zero, zero, zero, zero) };
}
