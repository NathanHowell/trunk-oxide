use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let trunk_recorder_dir = workspace_root.join("trunk-recorder");
    let compat_dir = manifest_dir.join("compat");

    // ── Probe gnuradio packages ──

    let gnuradio_runtime = pkg_config::Config::new()
        .atleast_version("3.8")
        .probe("gnuradio-runtime")
        .expect("gnuradio-runtime not found via pkg-config");

    // Compute GNURADIO_VERSION hex from pkg-config version (e.g. "3.10.12")
    let gnuradio_version_hex = compute_gnuradio_version(&gnuradio_runtime.version);
    eprintln!(
        "Using GNURADIO_VERSION=0x{gnuradio_version_hex:06x} ({})",
        gnuradio_runtime.version
    );

    // Required gnuradio subpackages
    let required_gr_packages = [
        "gnuradio-blocks",
        "gnuradio-filter",
        "gnuradio-analog",
        "gnuradio-digital",
        "gnuradio-fft",
    ];
    let mut all_gnuradio_libs = vec![gnuradio_runtime];
    for pkg in &required_gr_packages {
        let lib = pkg_config::Config::new()
            .probe(pkg)
            .unwrap_or_else(|e| panic!("{pkg} not found via pkg-config: {e}"));
        all_gnuradio_libs.push(lib);
    }

    // Optional gnuradio subpackages — probe but don't fail
    let optional_gr_packages = ["gnuradio-uhd"];
    for pkg in &optional_gr_packages {
        if let Ok(lib) = pkg_config::Config::new().probe(pkg) {
            all_gnuradio_libs.push(lib);
        }
    }

    // Optional external SDR packages
    let optional_sdr_packages = ["osmosdr", "gr-osmosdr", "uhd"];
    for pkg in &optional_sdr_packages {
        if let Ok(lib) = pkg_config::Config::new().probe(pkg) {
            all_gnuradio_libs.push(lib);
        }
    }

    // ── Build the cxx bridge ──

    let mut build = cxx_build::bridge("src/lib.rs");
    build.file("cpp/bridge.cpp").std("c++17");

    // Define GNURADIO_VERSION so trunk-recorder headers use correct conditionals
    build.define(
        "GNURADIO_VERSION",
        format!("0x{gnuradio_version_hex:06x}").as_str(),
    );

    // trunk-recorder headers
    build.include(&trunk_recorder_dir);
    // json.hpp
    build.include(trunk_recorder_dir.join("lib"));
    // op25_repeater headers (bundled in trunk-recorder)
    build.include(trunk_recorder_dir.join("lib/op25_repeater/include"));

    // gnuradio and other package include paths
    for lib in &all_gnuradio_libs {
        for path in &lib.include_paths {
            build.include(path);
        }
    }

    // Boost headers
    if let Some(boost_include) = find_boost_include() {
        build.include(&boost_include);
    }

    // Generate stub headers for optional dependencies not present on this system
    generate_compat_stubs(&compat_dir, &trunk_recorder_dir);
    build.include(&compat_dir);

    // Suppress warnings from trunk-recorder headers
    build.warnings(false);

    // Add cmake.h compat stub for config.h's transitive include
    let cmake_stub = compat_dir.join("cmake.h");
    if !cmake_stub.exists() {
        std::fs::write(
            &cmake_stub,
            r#"#pragma once
#define PROJECT_NAME "trunk-recorder"
#define PROJECT_VER  "0.0.0"
#define PROJECT_VER_MAJOR "0"
#define PROJECT_VER_MINOR "0"
#define PTOJECT_VER_PATCH "0"
"#,
        )
        .ok();
    }

    // Link against libtrunk-recorder.so
    println!("cargo:rustc-link-lib=trunk-recorder");

    // spdlog uses external fmt on Debian (SPDLOG_FMT_EXTERNAL)
    if let Ok(fmt_lib) = pkg_config::Config::new().probe("fmt") {
        for path in &fmt_lib.link_paths {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
    } else {
        // Fallback: try linking fmt directly
        println!("cargo:rustc-link-lib=fmt");
    }

    if let Ok(lib_dir) = std::env::var("TRUNK_RECORDER_LIB_DIR") {
        println!("cargo:rustc-link-search=native={lib_dir}");
    }

    build.compile("trunk_recorder_sys_bridge");

    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cpp/bridge.h");
    println!("cargo:rerun-if-changed=cpp/bridge.cpp");
}

