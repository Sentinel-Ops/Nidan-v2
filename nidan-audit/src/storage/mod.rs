//! Gestion du stockage WORM des enregistrements de session.
//!
//! WORM = Write Once Read Many.
//! Une fois scellé, un fichier ne peut plus être modifié sans invalider le HMAC.
//!
//! ## Cycle de vie d'un fichier de session
//! ```text
//! recording/    ← en cours d'écriture
//!      ↓
//! finalize()    ← écriture terminée, HMAC calculé
//!      ↓
//! sessions/     ← stockage long terme, lecture seule
//!      ↓
//! [rotation]    ← après retention_days, archivage ou suppression
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Index des sessions enregistrées
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionIndex {
    pub entries: Vec<SessionIndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionIndexEntry {
    pub session_id:  String,
    pub user_id:     String,
    pub vm_id:       String,
    pub started_at:  DateTime<Utc>,
    pub ended_at:    Option<DateTime<Utc>>,
    pub file_name:   String,
    pub file_size:   u64,
    pub frame_count: u64,
    pub chain_hash:  String,
    pub sealed:      bool,
}

/// Gestionnaire de stockage WORM
pub struct WormStorage {
    base_dir:      PathBuf,
    retention_days: u32,
    max_storage_mb: u64,
}

impl WormStorage {
    pub fn new(base_dir: &str, retention_days: u32, max_storage_mb: u64) -> Result<Self> {
        let base = PathBuf::from(base_dir);
        std::fs::create_dir_all(&base)
            .with_context(|| format!("création répertoire WORM: {}", base.display()))?;

        Ok(Self {
            base_dir: base,
            retention_days,
            max_storage_mb,
        })
    }

    /// Charge ou crée l'index des sessions
    pub fn load_index(&self) -> SessionIndex {
        let index_path = self.base_dir.join("index.json");
        if let Ok(content) = std::fs::read_to_string(&index_path) {
            serde_json::from_str(&content).unwrap_or_else(|_| SessionIndex { entries: vec![] })
        } else {
            SessionIndex { entries: vec![] }
        }
    }

    /// Sauvegarde l'index
    pub fn save_index(&self, index: &SessionIndex) -> Result<()> {
        let index_path = self.base_dir.join("index.json");
        let content = serde_json::to_string_pretty(index)?;
        std::fs::write(&index_path, content)
            .context("écriture index sessions")
    }

    /// Ajoute une entrée à l'index
    pub fn register_session(&self, entry: SessionIndexEntry) -> Result<()> {
        let mut index = self.load_index();
        index.entries.push(entry);
        self.save_index(&index)
    }

    /// Retourne les stats de stockage
    pub fn storage_stats(&self) -> StorageStats {
        let total_size = self.compute_total_size();
        let file_count = self.load_index().entries.len();
        let oldest     = self.load_index().entries.iter()
            .map(|e| e.started_at)
            .min();

        StorageStats {
            total_size_bytes: total_size,
            total_size_mb:    total_size / (1024 * 1024),
            file_count,
            oldest_session:   oldest,
        }
    }

    /// Applique la politique de rétention — supprime les fichiers expirés
    pub fn apply_retention(&self) -> Result<usize> {
        if self.retention_days == 0 { return Ok(0); }

        let cutoff = Utc::now() - chrono::Duration::days(self.retention_days as i64);
        let mut index = self.load_index();
        let initial   = index.entries.len();

        let mut to_delete = vec![];
        index.entries.retain(|e| {
            if e.started_at < cutoff {
                to_delete.push(e.file_name.clone());
                false
            } else {
                true
            }
        });

        for filename in &to_delete {
            let path = self.base_dir.join(filename);
            if let Err(e) = std::fs::remove_file(&path) {
                warn!(error = %e, file = %path.display(), "suppression fichier expiré échouée");
            } else {
                info!(file = %filename, "fichier de session expiré supprimé");
            }
            // Supprimer le sceau aussi
            let seal = self.base_dir.join(
                PathBuf::from(filename).with_extension("seal")
            );
            let _ = std::fs::remove_file(seal);
        }

        self.save_index(&index)?;
        Ok(initial - index.entries.len())
    }

    /// Vérifie si la limite de stockage est atteinte
    pub fn is_storage_full(&self) -> bool {
        if self.max_storage_mb == 0 { return false; }
        let used_mb = self.compute_total_size() / (1024 * 1024);
        used_mb >= self.max_storage_mb
    }

    fn compute_total_size(&self) -> u64 {
        std::fs::read_dir(&self.base_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0)
    }
}

#[derive(Debug)]
pub struct StorageStats {
    pub total_size_bytes: u64,
    pub total_size_mb:    u64,
    pub file_count:       usize,
    pub oldest_session:   Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_index_persist_reload() {
        let dir  = TempDir::new().unwrap();
        let worm = WormStorage::new(dir.path().to_str().unwrap(), 30, 0).unwrap();

        let entry = SessionIndexEntry {
            session_id:  "sess-001".to_string(),
            user_id:     "jo".to_string(),
            vm_id:       "vm-001".to_string(),
            started_at:  Utc::now(),
            ended_at:    Some(Utc::now()),
            file_name:   "session_abc.mkv".to_string(),
            file_size:   1024,
            frame_count: 300,
            chain_hash:  "deadbeef".to_string(),
            sealed:      true,
        };

        worm.register_session(entry).unwrap();

        let index = worm.load_index();
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].session_id, "sess-001");
    }

    #[test]
    fn test_retention_removes_old_files() {
        let dir  = TempDir::new().unwrap();
        let worm = WormStorage::new(dir.path().to_str().unwrap(), 1, 0).unwrap();

        // Session très ancienne
        let old_entry = SessionIndexEntry {
            session_id:  "old-001".to_string(),
            user_id:     "user".to_string(),
            vm_id:       "vm".to_string(),
            started_at:  Utc::now() - chrono::Duration::days(10),
            ended_at:    None,
            file_name:   "old.mkv".to_string(),
            file_size:   0,
            frame_count: 0,
            chain_hash:  "".to_string(),
            sealed:      false,
        };
        worm.register_session(old_entry).unwrap();

        let removed = worm.apply_retention().unwrap();
        assert_eq!(removed, 1);
        assert_eq!(worm.load_index().entries.len(), 0);
    }
}
