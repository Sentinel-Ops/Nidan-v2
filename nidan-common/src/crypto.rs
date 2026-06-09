//! Dérivation des clés de session et chiffrement du stream vidéo.
//!
//! NIDAN utilise ECDH X25519 pour l'échange de clés, puis HKDF-SHA256
//! pour dériver une clé de session, et ChaCha20-Poly1305 pour le chiffrement
//! des frames vidéo/audio.

use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    ChaCha20Poly1305, Key, Nonce,
};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;

use crate::error::{NidanError, NidanResult};
use nidan_proto::{CHACHA20_NONCE_SIZE, SESSION_KEY_SIZE};

/// Contexte HKDF pour la dérivation des clés de session
const HKDF_INFO_VIDEO: &[u8] = b"nidan-v1-video-stream";
const HKDF_INFO_AUDIO: &[u8] = b"nidan-v1-audio-stream";
const HKDF_INFO_CONTROL: &[u8] = b"nidan-v1-control";

/// Clé de session symétrique dérivée
#[derive(Clone, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
pub struct SessionKey {
    key: [u8; SESSION_KEY_SIZE],
}

impl SessionKey {
    /// Crée une SessionKey à partir d'un matériau de clé brut
    pub fn from_bytes(bytes: &[u8]) -> NidanResult<Self> {
        if bytes.len() < SESSION_KEY_SIZE {
            return Err(NidanError::Crypto(format!(
                "matériau de clé trop court: {} < {}",
                bytes.len(),
                SESSION_KEY_SIZE
            )));
        }
        let mut key = [0u8; SESSION_KEY_SIZE];
        key.copy_from_slice(&bytes[..SESSION_KEY_SIZE]);
        Ok(Self { key })
    }

    /// Accès à la clé brute (usage interne uniquement)
    pub fn as_bytes(&self) -> &[u8; SESSION_KEY_SIZE] {
        &self.key
    }
}

/// Ensemble des clés dérivées pour une session
pub struct SessionKeys {
    /// Clé pour le stream vidéo
    pub video: SessionKey,
    /// Clé pour le stream audio
    pub audio: SessionKey,
    /// Clé pour le canal de contrôle
    pub control: SessionKey,
}

/// Dérive les clés de session depuis les nonces échangés et le secret partagé.
///
/// # Paramètres
/// - `shared_secret` : secret ECDH X25519 (32 bytes)
/// - `client_nonce`  : nonce aléatoire du client (32 bytes)
/// - `server_nonce`  : nonce aléatoire du serveur (32 bytes)
///
/// Les deux nonces sont concaténés comme sel HKDF pour garantir la fraîcheur
/// même si l'un des deux est prévisible.
pub fn derive_session_keys(
    shared_secret: &[u8],
    client_nonce: &[u8],
    server_nonce: &[u8],
) -> NidanResult<SessionKeys> {
    // Sel = client_nonce || server_nonce
    let mut salt = Vec::with_capacity(client_nonce.len() + server_nonce.len());
    salt.extend_from_slice(client_nonce);
    salt.extend_from_slice(server_nonce);

    let hk = Hkdf::<Sha256>::new(Some(&salt), shared_secret);

    let mut video_key = [0u8; SESSION_KEY_SIZE];
    let mut audio_key = [0u8; SESSION_KEY_SIZE];
    let mut control_key = [0u8; SESSION_KEY_SIZE];

    hk.expand(HKDF_INFO_VIDEO, &mut video_key)
        .map_err(|e| NidanError::Crypto(format!("HKDF expand video: {}", e)))?;
    hk.expand(HKDF_INFO_AUDIO, &mut audio_key)
        .map_err(|e| NidanError::Crypto(format!("HKDF expand audio: {}", e)))?;
    hk.expand(HKDF_INFO_CONTROL, &mut control_key)
        .map_err(|e| NidanError::Crypto(format!("HKDF expand control: {}", e)))?;

    Ok(SessionKeys {
        video: SessionKey { key: video_key },
        audio: SessionKey { key: audio_key },
        control: SessionKey { key: control_key },
    })
}

/// Chiffreur/déchiffreur ChaCha20-Poly1305 pour un stream donné.
/// Maintient un compteur de nonce monotone pour éviter les réutilisations.
pub struct StreamCipher {
    cipher: ChaCha20Poly1305,
    /// Compteur de frame, utilisé pour construire le nonce (jamais réutilisé)
    frame_counter: u64,
}

