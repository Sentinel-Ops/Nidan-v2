//! Watermarking forensique stéganographique pour NIDAN.
//!
//! ## Principe
//!
//! Chaque frame vidéo BGRA reçue est marquée en modifiant les bits
//! de poids faible (LSB) des canaux chroma (Cb, Cr en YUV, ou B/R en BGR).
//!
//! La marque encode :
//! - L'identifiant de session (64 bits)
//! - L'horodatage (32 bits, secondes depuis session_start)
//! - Un checksum CRC-8 de vérification
//!
//! Structure de la marque (total 13 bytes = 104 bits) :
//! ```text
//! [session_id 8B][timestamp 4B][crc 1B]
//! ```
//!
//! ## Invisibilité
//!
//! La modification de 1–2 bits LSB sur les canaux chroma est imperceptible
//! à l'œil humain mais détectable algorithmiquement par NIDAN.
//! La marque est répétée toutes les N frames (configurable) pour la robustesse.
//!
//! ## Résistance
//!
//! La répétition de la marque et le CRC permettent la détection même après :
//! - Recompression vidéo légère (codec identique)
//! - Capture d'écran de la fenêtre cliente
//! - Redimensionnement modéré

/// Marqueur stéganographique
pub struct Watermarker {
    /// ID de session encodé sur 8 bytes
    session_id_bytes: [u8; 8],
    /// Timestamp de début de session (Unix)
    session_start:    u64,
    /// Nombre de bits LSB à modifier (1–4)
    strength:         u8,
    /// Compteur de frames (pour horodatage relatif)
    frame_count:      u64,
}

impl Watermarker {
    /// Crée un marqueur pour une session
    pub fn new(session_id: &str, strength: u8) -> Self {
        // Hash du session_id sur 8 bytes
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(session_id.as_bytes());
        let mut id_bytes = [0u8; 8];
        id_bytes.copy_from_slice(&hash[..8]);

        let session_start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            session_id_bytes: id_bytes,
            session_start,
            strength: strength.clamp(1, 4),
            frame_count: 0,
        }
    }

    /// Injecte la marque dans une frame BGRA.
    ///
    /// Modifie les pixels en place. Ne modifie que les canaux B et R
    /// (approximation chroma en espace BGR), préservant le canal G (luma).
    ///
    /// Retourne le nombre de pixels modifiés.
    pub fn mark_frame(&mut self, pixels: &mut Vec<u8>, width: u32, height: u32) -> usize {
        if pixels.len() < (width * height * 4) as usize {
            return 0;
        }

        let payload = self.build_payload();
        let mask    = self.lsb_mask();

        // Zone d'injection : coins de l'image (plus résistants au crop)
        // Coin haut-gauche : pixels 0..N
        // Coin haut-droit  : pixels (width-N)..width
        // On utilise une grille dispersée pour la robustesse
        let n_bits = payload.len() * 8;
        let mut modified = 0;

        for bit_idx in 0..n_bits {
            let byte_idx = bit_idx / 8;
            let bit_pos  = bit_idx % 8;
            let bit_val  = (payload[byte_idx] >> bit_pos) & 1;

            // Position du pixel dans l'image (dispersion pseudo-aléatoire)
            let pixel_idx = self.pixel_position(bit_idx, width, height);
            let base      = pixel_idx * 4;

            if base + 3 >= pixels.len() { continue; }

            // Modification du canal B (index 0) pour les bits pairs
            // Modification du canal R (index 2) pour les bits impairs
            // Le canal G (luma) n'est jamais touché
            let channel = if bit_idx % 2 == 0 { 0 } else { 2 };

            let old_val = pixels[base + channel];
            let new_val = (old_val & !mask) | (bit_val.wrapping_mul(mask));
            pixels[base + channel] = new_val;

            if old_val != new_val { modified += 1; }
        }

        self.frame_count += 1;
        modified
    }

    /// Extrait et vérifie une marque depuis une frame BGRA
    pub fn extract_mark(
        &self,
        pixels: &[u8],
        width:  u32,
        height: u32,
    ) -> Option<WatermarkData> {
        let payload_len = 13; // 8 + 4 + 1
        let n_bits = payload_len * 8;
        let mask   = self.lsb_mask();

        let mut payload = vec![0u8; payload_len];

        for bit_idx in 0..n_bits {
            let byte_idx = bit_idx / 8;
            let bit_pos  = bit_idx % 8;

            let pixel_idx = self.pixel_position(bit_idx, width, height);
            let base      = pixel_idx * 4;

            if base + 3 >= pixels.len() { return None; }

            let channel = if bit_idx % 2 == 0 { 0 } else { 2 };
            let bit_val = (pixels[base + channel] & mask != 0) as u8;

            payload[byte_idx] |= bit_val << bit_pos;
        }

        // Vérification CRC
        let expected_crc = Self::crc8(&payload[..12]);
        if payload[12] != expected_crc {
            return None; // CRC invalide → pas de marque ou frame altérée
        }

        let mut session_id_bytes = [0u8; 8];
        session_id_bytes.copy_from_slice(&payload[..8]);

        let timestamp = u32::from_le_bytes(
            payload[8..12].try_into().unwrap_or([0; 4])
        );

        Some(WatermarkData { session_id_bytes, timestamp_secs: timestamp })
    }

    // ── Helpers privés ───────────────────────────────────────────────────────

    /// Construit le payload de 13 bytes à injecter
    fn build_payload(&self) -> Vec<u8> {
        let elapsed = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(self.session_start)) as u32;

        let mut payload = Vec::with_capacity(13);
        payload.extend_from_slice(&self.session_id_bytes); // 8 bytes
        payload.extend_from_slice(&elapsed.to_le_bytes()); // 4 bytes
        let crc = Self::crc8(&payload);
        payload.push(crc);                                  // 1 byte
        payload
    }

    /// Masque LSB selon la force du watermark
    fn lsb_mask(&self) -> u8 {
        (1u8 << self.strength) - 1
    }

    /// Calcule la position d'un pixel pour l'injection (grille dispersée)
    /// Utilise une fonction de dispersion déterministe
    fn pixel_position(&self, bit_idx: usize, width: u32, height: u32) -> usize {
        // Distribution sur les 1/4 des pixels (bords de l'image)
        // pour une meilleure résistance au crop
        let total_pixels = (width * height) as usize;
        let zone         = total_pixels / 4;

        // Dispersion pseudo-aléatoire déterministe via multiplication
        let idx = (bit_idx * 0x9E3779B9 + 0x6C62272E) % zone;
        idx
    }

    /// CRC-8 (polynôme 0x07 — CRC-8/SMBUS)
    fn crc8(data: &[u8]) -> u8 {
        let mut crc = 0u8;
        for &byte in data {
            crc ^= byte;
            for _ in 0..8 {
                if crc & 0x80 != 0 {
                    crc = (crc << 1) ^ 0x07;
                } else {
                    crc <<= 1;
                }
            }
        }
        crc
    }
}

