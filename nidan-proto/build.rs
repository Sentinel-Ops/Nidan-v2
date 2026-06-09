use std::io::Result;

fn main() -> Result<()> {
    // Génère le code Rust à partir des fichiers .proto
    // Les fichiers générés sont placés dans OUT_DIR (cargo gère ça)
    tonic_build::configure()
        // Dériver serde pour la sérialisation JSON des messages (utile pour l'API REST du broker)
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        // Dériver PartialEq pour les tests unitaires
        .type_attribute(".", "#[derive(PartialEq)]")
        // Activer la génération des clients/serveurs gRPC (utilisés par le broker)
        .build_server(true)
        .build_client(true)
        // Chemin de sortie dans OUT_DIR (géré par cargo)
        .compile(
            &["proto/nidan.proto"],
            &["proto/", "vendor/"],
        )?;

    // Re-run si le proto change
    println!("cargo:rerun-if-changed=proto/nidan.proto");
    println!("cargo:rerun-if-changed=build.rs");

    Ok(())
}
