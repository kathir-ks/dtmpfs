fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .bytes(["."])
        .build_client(true)
        .build_server(true)
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_protos(
            &["../../proto/meta.proto", "../../proto/store.proto"],
            &["../../proto"],
        )?;
    Ok(())
}
