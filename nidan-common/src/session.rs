//! Gestion des identifiants et états de session.

use std::fmt;

/// Identifiant de session (UUID v4 sous forme de string)
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    /// Génère un nouvel identifiant de session aléatoire
    pub fn new() -> Self {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let bytes: [u8; 16] = rng.gen();
        // Format UUID v4 simplifié
        let s = format!(
            "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            u16::from_be_bytes([bytes[4], bytes[5]]),
            u16::from_be_bytes([bytes[6], bytes[7]]) & 0x0FFF,
            (u16::from_be_bytes([bytes[8], bytes[9]]) & 0x3FFF) | 0x8000,
            {
                let mut v = 0u64;
                for i in 10..16 { v = (v << 8) | bytes[i] as u64; }
                v
            }
        );
        Self(s)
    }
}

impl Default for SessionId {
    fn default() -> Self { Self::new() }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str { &self.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_id_unique() {
        let ids: Vec<_> = (0..100).map(|_| SessionId::new()).collect();
        let set: std::collections::HashSet<_> = ids.iter().map(|s| &s.0).collect();
        assert_eq!(set.len(), 100, "tous les IDs doivent être uniques");
    }
}
