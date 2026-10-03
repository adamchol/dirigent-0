fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/player.proto");
    // protox compiles the proto in pure Rust, so no `protoc` is needed.
    let fds = protox::compile(["proto/player.proto"], ["proto"])?;
    tonic_prost_build::configure().compile_fds(fds)?;
    Ok(())
}
