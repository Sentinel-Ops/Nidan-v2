//! Helpers mTLS — extraction d'identité depuis les certificats client.

/// Extrait l'identité depuis le peer certificate QUIC/TLS
/// Retourne le DN complet du Subject
pub fn extract_peer_identity(conn: &quinn::Connection) -> Option<String> {
    // quinn expose les peer certificates via connection.peer_identity()
    // En Phase 3 : parser le Subject DN du certificat DER
    // Pour l'instant : stub qui retourne un DN fictif si la connexion est mTLS
    conn.peer_identity().map(|_| {
        // TODO Phase 3.1 : parser X.509 DER → Subject DN
        // x509_parser::parse_x509_certificate(cert_der) → .subject.to_string()
        "CN=client,OU=nidan,O=Example,C=FR".to_string()
    })
}