/// Compute the `GNURADIO_VERSION` hex value using trunk-recorder's encoding:
/// `(major/10)<<20 | (major%10)<<16 | (minor/10)<<12 | (minor%10)<<8 | (patch/10)<<4 | (patch%10)`
fn compute_gnuradio_version(version: &str) -> u32 {
    let parts: Vec<u32> = version.split('.').filter_map(|s| s.parse().ok()).collect();
    let (major, minor, patch) = (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
    );
    (major / 10) << 20
        | (major % 10) << 16
        | (minor / 10) << 12
        | (minor % 10) << 8
        | (patch / 10) << 4
        | (patch % 10)
}

/// Find boost include directory
fn find_boost_include() -> Option<PathBuf> {
    // Try pkg-config first
    if let Ok(boost) = pkg_config::Config::new().probe("boost") {
        if let Some(path) = boost.include_paths.first() {
            return Some(path.clone());
        }
    }

    // Try brew
    if let Ok(output) = std::process::Command::new("brew")
        .args(["--prefix", "boost"])
        .output()
    {
        if output.status.success() {
            let prefix = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let inc = PathBuf::from(format!("{prefix}/include"));
            if inc.exists() {
                return Some(inc);
            }
        }
    }

    // System paths
    for p in ["/usr/include", "/usr/local/include"] {
        let path = PathBuf::from(p);
        if path.join("boost/shared_ptr.hpp").exists() {
            return Some(path);
        }
    }

    None
}

/// Generate minimal stub headers for optional dependencies that aren't installed.
/// These stubs provide just enough type declarations to satisfy the compiler
/// for headers that are transitively included by trunk-recorder but not actually
/// used by our bridge code. On a full build machine (e.g. Linux with all deps),
/// the real headers will be found first via the earlier include paths.
fn generate_compat_stubs(compat_dir: &std::path::Path, trunk_recorder_dir: &std::path::Path) {
    // Check which optional headers are missing by testing if the compiler can find them
    let stubs: &[(&str, &str)] = &[(
        "osmosdr/source.h",
        r#"#pragma once
// Stub: osmosdr not available on this system.
// The real header is only needed for Source's osmosdr::source member,
// which we never instantiate from the plugin bridge.
#include <memory>
namespace osmosdr {
class source {
public:
    #if GNURADIO_VERSION < 0x030900
    typedef boost::shared_ptr<source> sptr;
    #else
    typedef std::shared_ptr<source> sptr;
    #endif
    static sptr make(const std::string &args = "") { return nullptr; }
    void set_sample_rate(double rate) {}
    double get_sample_rate() { return 0; }
    void set_center_freq(double freq) {}
    double get_center_freq() { return 0; }
    void set_freq_corr(double ppm) {}
    void set_gain_mode(bool automatic) {}
    void set_gain(double gain) {}
    void set_if_gain(double gain) {}
    void set_bb_gain(double gain) {}
    void set_antenna(const std::string &antenna) {}
    std::string get_antenna() { return ""; }
    std::vector<std::string> get_antennas() { return {}; }
    void set_bandwidth(double bandwidth) {}
    std::vector<std::string> get_gain_names() { return {}; }
    double get_gain(const std::string &name = "") { return 0; }
    void set_gain(double gain, const std::string &name) {}
};
} // namespace osmosdr
"#,
    )];

    // Only write stubs for headers that can't be found in the real include paths
    for (header, content) in stubs {
        let real_found = check_header_exists(header, trunk_recorder_dir);
        if !real_found {
            let stub_path = compat_dir.join(header);
            if let Some(parent) = stub_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&stub_path, content).ok();
            println!("cargo:warning=Generated compat stub for {header} (not found on system)");
        }
    }
}

/// Check if a header can be found via pkg-config include paths
fn check_header_exists(header: &str, _trunk_recorder_dir: &std::path::Path) -> bool {
    // Check common system include paths
    let search_paths = [
        "/usr/include",
        "/usr/local/include",
        "/opt/homebrew/include",
    ];

    for base in &search_paths {
        if std::path::Path::new(base).join(header).exists() {
            return true;
        }
    }

    // Also check via pkg-config paths for relevant packages
    for pkg in &["osmosdr", "gr-osmosdr"] {
        if let Ok(lib) = pkg_config::Config::new().probe(pkg) {
            for path in &lib.include_paths {
                if path.join(header).exists() {
                    return true;
                }
            }
        }
    }

    false
}
