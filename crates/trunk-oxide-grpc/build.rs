fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("proto/trunk_oxide.proto")?;

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();

    // Allow undefined symbols in the cdylib. Source and Recorder have non-virtual
    // methods (get_num, get_driver, etc.) defined in trunk-recorder's binary.
    // Since we're a plugin loaded via dlopen, these symbols resolve at load time
    // from the host process.
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
    }

    Ok(())
}
