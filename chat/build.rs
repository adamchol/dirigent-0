fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../player/proto/player.proto");
    let fds = protox::compile(["player.proto"], ["../player/proto"])?;
    tonic_prost_build::configure()
        .build_server(false)
        .compile_fds(fds)?;
    Ok(())
}
