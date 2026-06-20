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

use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};

/// Paire de clés éphémère X25519 pour l'échange ECDH d'une session.
///
/// Chaque pair (client et serveur) génère sa propre paire, échange la clé
/// publique dans le handshake, puis calcule le même secret partagé.
pub struct KeyExchange {
    secret: StaticSecret,
    pub public: [u8; 32],
}

impl KeyExchange {
    /// Génère une nouvelle paire éphémère.
    pub fn new() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret).to_bytes();
        Self { secret, public }
    }

    /// Calcule le secret partagé ECDH à partir de la clé publique du pair.
    pub fn shared_secret(&self, peer_public: &[u8]) -> NidanResult<[u8; 32]> {
        if peer_public.len() != 32 {
            return Err(NidanError::Crypto(format!(
                "clé publique X25519 invalide: {} bytes attendus, {} reçus",
                32,
                peer_public.len()
            )));
        }
        let mut pk = [0u8; 32];
        pk.copy_from_slice(peer_public);
        let peer = PublicKey::from(pk);
        let shared = self.secret.diffie_hellman(&peer);
        Ok(shared.to_bytes())
    }
}

impl Default for KeyExchange {
    fn default() -> Self {
        Self::new()
    }
}

// EphemeralSecret est importé pour documenter l'intention ; StaticSecret est
// utilisé car il permet de conserver le secret jusqu'au calcul du shared secret
// après réception de la clé publique du pair (l'éphémère est consommé à l'usage).
#[allow(unused_imports)]
use EphemeralSecret as _UnusedEphemeral;

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

    #[test]
    fn test_x25519_ecdh_shared_secret() {
        // Deux parties génèrent leurs paires
        let client = KeyExchange::new();
        let server = KeyExchange::new();

        // Chacun calcule le secret partagé avec la clé publique de l'autre
        let client_shared = client.shared_secret(&server.public).unwrap();
        let server_shared = server.shared_secret(&client.public).unwrap();

        // Les deux secrets doivent être identiques (propriété ECDH)
        assert_eq!(client_shared, server_shared, "les secrets ECDH divergent");
        assert_ne!(client_shared, [0u8; 32], "secret nul");
    }

    #[test]
    fn test_full_e2e_handshake_and_encrypt() {
        // Simule le handshake complet client <-> serveur
        let client_kx = KeyExchange::new();
        let server_kx = KeyExchange::new();
        let client_nonce = random_bytes(32);
        let server_nonce = random_bytes(32);

        // Chaque côté calcule le secret partagé
        let client_secret = client_kx.shared_secret(&server_kx.public).unwrap();
        let server_secret = server_kx.shared_secret(&client_kx.public).unwrap();
        assert_eq!(client_secret, server_secret);

        // Chaque côté dérive les mêmes clés de session
        let client_keys = derive_session_keys(&client_secret, &client_nonce, &server_nonce).unwrap();
        let server_keys = derive_session_keys(&server_secret, &client_nonce, &server_nonce).unwrap();
        assert_eq!(client_keys.video.as_bytes(), server_keys.video.as_bytes());

        // Le serveur chiffre une "frame vidéo", le client la déchiffre
        let mut server_cipher = StreamCipher::new(&server_keys.video);
        let client_cipher = StreamCipher::new(&client_keys.video);

        let frame = b"frame video H.264 confidentielle";
        let (ciphertext, nonce) = server_cipher.encrypt(frame).unwrap();
        assert_ne!(&ciphertext[..], &frame[..], "le texte n'est pas chiffré");

        let decrypted = client_cipher.decrypt(&ciphertext, &nonce).unwrap();
        assert_eq!(&decrypted[..], &frame[..], "déchiffrement E2E incorrect");
    }

}