use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = "proto";
    let proto_files = vec!["echo.proto"];
    let proto_paths: Vec<_> = proto_files
        .iter()
        .map(|file| Path::new(proto_root).join(file))
        .collect();

    // Ensure the output directory exists
    std::fs::create_dir_all("src/pb")?;

    // Use tonic-prost-build for service generation
    tonic_prost_build::configure()
        .out_dir("src/pb")
        .include_file("mod.rs")
        // Disable tonic's transport features - we want just the service traits
        .build_transport(false)
        .build_client(true)
        .build_server(true) // Generate server traits we can implement
        .compile_protos(&proto_paths, &[Path::new(proto_root).to_path_buf()])?;

    // Tell cargo to rerun if any proto files change
    for proto_file in &proto_files {
        println!("cargo:rerun-if-changed=proto/{proto_file}");
    }

    Ok(())
}
