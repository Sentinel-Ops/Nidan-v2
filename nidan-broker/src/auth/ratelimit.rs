//! Rate limiting et bannissement des IPs pour le broker NIDAN.
//!
//! Protège contre les attaques par force brute sur l'authentification.
//! Utilise une fenêtre glissante de 5 minutes.

use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tracing::warn;

/// Entrée de suivi pour une IP
struct IpEntry {
    /// Timestamps des tentatives échouées (fenêtre glissante)
    failures: Vec<Instant>,
    /// Optionnel : timestamp de bannissement
    banned_until: Option<Instant>,
}

impl IpEntry {
    fn new() -> Self {
        Self { failures: Vec::new(), banned_until: None }
    }

    /// Nettoie les tentatives hors fenêtre (5 minutes)
    fn prune(&mut self, window: Duration) {
        let cutoff = Instant::now() - window;
        self.failures.retain(|t| *t > cutoff);
    }

    fn failure_count(&mut self, window: Duration) -> usize {
        self.prune(window);
        self.failures.len()
    }
}

/// Rate limiter thread-safe
pub struct RateLimiter {
    entries:      DashMap<IpAddr, IpEntry>,
    max_failures: u32,
    ban_duration: Duration,
    window:       Duration,
}

impl RateLimiter {
    pub fn new(max_failures: u32, ban_secs: u64) -> Self {
        Self {
            entries:      DashMap::new(),
            max_failures,
            ban_duration: Duration::from_secs(ban_secs),
            window:       Duration::from_secs(300), // fenêtre 5 minutes
        }
    }

    /// Retourne true si l'IP est actuellement bannie
    pub fn is_banned(&self, ip: IpAddr) -> bool {
        if let Some(entry) = self.entries.get(&ip) {
            if let Some(banned_until) = entry.banned_until {
                if Instant::now() < banned_until {
                    return true;
                }
            }
        }
        false
    }

    /// Retourne true si l'IP a dépassé le rate limit (sans encore la bannir)
    pub fn is_rate_limited(&self, ip: IpAddr) -> bool {
        if let Some(mut entry) = self.entries.get_mut(&ip) {
            entry.failure_count(self.window) >= self.max_failures as usize
        } else {
            false
        }
    }

    /// Enregistre un échec d'authentification
    pub fn record_failure(&self, ip: IpAddr) {
        let mut entry = self.entries.entry(ip).or_insert_with(IpEntry::new);
        entry.failures.push(Instant::now());

        let count = entry.failure_count(self.window);
        if count >= self.max_failures as usize {
            let ban_until = Instant::now() + self.ban_duration;
            entry.banned_until = Some(ban_until);
            warn!(
                ip   = %ip,
                count = count,
                ban_secs = self.ban_duration.as_secs(),
                "IP bannie après trop d'échecs d'auth"
            );
        }
    }

    /// Enregistre un succès — réinitialise le compteur
    pub fn record_success(&self, ip: IpAddr) {
        self.entries.remove(&ip);
    }

    /// Retourne les statistiques courantes
    pub fn stats(&self) -> RateLimitStats {
        let now = Instant::now();
        let banned = self.entries.iter()
            .filter(|e| e.banned_until.map(|t| now < t).unwrap_or(false))
            .count();
        RateLimitStats {
            tracked_ips: self.entries.len(),
            banned_ips:  banned,
        }
    }
}

#[derive(Debug)]
pub struct RateLimitStats {
    pub tracked_ips: usize,
    pub banned_ips:  usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limit_triggers() {
        let rl = RateLimiter::new(3, 60);
        let ip: IpAddr = "192.168.1.1".parse().unwrap();

        assert!(!rl.is_rate_limited(ip));
        rl.record_failure(ip);
        rl.record_failure(ip);
        assert!(!rl.is_rate_limited(ip));
        rl.record_failure(ip);
        assert!(rl.is_rate_limited(ip));
    }

    #[test]
    fn test_ban_after_failures() {
        let rl = RateLimiter::new(2, 60);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        rl.record_failure(ip);
        rl.record_failure(ip);
        assert!(rl.is_banned(ip));
    }

    #[test]
    fn test_success_resets_counter() {
        let rl = RateLimiter::new(3, 60);
        let ip: IpAddr = "172.16.0.1".parse().unwrap();

        rl.record_failure(ip);
        rl.record_failure(ip);
        rl.record_success(ip);
        assert!(!rl.is_rate_limited(ip));
    }
}
