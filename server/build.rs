fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/dirigent.proto");
    // protox compiles the proto in pure Rust, so no `protoc` is needed.
    let fds = protox::compile(["proto/dirigent.proto"], ["proto"])?;
    tonic_prost_build::configure().compile_fds(fds)?;
    Ok(())
}
