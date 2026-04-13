//! Whale wallet tracker: maintains a set of known profitable wallets.
//! Signals from whale wallets get priority treatment (lower size threshold).

use std::collections::HashSet;
use std::path::Path;

use tracing::{info, warn};

const DEFAULT_WHALE_MIN_SOL: f64 = 2.0; // lower threshold for whale signals (vs 5 SOL normal)

pub struct WhaleTracker {
    wallets: HashSet<String>,
    pub min_sol_for_whale: f64,
}

impl WhaleTracker {
    pub fn new() -> Self {
        Self {
            wallets: HashSet::new(),
            min_sol_for_whale: DEFAULT_WHALE_MIN_SOL,
        }
    }

    /// Load whale wallets from a file (one address per line)
    pub fn load_from_file(path: &Path) -> Self {
        let mut tracker = Self::new();

        match std::fs::read_to_string(path) {
            Ok(contents) => {
                for line in contents.lines() {
                    let addr = line.trim();
                    if !addr.is_empty() && !addr.starts_with('#') {
                        tracker.wallets.insert(addr.to_string());
                    }
                }
                info!("Loaded {} whale wallets from {}", tracker.wallets.len(), path.display());
            }
            Err(e) => {
                warn!("Could not load whale file {}: {}", path.display(), e);
            }
        }

        tracker
    }

    /// Add wallets from CLI args
    pub fn add_wallets(&mut self, wallets: &[String]) {
        for w in wallets {
            self.wallets.insert(w.clone());
        }
        if !wallets.is_empty() {
            info!("Added {} whale wallets from CLI args", wallets.len());
        }
    }

    pub fn is_whale(&self, signer: &str) -> bool {
        self.wallets.contains(signer)
    }

    pub fn wallet_count(&self) -> usize {
        self.wallets.len()
    }
}
