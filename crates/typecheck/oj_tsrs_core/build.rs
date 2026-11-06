// `cfg(compressed_ptrs)`: `P<T>` is a 32-bit handle (notes/mem-pointer-compression.md). On by default on unix;
// the `plain-ptrs` feature, or a target without `mmap`, keeps `P<T>` a plain pointer.
fn main() {
    println!("cargo:rustc-check-cfg=cfg(compressed_ptrs)");
    let on = std::env::var_os("CARGO_FEATURE_PLAIN_PTRS").is_none()
        && std::env::var("CARGO_CFG_TARGET_FAMILY")
            .is_ok_and(|f| f.split(',').any(|f| f == "unix"));
    if on {
        println!("cargo:rustc-cfg=compressed_ptrs");
    }
}
