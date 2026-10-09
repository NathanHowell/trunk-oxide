//! Links the prebuilt Cactus `libneedle.a` (C++ engine behind Whistle).
//!
//! The archive is not vendored: point `NEEDLE_LIB_DIR` at a directory holding
//! `libneedle.a` (e.g. the `linux-x86_64/` folder of `Cactus-Compute/needle3` on
//! Hugging Face). `Dockerfile.whistle` fetches it at a pinned revision.

fn main() {
    println!("cargo:rerun-if-env-changed=NEEDLE_LIB_DIR");
    let dir = std::env::var("NEEDLE_LIB_DIR").unwrap_or_else(|_| {
        panic!(
            "NEEDLE_LIB_DIR must point at a directory containing libneedle.a \
             (Cactus-Compute/needle3 on Hugging Face, linux-x86_64/); see Dockerfile.whistle"
        )
    });
    println!("cargo:rerun-if-changed={dir}/libneedle.a");
    println!("cargo:rustc-link-search=native={dir}");
    println!("cargo:rustc-link-lib=static=needle");
    // libneedle.a is built against LLVM's libc++ (std::__1), not libstdc++. Link
    // it statically so the distroless/cc runtime (libstdc++ only) needs nothing
    // extra. Needs libc++ >= 21 (static archives) at build time.
    // Optional: directory with libc++.a/libc++abi.a when not on the default path
    // (Debian keeps them under /usr/lib/llvm-N/lib).
    println!("cargo:rerun-if-env-changed=LIBCXX_LIB_DIR");
    if let Ok(dir) = std::env::var("LIBCXX_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
    }
    println!("cargo:rustc-link-lib=static=c++");
    println!("cargo:rustc-link-lib=static=c++abi");
}
