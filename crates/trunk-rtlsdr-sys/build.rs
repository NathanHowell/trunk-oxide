//! Link librtlsdr. Debian's `librtlsdr-dev` ships a pkg-config file and the
//! unversioned `librtlsdr.so` link; a host with only the runtime package
//! (`librtlsdr0`) has `librtlsdr.so.0` alone, so when pkg-config comes up
//! empty we link that one through a symlink of our own. The container build
//! has the dev package.

use std::path::{Path, PathBuf};

const RUNTIME_LIB_DIRS: &[&str] = &[
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
    "/usr/lib64",
    "/usr/lib",
    "/usr/local/lib",
];

fn main() {
    println!("cargo:rerun-if-env-changed=RTLSDR_LIB_DIR");
    if let Ok(dir) = std::env::var("RTLSDR_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
        println!("cargo:rustc-link-lib=dylib=rtlsdr");
        return;
    }
    if pkg_config::Config::new().probe("librtlsdr").is_ok() {
        return;
    }
    let runtime = RUNTIME_LIB_DIRS
        .iter()
        .map(Path::new)
        .find_map(|dir| {
            ["librtlsdr.so.0", "librtlsdr.so.2"]
                .iter()
                .map(|name| dir.join(name))
                .find(|p| p.exists())
        })
        .expect(
            "librtlsdr not found: install librtlsdr-dev (or librtlsdr0), or set RTLSDR_LIB_DIR",
        );
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let link = out.join("librtlsdr.so");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&runtime, &link).expect("symlink librtlsdr.so into OUT_DIR");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=dylib=rtlsdr");
}
