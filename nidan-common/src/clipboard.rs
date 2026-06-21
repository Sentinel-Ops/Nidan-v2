//! Moteur de filtrage du presse-papier.
//!
//! Applique la `ClipboardPolicy` à chaque transfert de presse-papier, dans les
//! deux directions (client → serveur et serveur → client). Le filtrage couvre :
//!   - la direction (un sens peut être désactivé) ;
//!   - la taille (plafond configurable, borné par `MAX_CLIPBOARD_BYTES`) ;
//!   - le type MIME (liste blanche optionnelle) ;
//!   - le contenu (motifs regex bloqués : clés privées, numéros de carte…).
//!
//! Ce module est partagé client/serveur : le filtrage doit être appliqué
//! côté émetteur ET côté récepteur (défense en profondeur — un client modifié
//! ne doit pas pouvoir contourner la politique du serveur).

use crate::config::ClipboardPolicy;
use regex::Regex;

/// Taille maximale absolue d'un transfert presse-papier (garde-fou).
/// Aligné sur `nidan_proto::MAX_CLIPBOARD_BYTES`.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// Direction d'un transfert de presse-papier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardDirection {
    /// Le contenu va du client vers le serveur (poste local → machine distante).
    ClientToServer,
    /// Le contenu va du serveur vers le client (machine distante → poste local).
    ServerToClient,
}

/// Décision rendue par le moteur de filtrage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardDecision {
    /// Transfert autorisé.
    Allow,
    /// Transfert refusé, avec le motif (journalisable, présentable à l'audit).
    Block(ClipboardBlockReason),
}

impl ClipboardDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, ClipboardDecision::Allow)
    }
}

/// Motif de blocage d'un transfert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardBlockReason {
    /// La direction est désactivée par la politique.
    DirectionDisabled(ClipboardDirection),
    /// Le contenu dépasse la taille maximale autorisée.
    TooLarge { size: usize, max: usize },
    /// Le type MIME n'est pas dans la liste blanche.
    MimeNotAllowed(String),
    /// Le contenu correspond à un motif bloqué (exfiltration de secret).
    BlockedPattern(String),
    /// Un motif regex de la politique est invalide (fail-closed : on bloque).
    InvalidPolicyPattern(String),
}

impl std::fmt::Display for ClipboardBlockReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DirectionDisabled(d) =>
                write!(f, "direction désactivée par la politique ({d:?})"),
            Self::TooLarge { size, max } =>
                write!(f, "contenu trop volumineux ({size} > {max} octets)"),
            Self::MimeNotAllowed(m) =>
                write!(f, "type MIME non autorisé ({m})"),
            Self::BlockedPattern(p) =>
                write!(f, "contenu bloqué par un motif de sécurité ({p})"),
            Self::InvalidPolicyPattern(p) =>
                write!(f, "motif de politique invalide, transfert bloqué par précaution ({p})"),
        }
    }
}

/// Moteur de filtrage : compile une fois les motifs regex de la politique,
/// puis évalue chaque transfert. Crée-le à l'ouverture de session et réutilise-le.
pub struct ClipboardFilter {
    policy: ClipboardPolicy,
    // Motifs compilés (Ok) ou source du motif invalide (Err) — fail-closed.
    compiled: Vec<Result<Regex, String>>,
}

impl ClipboardFilter {
    /// Construit le filtre à partir d'une politique. Les motifs regex sont
    /// compilés une seule fois ; un motif invalide est conservé pour
    /// déclencher un blocage par précaution (fail-closed) plutôt qu'ignoré.
    pub fn new(policy: ClipboardPolicy) -> Self {
        let compiled = policy
            .blocked_patterns
            .iter()
            .map(|p| Regex::new(p).map_err(|_| p.clone()))
            .collect();
        Self { policy, compiled }
    }

