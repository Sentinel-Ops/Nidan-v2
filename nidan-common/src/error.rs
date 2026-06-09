//! Système d'erreurs centralisé pour NIDAN.

/// Erreurs du domaine NIDAN
#[derive(Debug, thiserror::Error)]
pub enum NidanError {
    /// Erreur réseau / transport QUIC
    #[error("erreur réseau: {0}")]
    Network(#[from] std::io::Error),

    /// Erreur d'authentification
    #[error("authentification échouée: {reason}")]
    Auth {
        /// Raison de l'échec
        reason: String,
    },

    /// Session introuvable ou expirée
    #[error("session introuvable: {session_id}")]
    SessionNotFound {
        /// Identifiant de la session
        session_id: String,
    },

    /// Session déjà fermée
    #[error("session déjà fermée: {session_id}")]
    SessionClosed {
        /// Identifiant de la session
        session_id: String,
    },

    /// Erreur de chiffrement / déchiffrement
    #[error("erreur cryptographique: {0}")]
    Crypto(String),

    /// Erreur de validation du protocole
    #[error("erreur de validation proto: {0}")]
    ProtoValidation(#[from] nidan_proto::ProtoValidationError),

    /// Erreur d'encodage/décodage vidéo
    #[error("erreur codec vidéo: {0}")]
    Codec(String),

    /// Transfert clipboard refusé par politique
    #[error("clipboard refusé: {reason}")]
    ClipboardDenied {
        /// Raison du refus
        reason: String,
    },

    /// Aucune VM disponible dans le pool
    #[error("aucune VM disponible dans le pool")]
    NoVmAvailable,

    /// Erreur de configuration
    #[error("erreur de configuration: {0}")]
    Config(String),

    /// Erreur d'audit (non fatale)
    #[error("erreur d'audit: {0}")]
    Audit(String),

    /// Erreur générique avec contexte
    #[error("{context}: {source}")]
    WithContext {
        /// Contexte de l'erreur
        context: String,
        /// Source
        #[source]
        source: Box<NidanError>,
    },
}

/// Alias de Result pour le domaine NIDAN
pub type NidanResult<T> = Result<T, NidanError>;

/// Extension trait pour ajouter du contexte aux erreurs
pub trait NidanResultExt<T> {
    /// Ajoute un contexte textuel à l'erreur
    fn nidan_context(self, context: impl Into<String>) -> NidanResult<T>;
}

impl<T> NidanResultExt<T> for NidanResult<T> {
    fn nidan_context(self, context: impl Into<String>) -> NidanResult<T> {
        self.map_err(|e| NidanError::WithContext {
            context: context.into(),
            source: Box::new(e),
        })
    }
}
