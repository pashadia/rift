fn main() {
    let proto_root = "../../proto";
    prost_build::Config::new()
        .compile_protos(
            &[
                &format!("{proto_root}/common.proto"),
                &format!("{proto_root}/handshake.proto"),
                &format!("{proto_root}/operations.proto"),
                &format!("{proto_root}/transfer.proto"),
                &format!("{proto_root}/notifications.proto"),
            ],
            &[proto_root],
        )
        .expect("prost-build failed");
}