    /// Évalue un transfert et rend une décision.
    pub fn evaluate(
        &self,
        direction: ClipboardDirection,
        mime_type: &str,
        content: &[u8],
    ) -> ClipboardDecision {
        // 1. Direction
        let direction_ok = match direction {
            ClipboardDirection::ClientToServer => self.policy.allow_client_to_server,
            ClipboardDirection::ServerToClient => self.policy.allow_server_to_client,
        };
        if !direction_ok {
            return ClipboardDecision::Block(ClipboardBlockReason::DirectionDisabled(direction));
        }

        // 2. Taille : plafond de la politique (si > 0) ET garde-fou absolu
        let effective_max = if self.policy.max_size_bytes == 0 {
            MAX_CLIPBOARD_BYTES
        } else {
            (self.policy.max_size_bytes as usize).min(MAX_CLIPBOARD_BYTES)
        };
        if content.len() > effective_max {
            return ClipboardDecision::Block(ClipboardBlockReason::TooLarge {
                size: content.len(),
                max: effective_max,
            });
        }

        // 3. Type MIME : si une liste blanche est définie, le type doit y figurer
        if !self.policy.allowed_mime_types.is_empty()
            && !self.policy.allowed_mime_types.iter().any(|m| m == mime_type)
        {
            return ClipboardDecision::Block(ClipboardBlockReason::MimeNotAllowed(
                mime_type.to_string(),
            ));
        }

        // 4. Motifs bloqués (exfiltration de secrets). On n'analyse que du texte ;
        //    un contenu binaire non-UTF8 n'est pas scanné par regex (mais reste
        //    soumis aux règles de taille/MIME/direction ci-dessus).
        if let Ok(text) = std::str::from_utf8(content) {
            for entry in &self.compiled {
                match entry {
                    Ok(re) => {
                        if re.is_match(text) {
                            return ClipboardDecision::Block(
                                ClipboardBlockReason::BlockedPattern(re.as_str().to_string()),
                            );
                        }
                    }
                    Err(src) => {
                        // Motif invalide : fail-closed pour ne pas créer un trou.
                        return ClipboardDecision::Block(
                            ClipboardBlockReason::InvalidPolicyPattern(src.clone()),
                        );
                    }
                }
            }
        }

        ClipboardDecision::Allow
    }

    /// Indique si l'audit des transferts est demandé par la politique.
    pub fn audit_enabled(&self) -> bool {
        self.policy.audit_transfers
    }

