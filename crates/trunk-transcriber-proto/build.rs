fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .type_attribute(".", "#[allow(clippy::all)]")
        .server_mod_attribute(".", "#[allow(clippy::all)]")
        .client_mod_attribute(".", "#[allow(clippy::all)]")
        .compile_protos(&["proto/transcriber.proto"], &["proto"])?;
    Ok(())
}
