fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("proto/trunk_oxide.proto")?;

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();

    // Allow undefined symbols in the cdylib. The plugin is loaded via dlopen
    // into trunk-recorder, so any remaining undefined symbols resolve from the
    // host process at load time.
    if target_os == "macos" {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-undefined,dynamic_lookup");
    } else if target_os == "linux" {
        println!("cargo:rustc-cdylib-link-arg=-Wl,--allow-shlib-undefined");
    }

    // Force the linker to keep `create_plugin` from the static bridge library.
    // Without this, the linker dead-strips it since no Rust code references it —
    // it's only called by trunk-recorder at dlopen time.
    if target_os == "macos" {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-exported_symbol,_create_plugin");
    } else if target_os == "linux" {
        println!("cargo:rustc-cdylib-link-arg=-Wl,--require-defined=create_plugin");

        // Rust's cdylib uses a version script with `local: *` that hides all
        // non-Rust symbols. Add a supplementary version script to export
        // create_plugin to the dynamic symbol table.
        let out_dir = std::env::var("OUT_DIR")?;
        let ver_script = format!("{out_dir}/export_create_plugin.ver");
        std::fs::write(&ver_script, "{ global: create_plugin; };\n")?;
        println!("cargo:rustc-cdylib-link-arg=-Wl,--version-script={ver_script}");
    }

    Ok(())
}
