//! Génération des types Rust pour le protocole v2 (canal vsock agent ↔ proxy).
//!
//! Le proto v1 (nidan.proto) reste en Rust pur / serde (lib.rs), pour
//! cohérence avec le code existant. Seul le proto v2 (agent.proto) passe
//! par prost-build : refonte = occasion de moderniser sans casser la v1.

fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=proto/agent.proto");
    println!("cargo:rerun-if-changed=build.rs");
    prost_build::compile_protos(
        &["proto/agent.proto"],
        &["proto/"],
    )?;
    Ok(())
}
