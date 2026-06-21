//! Helpers mTLS — extraction d'identité depuis les certificats client.
//!
//! quinn expose le certificat présenté par le pair via `peer_identity()`,
//! typé `Box<dyn Any>`. Sous rustls, le type concret est
//! `Vec<CertificateDer<'static>>` (la chaîne du client, leaf en premier).
//! On parse le certificat leaf en X.509 et on en extrait le Subject DN réel.

use rustls::pki_types::CertificateDer;
use tracing::{debug, warn};
use x509_parser::prelude::*;

/// Extrait l'identité (Subject DN) depuis le certificat client mTLS.
///
/// Retourne `None` si la connexion n'est pas mutuellement authentifiée
/// (pas de certificat client) ou si le certificat est illisible.
pub fn extract_peer_identity(conn: &quinn::Connection) -> Option<String> {
    let identity = conn.peer_identity()?;

    // Sous rustls, peer_identity() est un Vec<CertificateDer> (chaîne client).
    let certs = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| warn!("peer_identity: type inattendu (pas un Vec<CertificateDer>)"))
        .ok()?;

    let leaf = certs.first()?; // le certificat client (leaf) est en tête de chaîne
    extract_subject_dn(leaf.as_ref())
}

/// Parse un certificat DER et retourne son Subject DN au format RFC 4514
/// (ex. "CN=jdoe,OU=ANSSI,O=SGDSN,C=FR").
pub fn extract_subject_dn(cert_der: &[u8]) -> Option<String> {
    match parse_x509_certificate(cert_der) {
        Ok((_, cert)) => {
            let dn = cert.subject().to_string();
            debug!(subject = %dn, "identité mTLS extraite du certificat client");
            Some(dn)
        }
        Err(e) => {
            warn!(error = %e, "échec parsing X.509 du certificat client");
            None
        }
    }
}

/// Extrait le seul CN (Common Name) du Subject, si présent.
/// Pratique pour journaliser ou indexer par identité courte.
pub fn extract_common_name(cert_der: &[u8]) -> Option<String> {
    let (_, cert) = parse_x509_certificate(cert_der).ok()?;
    let cn: Option<String> = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|attr| attr.as_str().ok())
        .map(|s| s.to_string());
    cn
}

#[cfg(test)]
mod tests {
    use super::*;

    // Certificat client de test généré par scripts/pki-init.sh, encodé en DER.
    // Pour éviter d'embarquer un binaire, le test génère un certificat à la volée.
    fn make_test_cert(cn: &str, ou: &str) -> Vec<u8> {
        use rcgen::{CertificateParams, DistinguishedName, DnType};
        let mut params = CertificateParams::new(vec![]).unwrap();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, cn);
        dn.push(DnType::OrganizationalUnitName, ou);
        dn.push(DnType::OrganizationName, "SGDSN");
        dn.push(DnType::CountryName, "FR");
        params.distinguished_name = dn;
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        cert.der().to_vec()
    }

    #[test]
    fn test_extract_subject_dn_real_cert() {
        let der = make_test_cert("jdoe", "ANSSI");
        let dn = extract_subject_dn(&der).expect("DN doit être extrait");
        assert!(dn.contains("CN=jdoe"), "DN={dn}");
        assert!(dn.contains("OU=ANSSI"), "DN={dn}");
        assert!(dn.contains("O=SGDSN"), "DN={dn}");
    }

    #[test]
    fn test_extract_common_name() {
        let der = make_test_cert("alice", "DEFENSE");
        let cn = extract_common_name(&der).expect("CN doit être extrait");
        assert_eq!(cn, "alice");
    }

    #[test]
    fn test_invalid_der_returns_none() {
        let garbage = vec![0x00, 0x01, 0x02, 0x03];
        assert!(extract_subject_dn(&garbage).is_none());
        assert!(extract_common_name(&garbage).is_none());
    }

    #[test]
    fn test_distinct_certs_distinct_identities() {
        // Preuve anti-stub : deux certs différents → deux identités différentes
        let a = extract_subject_dn(&make_test_cert("user-a", "OU1")).unwrap();
        let b = extract_subject_dn(&make_test_cert("user-b", "OU2")).unwrap();
        assert_ne!(a, b, "des certificats distincts doivent donner des DN distincts");
    }
}