impl StreamCipher {
    /// Crée un nouveau StreamCipher depuis une SessionKey
    pub fn new(key: &SessionKey) -> Self {
        let k = Key::from_slice(key.as_bytes());
        Self {
            cipher: ChaCha20Poly1305::new(k),
            frame_counter: 0,
        }
    }

    /// Génère le nonce pour la frame courante.
    /// Structure : [counter 8 bytes LE] [padding 4 bytes zéro]
    fn nonce_for_counter(counter: u64) -> [u8; CHACHA20_NONCE_SIZE] {
        let mut nonce = [0u8; CHACHA20_NONCE_SIZE];
        nonce[..8].copy_from_slice(&counter.to_le_bytes());
        nonce
    }

    /// Chiffre une frame. Retourne (ciphertext, nonce).
    pub fn encrypt(&mut self, plaintext: &[u8]) -> NidanResult<(Vec<u8>, [u8; CHACHA20_NONCE_SIZE])> {
        let nonce_bytes = Self::nonce_for_counter(self.frame_counter);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| NidanError::Crypto(format!("encrypt: {}", e)))?;

        self.frame_counter = self
            .frame_counter
            .checked_add(1)
            .ok_or_else(|| NidanError::Crypto("compteur de frame saturé".to_string()))?;

        Ok((ciphertext, nonce_bytes))
    }

    /// Déchiffre une frame avec le nonce fourni.
    pub fn decrypt(&self, ciphertext: &[u8], nonce_bytes: &[u8]) -> NidanResult<Vec<u8>> {
        if nonce_bytes.len() != CHACHA20_NONCE_SIZE {
            return Err(NidanError::Crypto(format!(
                "nonce invalide: {} bytes attendus, {} reçus",
                CHACHA20_NONCE_SIZE,
                nonce_bytes.len()
            )));
        }
        let nonce = Nonce::from_slice(nonce_bytes);
        self.cipher
            .decrypt(nonce, ciphertext)
            .map_err(|_| NidanError::Crypto("déchiffrement échoué (AEAD)".to_string()))
    }
}

/// Génère `n` bytes aléatoires cryptographiquement sûrs
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    OsRng.fill_bytes(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key_bytes = random_bytes(SESSION_KEY_SIZE);
        let key = SessionKey::from_bytes(&key_bytes).unwrap();
        let mut cipher = StreamCipher::new(&key);

        let plaintext = b"frame video test nidan";
        let (ciphertext, nonce) = cipher.encrypt(plaintext).unwrap();
        assert_ne!(ciphertext, plaintext);

        let decrypted = cipher.decrypt(&ciphertext, &nonce).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_nonce_monotone() {
        let key_bytes = random_bytes(SESSION_KEY_SIZE);
        let key = SessionKey::from_bytes(&key_bytes).unwrap();
        let mut cipher = StreamCipher::new(&key);

        let (_, n1) = cipher.encrypt(b"frame1").unwrap();
        let (_, n2) = cipher.encrypt(b"frame2").unwrap();
        assert_ne!(n1, n2, "les nonces doivent être distincts");
    }

    #[test]
    fn test_tampered_ciphertext_fails() {
        let key_bytes = random_bytes(SESSION_KEY_SIZE);
        let key = SessionKey::from_bytes(&key_bytes).unwrap();
        let mut cipher = StreamCipher::new(&key);

        let (mut ciphertext, nonce) = cipher.encrypt(b"donnees sensibles").unwrap();
        // Altération d'un byte
        ciphertext[0] ^= 0xFF;
        assert!(cipher.decrypt(&ciphertext, &nonce).is_err(), "AEAD doit détecter la falsification");
    }

    #[test]
    fn test_derive_session_keys() {
        let secret = random_bytes(32);
        let cn = random_bytes(32);
        let sn = random_bytes(32);
        let keys = derive_session_keys(&secret, &cn, &sn).unwrap();
        // Les trois clés doivent être distinctes
        assert_ne!(keys.video.key, keys.audio.key);
        assert_ne!(keys.video.key, keys.control.key);
        assert_ne!(keys.audio.key, keys.control.key);
    }
}
