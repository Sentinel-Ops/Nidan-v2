fn main() {
    // Génération protobuf désactivée — types Rust purs utilisés à la place
    // Pour activer : installer protoc et décommenter dans src/lib.rs
    println!("cargo:rerun-if-changed=build.rs");
}
