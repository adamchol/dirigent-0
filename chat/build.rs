fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../server/proto/dirigent.proto");
    let fds = protox::compile(["dirigent.proto"], ["../server/proto"])?;
    tonic_prost_build::configure()
        .build_server(false)
        .compile_fds(fds)?;
    Ok(())
}