    /// Évalue un transfert décrit par les champs bruts du proto
    /// (`ClipboardTransferRequest`). La direction suit la convention proto :
    /// 1 = client→serveur, 2 = serveur→client. Un code de direction inconnu
    /// est rejeté (fail-closed).
    pub fn evaluate_proto(
        &self,
        direction_code: i32,
        mime_type: &str,
        content: &[u8],
    ) -> ClipboardDecision {
        let direction = match direction_code {
            1 => ClipboardDirection::ClientToServer,
            2 => ClipboardDirection::ServerToClient,
            other => {
                return ClipboardDecision::Block(ClipboardBlockReason::InvalidPolicyPattern(
                    format!("direction inconnue: {other}"),
                ));
            }
        };
        self.evaluate(direction, mime_type, content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ClipboardPolicy {
        ClipboardPolicy {
            allow_client_to_server: true,
            allow_server_to_client: true,
            max_size_bytes: 1024,
            allowed_mime_types: vec![],
            blocked_patterns: vec![
                r"-----BEGIN [A-Z ]*PRIVATE KEY-----".to_string(), // clés privées PEM
                r"\b\d{4}[ -]?\d{4}[ -]?\d{4}[ -]?\d{4}\b".to_string(), // numéros CB
            ],
            audit_transfers: true,
        }
    }

    fn filter() -> ClipboardFilter {
        ClipboardFilter::new(policy())
    }

    #[test]
    fn test_normal_text_allowed() {
        let d = filter().evaluate(ClipboardDirection::ClientToServer, "text/plain", b"bonjour");
        assert!(d.is_allowed());
    }

    #[test]
    fn test_direction_disabled_blocks() {
        let mut p = policy();
        p.allow_client_to_server = false;
        let f = ClipboardFilter::new(p);
        let d = f.evaluate(ClipboardDirection::ClientToServer, "text/plain", b"hi");
        assert!(matches!(
            d,
            ClipboardDecision::Block(ClipboardBlockReason::DirectionDisabled(_))
        ));
        // L'autre sens reste autorisé
        assert!(f.evaluate(ClipboardDirection::ServerToClient, "text/plain", b"hi").is_allowed());
    }

    #[test]
    fn test_too_large_blocks() {
        let big = vec![b'a'; 2048]; // > max_size_bytes (1024)
        let d = filter().evaluate(ClipboardDirection::ClientToServer, "text/plain", &big);
        assert!(matches!(
            d,
            ClipboardDecision::Block(ClipboardBlockReason::TooLarge { .. })
        ));
    }

    #[test]
    fn test_mime_whitelist_blocks_others() {
        let mut p = policy();
        p.allowed_mime_types = vec!["text/plain".to_string()];
        let f = ClipboardFilter::new(p);
        assert!(f.evaluate(ClipboardDirection::ClientToServer, "text/plain", b"ok").is_allowed());
        let d = f.evaluate(ClipboardDirection::ClientToServer, "image/png", b"ok");
        assert!(matches!(
            d,
            ClipboardDecision::Block(ClipboardBlockReason::MimeNotAllowed(_))
        ));
    }

    #[test]
    fn test_private_key_blocked() {
        let secret = b"-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA...\n-----END...";
        let d = filter().evaluate(ClipboardDirection::ClientToServer, "text/plain", secret);
        assert!(matches!(
            d,
            ClipboardDecision::Block(ClipboardBlockReason::BlockedPattern(_))
        ), "une clé privée doit être bloquée");
    }

    #[test]
    fn test_credit_card_blocked() {
        let d = filter().evaluate(
            ClipboardDirection::ServerToClient,
            "text/plain",
            b"ma carte: 4111 1111 1111 1111 merci",
        );
        assert!(matches!(
            d,
            ClipboardDecision::Block(ClipboardBlockReason::BlockedPattern(_))
        ), "un numéro de carte doit être bloqué");
    }

    #[test]
    fn test_invalid_pattern_fails_closed() {
        let mut p = policy();
        p.blocked_patterns = vec![r"[invalid(regex".to_string()];
        let f = ClipboardFilter::new(p);
        let d = f.evaluate(ClipboardDirection::ClientToServer, "text/plain", b"texte anodin");
        assert!(matches!(
            d,
            ClipboardDecision::Block(ClipboardBlockReason::InvalidPolicyPattern(_))
        ), "un motif invalide doit bloquer (fail-closed), pas être ignoré");
    }

    #[test]
    fn test_binary_content_not_scanned_but_size_enforced() {
        // Contenu binaire non-UTF8 : pas de scan regex, mais taille appliquée
        let mut p = policy();
        p.max_size_bytes = 8;
        let f = ClipboardFilter::new(p);
        let bin = vec![0xff, 0xfe, 0x00, 0x01];
        assert!(f.evaluate(ClipboardDirection::ClientToServer, "application/octet-stream", &bin).is_allowed());
        let big_bin = vec![0xff; 16];
        assert!(!f.evaluate(ClipboardDirection::ClientToServer, "application/octet-stream", &big_bin).is_allowed());
    }

    #[test]
    fn test_zero_max_uses_absolute_cap() {
        let mut p = policy();
        p.max_size_bytes = 0; // 0 = pas de limite de politique → garde-fou absolu
        let f = ClipboardFilter::new(p);
        // Sous le garde-fou : autorisé
        assert!(f.evaluate(ClipboardDirection::ClientToServer, "text/plain", b"ok").is_allowed());
        // Au-dessus du garde-fou absolu : bloqué
        let huge = vec![b'a'; MAX_CLIPBOARD_BYTES + 1];
        assert!(!f.evaluate(ClipboardDirection::ClientToServer, "text/plain", &huge).is_allowed());
    }

    #[test]
    fn test_evaluate_proto_direction_mapping() {
        let f = filter();
        // 1 = client→serveur, 2 = serveur→client
        assert!(f.evaluate_proto(1, "text/plain", b"ok").is_allowed());
        assert!(f.evaluate_proto(2, "text/plain", b"ok").is_allowed());
        // direction inconnue → bloqué (fail-closed)
        assert!(!f.evaluate_proto(99, "text/plain", b"ok").is_allowed());
        assert!(!f.evaluate_proto(0, "text/plain", b"ok").is_allowed());
    }

    #[test]
    fn test_evaluate_proto_applies_filtering() {
        // Le passage par le proto applique bien les motifs bloqués
        let f = filter();
        let secret = b"-----BEGIN RSA PRIVATE KEY-----";
        assert!(!f.evaluate_proto(1, "text/plain", secret).is_allowed());
    }
}
