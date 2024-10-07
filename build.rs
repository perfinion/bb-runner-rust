use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let file_descriptor = out_dir.join("bb_descriptor.bin");

    let mut type_config: prost_build::Config = prost_build::Config::new();
    type_config.enable_type_names();
    type_config.type_name_domain(&["."], "type.googleapis.com");

    tonic_build::configure()
        .file_descriptor_set_path(file_descriptor)
        .compile_protos_with_config(
            type_config,
            &[
                "proto/resourceusage/resourceusage.proto",
                "proto/runner/runner.proto",
            ],
            &["proto/"],
        )?;

    Ok(())
}
