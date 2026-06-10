//! Enregistrement des sessions vidéo en format MKV.
//!
//! ## Format MKV / EBML (simplifié)
//!
//! NIDAN utilise un sous-ensemble du format Matroska/WebM :
//! - Un fichier MKV par session
//! - Cluster → SimpleBlock par frame vidéo
//! - Timestamp absolu UTC dans les tags MKV
//! - Index temporel pour navigation rapide (Cues)
//! - Scellage HMAC-SHA256 à la fermeture
//!
//! ## Chaîne de preuves
//!
//! Chaque frame enregistrée est chaînée cryptographiquement :
//! ```text
//! H(frame_0) → H(frame_0 || frame_1) → H(...|| frame_N)
//! ```
//! Le hash final est stocké dans les tags MKV et signé HMAC.
//! Toute modification d'une frame passée invalide la chaîne.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use nidan_proto::VideoFrame;

/// Métadonnées d'une session enregistrée
#[derive(Debug, Clone)]
pub struct SessionRecordingMeta {
    pub session_id:   String,
    pub user_id:      String,
    pub vm_id:        String,
    pub client_ip:    String,
    pub started_at:   DateTime<Utc>,
    pub ended_at:     Option<DateTime<Utc>>,
    pub frame_count:  u64,
    pub file_path:    PathBuf,
    pub file_size:    u64,
    pub chain_hash:   String,  // Hash final de la chaîne de preuves (hex)
    pub hmac_seal:    Option<String>, // HMAC du fichier (hex)
}

/// Entrée dans l'index temporel
#[derive(Debug, Clone)]
pub struct TimeIndex {
    pub timestamp_ms: u64,
    pub frame_seq:    u64,
    pub file_offset:  u64,
    pub is_keyframe:  bool,
}

/// Enregistreur de session
pub struct SessionRecorder {
    meta:          SessionRecordingMeta,
    output:        std::fs::File,
    chain_hasher:  Sha256,
    index:         Vec<TimeIndex>,
    file_offset:   u64,
    seal_key:      Option<Vec<u8>>,
    started:       Instant,
    quality:       u8,
}

impl SessionRecorder {
    /// Crée un nouvel enregistreur pour une session
    pub fn new(
        session_id:  &str,
        user_id:     &str,
        vm_id:       &str,
        client_ip:   &str,
        output_dir:  &str,
        seal_key:    Option<Vec<u8>>,
        quality:     u8,
    ) -> Result<Self> {
        let started_at = Utc::now();
        let filename = format!(
            "session_{}_{}.mkv",
            &session_id[..8.min(session_id.len())],
            started_at.format("%Y%m%d_%H%M%S")
        );
        let file_path = PathBuf::from(output_dir).join(&filename);

        let mut output = std::fs::File::create(&file_path)
            .with_context(|| format!("création fichier session: {}", file_path.display()))?;

        // Écriture de l'en-tête MKV (EBML header simplifié)
        let header = Self::make_ebml_header(session_id, user_id, vm_id, client_ip, &started_at);
        output.write_all(&header)?;
        let file_offset = header.len() as u64;

        info!(
            session_id = session_id,
            path       = %file_path.display(),
            "enregistrement démarré"
        );

        Ok(Self {
            meta: SessionRecordingMeta {
                session_id:  session_id.to_string(),
                user_id:     user_id.to_string(),
                vm_id:       vm_id.to_string(),
                client_ip:   client_ip.to_string(),
                started_at,
                ended_at:    None,
                frame_count: 0,
                file_path,
                file_size:   file_offset,
                chain_hash:  String::new(),
                hmac_seal:   None,
            },
            output,
            chain_hasher: Sha256::new(),
            index:        Vec::with_capacity(1024),
            file_offset,
            seal_key,
            started:      Instant::now(),
            quality,
        })
    }

