fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("proto/runner/runner.proto")?;

    let mut type_config: prost_build::Config = prost_build::Config::new();
    type_config.enable_type_names();
    type_config.type_name_domain(&["."], "type.googleapis.com");
    tonic_build::configure().compile_with_config(
        type_config,
        &["proto/resourceusage/resourceusage.proto"],
        &["proto/resourceusage/"],
    )?;

    Ok(())
}
