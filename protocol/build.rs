//! The control plane's gRPC contract with players (`proto/`), compiled without protoc.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "proto/dirigent/control/v1/players.proto";
    println!("cargo:rerun-if-changed={proto}");
    let fds = protox::compile([proto], ["proto"])?;
    tonic_prost_build::configure().compile_fds(fds)?;
    Ok(())
}