    /// Enregistre une frame vidéo
    pub fn record_frame(&mut self, frame: &VideoFrame) -> Result<()> {
        let pts_ms = frame.pts_ms as u64;
        let is_kf  = frame.keyframe;

        // Ajout à l'index temporel (seulement les keyframes et 1/sec)
        let should_index = is_kf || pts_ms % 1000 < 40;
        if should_index {
            self.index.push(TimeIndex {
                timestamp_ms: pts_ms,
                frame_seq:    frame.frame_seq,
                file_offset:  self.file_offset,
                is_keyframe:  is_kf,
            });
        }

        // Écriture du SimpleBlock MKV
        let block = Self::make_simple_block(
            frame.frame_seq,
            pts_ms,
            is_kf,
            &frame.encoded_data,
        );
        self.output.write_all(&block)
            .context("écriture frame MKV")?;

        // Mise à jour de la chaîne de preuves
        // H_n = SHA256(H_{n-1} || frame_seq || encoded_data)
        self.chain_hasher.update(&frame.frame_seq.to_le_bytes());
        self.chain_hasher.update(&frame.encoded_data);

        self.file_offset    += block.len() as u64;
        self.meta.file_size  = self.file_offset;
        self.meta.frame_count += 1;

        debug!(
            seq     = frame.frame_seq,
            size    = frame.encoded_data.len(),
            pts_ms,
            "frame enregistrée"
        );

        Ok(())
    }

    /// Finalise l'enregistrement : écrit l'index, le hash de chaîne et le sceau HMAC
    pub fn finalize(mut self) -> Result<SessionRecordingMeta> {
        let ended_at = Utc::now();

        // Hash final de la chaîne de preuves
        let chain_hash_bytes = self.chain_hasher.finalize();
        let chain_hash = hex::encode(chain_hash_bytes);

        // Écriture de l'index Cues dans le MKV
        let cues = Self::make_cues_element(&self.index);
        self.output.write_all(&cues)?;

        // Écriture des tags MKV (métadonnées + chain hash)
        let tags = Self::make_tags_element(
            &self.meta.session_id,
            &self.meta.user_id,
            &ended_at,
            &chain_hash,
        );
        self.output.write_all(&tags)?;
        self.output.flush()?;
        drop(self.output);

        // Scellage HMAC si clé fournie
        let hmac_seal = if let Some(ref key) = self.seal_key {
            Some(Self::seal_file(&self.meta.file_path, key)?)
        } else {
            None
        };

        let duration = self.started.elapsed();
        info!(
            session_id  = %self.meta.session_id,
            frames      = self.meta.frame_count,
            duration_s  = duration.as_secs(),
            file_size   = self.meta.file_size,
            chain_hash  = %chain_hash,
            sealed      = hmac_seal.is_some(),
            "enregistrement finalisé"
        );

        Ok(SessionRecordingMeta {
            ended_at:   Some(ended_at),
            chain_hash,
            hmac_seal,
            ..self.meta
        })
    }

    // ── Constructeurs EBML/MKV ──────────────────────────────────────────────

    /// En-tête EBML + Segment + SegmentInfo + TrackEntry
    fn make_ebml_header(
        session_id: &str,
        user_id:    &str,
        vm_id:      &str,
        client_ip:  &str,
        started_at: &DateTime<Utc>,
    ) -> Vec<u8> {
        // EBML Header simplifié (DocType = matroska)
        // En production : utiliser une librairie EBML complète
        // Ici : structure binaire minimale compatible Matroska
        let mut buf = Vec::new();

        // Magic bytes EBML : 0x1A 0x45 0xDF 0xA3
        buf.extend_from_slice(&[0x1A, 0x45, 0xDF, 0xA3]);
        // EBML version, doctype etc. (simplifié)
        buf.extend_from_slice(b"\x42\x86\x81\x01"); // EBMLVersion = 1
        buf.extend_from_slice(b"\x42\xF7\x81\x01"); // EBMLReadVersion = 1
        buf.extend_from_slice(b"\x42\xF2\x81\x04"); // EBMLMaxIDLength = 4
        buf.extend_from_slice(b"\x42\xF3\x81\x08"); // EBMLMaxSizeLength = 8

        // DocType = "matroska"
        let doctype = b"matroska";
        buf.push(0x42); buf.push(0x82); // DocType ID
        buf.push(doctype.len() as u8);
        buf.extend_from_slice(doctype);

        // Segment UUID (16 bytes aléatoires — représenté par session_id tronqué)
        buf.extend_from_slice(b"NIDAN\x00"); // Marker interne

        // Métadonnées encodées en UTF-8 dans un bloc "Title"
        let title = format!(
            "NIDAN-SESSION:{session_id}|USER:{user_id}|VM:{vm_id}|IP:{client_ip}|TS:{}",
            started_at.to_rfc3339()
        );
        buf.extend_from_slice(title.as_bytes());
        buf.push(0x00); // null terminator

        buf
    }