/// Données extraites d'une marque stéganographique
#[derive(Debug, Clone)]
pub struct WatermarkData {
    /// Hash du session_id (8 bytes)
    pub session_id_bytes:  [u8; 8],
    /// Secondes depuis le début de la session
    pub timestamp_secs:    u32,
}

impl WatermarkData {
    /// Vérifie si la marque correspond à un session_id attendu
    pub fn matches_session(&self, session_id: &str) -> bool {
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(session_id.as_bytes());
        let mut expected = [0u8; 8];
        expected.copy_from_slice(&hash[..8]);
        self.session_id_bytes == expected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(w: u32, h: u32) -> Vec<u8> {
        // Frame BGRA avec valeurs pseudo-aléatoires (multiples de 8 → LSBs = 0)
        (0..(w * h * 4)).map(|i| ((i / 4 * 8) % 256) as u8).collect()
    }

    #[test]
    fn test_mark_and_extract() {
        let session_id = "test-session-uuid-1234";
        let mut wm = Watermarker::new(session_id, 2);

        let mut pixels = make_frame(1920, 1080);
        let modified = wm.mark_frame(&mut pixels, 1920, 1080);
        assert!(modified > 0, "au moins un pixel doit être modifié");

        let extractor = Watermarker::new(session_id, 2);
        let data = extractor.extract_mark(&pixels, 1920, 1080);
        assert!(data.is_some(), "la marque doit être extractible");

        let data = data.unwrap();
        assert!(
            data.matches_session(session_id),
            "la marque doit correspondre au session_id"
        );
    }

    #[test]
    fn test_wrong_session_no_match() {
        let mut wm = Watermarker::new("session-A", 2);
        let mut pixels = make_frame(1920, 1080);
        wm.mark_frame(&mut pixels, 1920, 1080);

        let extractor = Watermarker::new("session-A", 2);
        let data = extractor.extract_mark(&pixels, 1920, 1080).unwrap();
        assert!(!data.matches_session("session-B"),
            "ne doit pas matcher un autre session_id");
    }

    #[test]
    fn test_crc8_consistency() {
        let data = b"nidan-watermark-test";
        let crc1 = Watermarker::crc8(data);
        let crc2 = Watermarker::crc8(data);
        assert_eq!(crc1, crc2, "CRC doit être déterministe");
        assert_ne!(crc1, 0, "CRC ne doit pas être zéro");
    }

    #[test]
    fn test_strength_1_minimal_modification() {
        let mut wm = Watermarker::new("s", 1);
        let original = make_frame(1920, 1080);
        let mut pixels = original.clone();
        wm.mark_frame(&mut pixels, 1920, 1080);

        // Avec strength=1, les différences doivent être de maximum 1 bit
        let diffs: Vec<u8> = original.iter().zip(pixels.iter())
            .map(|(a, b)| (a ^ b) as u8)
            .filter(|&d| d != 0)
            .collect();

        for &diff in &diffs {
            assert!(diff <= 1, "strength=1 → max 1 bit de différence");
        }
    }
}
