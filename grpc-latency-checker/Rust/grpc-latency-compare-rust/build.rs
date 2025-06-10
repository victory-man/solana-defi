fn main() {
    let includes: &[&str] = &["./protos"];
    tonic_build::configure()
        .bytes(&["tempo.Transaction.payload", "bloxroute.Transaction.data"])
        .compile_protos(&["./protos/tx_streamer.proto", "./protos/tempo.proto"], includes)
        .unwrap_or_else(|e| panic!("Failed to compile protos {:?}", e));
}