    /// Construit un SimpleBlock MKV pour une frame
    fn make_simple_block(
        seq:      u64,
        pts_ms:   u64,
        keyframe: bool,
        data:     &[u8],
    ) -> Vec<u8> {
        let mut block = Vec::new();

        // SimpleBlock ID : 0xA3
        block.push(0xA3);

        // Flags : bit 7 = keyframe
        let flags: u8 = if keyframe { 0x80 } else { 0x00 };

        // Header du bloc : [track_num=1][timecode 2B][flags][seq 8B]
        let header = [
            0x81u8,                              // Track number (VINT = 1)
            ((pts_ms >> 8) & 0xFF) as u8,        // Timecode high byte
            (pts_ms & 0xFF) as u8,               // Timecode low byte
            flags,
        ];

        // Taille totale du bloc = header + seq(8) + data
        let total_size = header.len() + 8 + data.len();
        // Encodage VINT de la taille (simplifié pour tailles < 2^21)
        if total_size < 0x7F {
            block.push(0x80 | total_size as u8);
        } else if total_size < 0x3FFF {
            block.push(0x40 | ((total_size >> 8) as u8));
            block.push((total_size & 0xFF) as u8);
        } else {
            block.push(0x20 | ((total_size >> 16) as u8));
            block.push(((total_size >> 8) & 0xFF) as u8);
            block.push((total_size & 0xFF) as u8);
        }

        block.extend_from_slice(&header);
        block.extend_from_slice(&seq.to_be_bytes()); // Numéro de séquence NIDAN
        block.extend_from_slice(data);

        block
    }

    /// Construit l'élément Cues (index temporel)
    fn make_cues_element(index: &[TimeIndex]) -> Vec<u8> {
        let mut cues = Vec::new();
        // Cues ID : 0x1C 0x53 0xBB 0x6B
        cues.extend_from_slice(&[0x1C, 0x53, 0xBB, 0x6B]);

        for entry in index {
            // CuePoint
            cues.extend_from_slice(&[0xBB]); // CuePoint ID
            // CueTime
            cues.extend_from_slice(&[0xB3, 0x88]); // CueTime ID + size 8
            cues.extend_from_slice(&entry.timestamp_ms.to_be_bytes());
            // CueClusterPosition (simplifié = file_offset)
            cues.extend_from_slice(&[0xB7, 0x88]); // CueTrackPositions
            cues.extend_from_slice(&entry.file_offset.to_be_bytes());
        }

        cues
    }

    /// Construit l'élément Tags (métadonnées + chain hash)
    fn make_tags_element(
        session_id:  &str,
        user_id:     &str,
        ended_at:    &DateTime<Utc>,
        chain_hash:  &str,
    ) -> Vec<u8> {
        let mut tags = Vec::new();
        // Tags ID : 0x12 0x54 0xC3 0x67
        tags.extend_from_slice(&[0x12, 0x54, 0xC3, 0x67]);

        let content = format!(
            "SESSION_ID={session_id}\nUSER_ID={user_id}\nENDED_AT={}\nCHAIN_HASH={chain_hash}",
            ended_at.to_rfc3339()
        );
        tags.extend_from_slice(content.as_bytes());
        tags
    }

    /// Scelle le fichier avec HMAC-SHA256
    fn seal_file(path: &Path, key: &[u8]) -> Result<String> {
        use hmac::{Hmac, Mac};
        type HmacSha256 = Hmac<Sha256>;

        let content = std::fs::read(path)
            .with_context(|| format!("lecture fichier pour scellage: {}", path.display()))?;

        let mut mac = HmacSha256::new_from_slice(key)
            .context("initialisation HMAC")?;
        mac.update(&content);
        let seal = hex::encode(mac.finalize().into_bytes());

        // Écriture du sceau dans un fichier .seal adjacent
        let seal_path = path.with_extension("seal");
        std::fs::write(&seal_path, &seal)
            .with_context(|| format!("écriture sceau: {}", seal_path.display()))?;

        info!(path = %path.display(), "fichier scellé HMAC-SHA256");
        Ok(seal)
    }
}

