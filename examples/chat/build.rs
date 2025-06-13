fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("./proto/p2p_chat.proto")?;
    Ok(())
}