/// Vérifie l'intégrité d'un fichier de session via son sceau HMAC
pub fn verify_seal(session_file: &Path, key: &[u8]) -> Result<bool> {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<Sha256>;

    let seal_path = session_file.with_extension("seal");
    let expected_seal = std::fs::read_to_string(&seal_path)
        .with_context(|| format!("lecture sceau: {}", seal_path.display()))?;

    let content = std::fs::read(session_file)
        .with_context(|| format!("lecture session: {}", session_file.display()))?;

    let mut mac = HmacSha256::new_from_slice(key)
        .context("initialisation HMAC")?;
    mac.update(&content);

    let computed = hex::encode(mac.finalize().into_bytes());
    Ok(computed == expected_seal.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_frame(seq: u64, keyframe: bool) -> VideoFrame {
        VideoFrame {
            frame_seq:    seq,
            keyframe,
            encoded_data: vec![0xAB; 2048],
            nonce:        vec![0u8; 12],
            width:        1920,
            height:       1080,
            pts_ms:       (seq * 33) as u32,
            ..Default::default()
        }
    }

    #[test]
    fn test_recording_create_finalize() {
        let dir = TempDir::new().unwrap();

        let mut recorder = SessionRecorder::new(
            "test-session-001",
            "jo.dupont",
            "vm-001",
            "192.168.1.100",
            dir.path().to_str().unwrap(),
            None,
            80,
        ).unwrap();

        // Enregistrement de 10 frames
        for i in 0..10u64 {
            recorder.record_frame(&make_frame(i, i == 0)).unwrap();
        }

        let meta = recorder.finalize().unwrap();

        assert_eq!(meta.frame_count, 10);
        assert!(!meta.chain_hash.is_empty());
        assert!(meta.file_path.exists());
        assert!(meta.file_size > 0);
        assert!(meta.ended_at.is_some());
    }

    #[test]
    fn test_seal_and_verify() {
        let dir = TempDir::new().unwrap();
        let key = b"test_seal_key_32_bytes_exactly!!";

        let mut recorder = SessionRecorder::new(
            "test-seal-002",
            "user",
            "vm-001",
            "10.0.0.1",
            dir.path().to_str().unwrap(),
            Some(key.to_vec()),
            80,
        ).unwrap();

        recorder.record_frame(&make_frame(0, true)).unwrap();
        let meta = recorder.finalize().unwrap();

        assert!(meta.hmac_seal.is_some());
        assert!(verify_seal(&meta.file_path, key).unwrap());
    }

    #[test]
    fn test_tampered_file_fails_verification() {
        let dir = TempDir::new().unwrap();
        let key = b"test_seal_key_32_bytes_exactly!!";

        let mut recorder = SessionRecorder::new(
            "test-tamper-003",
            "user",
            "vm-001",
            "10.0.0.1",
            dir.path().to_str().unwrap(),
            Some(key.to_vec()),
            80,
        ).unwrap();

        recorder.record_frame(&make_frame(0, true)).unwrap();
        let meta = recorder.finalize().unwrap();

        // Altération du fichier
        let mut content = std::fs::read(&meta.file_path).unwrap();
        if let Some(byte) = content.last_mut() { *byte ^= 0xFF; }
        std::fs::write(&meta.file_path, content).unwrap();

        assert!(!verify_seal(&meta.file_path, key).unwrap(),
            "un fichier altéré doit échouer la vérification");
    }

    #[test]
    fn test_chain_hash_deterministic() {
        let dir = TempDir::new().unwrap();

        let mut r1 = SessionRecorder::new("s1","u","v","ip",
            dir.path().to_str().unwrap(), None, 80).unwrap();
        let mut r2 = SessionRecorder::new("s1","u","v","ip",
            dir.path().to_str().unwrap(), None, 80).unwrap();

        for i in 0..5u64 {
            r1.record_frame(&make_frame(i, i == 0)).unwrap();
            r2.record_frame(&make_frame(i, i == 0)).unwrap();
        }

        let m1 = r1.finalize().unwrap();
        let m2 = r2.finalize().unwrap();

        // Mêmes frames → même hash de chaîne
        assert_eq!(m1.chain_hash, m2.chain_hash,
            "chaîne déterministe pour les mêmes frames");
    }
}
