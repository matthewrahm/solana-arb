//! Tracks swap volume per token from the forge stream to detect activity spikes.
//! When a token's 1-minute swap count exceeds 3x its 5-minute average, it's flagged as "hot."

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use tracing::info;

const WINDOW_SECS: f64 = 300.0; // 5-minute rolling window
const SPIKE_WINDOW_SECS: f64 = 60.0; // 1-minute recent window
const SPIKE_MULTIPLIER: f64 = 3.0; // 3x average = spike
const HOT_DURATION_SECS: f64 = 300.0; // keep token hot for 5 minutes

pub struct VolumeTracker {
    /// Recent swap timestamps per token mint
    swaps: HashMap<String, VecDeque<Instant>>,
    /// Tokens currently flagged as "hot" with expiration
    hot_tokens: HashMap<String, Instant>,
}

impl VolumeTracker {
    pub fn new() -> Self {
        Self {
            swaps: HashMap::new(),
            hot_tokens: HashMap::new(),
        }
    }

    /// Record a swap event for a token. Returns true if this triggers a new hot token alert.
    pub fn record_swap(&mut self, token_mint: &str) -> bool {
        let now = Instant::now();

        // Add to swap history
        let history = self.swaps.entry(token_mint.to_string()).or_default();
        history.push_back(now);

        // Prune old entries beyond the 5-minute window
        while let Some(front) = history.front() {
            if now.duration_since(*front).as_secs_f64() > WINDOW_SECS {
                history.pop_front();
            } else {
                break;
            }
        }

        // Need at least some history to detect a spike
        if history.len() < 5 {
            return false;
        }

        // Count swaps in the recent 1-minute window
        let recent_count = history
            .iter()
            .filter(|t| now.duration_since(**t).as_secs_f64() < SPIKE_WINDOW_SECS)
            .count();

        // Calculate the 5-minute average rate (per minute)
        let total_count = history.len();
        let window_duration = now
            .duration_since(*history.front().unwrap())
            .as_secs_f64()
            .max(1.0);
        let avg_per_minute = (total_count as f64 / window_duration) * 60.0;

        // Spike detection: recent 1-min count > 3x the average per-minute rate
        let is_spike = recent_count as f64 > avg_per_minute * SPIKE_MULTIPLIER;

        if is_spike && !self.is_hot(token_mint) {
            info!(
                "VOLUME SPIKE {} | {} swaps in last 60s vs {:.1}/min avg | FLAGGED HOT",
                &token_mint[..8.min(token_mint.len())],
                recent_count,
                avg_per_minute,
            );
            self.hot_tokens.insert(token_mint.to_string(), now);
            return true;
        }

        false
    }

    /// Check if a token is currently flagged as hot
    pub fn is_hot(&self, token_mint: &str) -> bool {
        if let Some(flagged_at) = self.hot_tokens.get(token_mint) {
            flagged_at.elapsed().as_secs_f64() < HOT_DURATION_SECS
        } else {
            false
        }
    }

    /// Get all currently hot tokens
    pub fn hot_tokens(&self) -> Vec<String> {
        self.hot_tokens
            .iter()
            .filter(|(_, t)| t.elapsed().as_secs_f64() < HOT_DURATION_SECS)
            .map(|(mint, _)| mint.clone())
            .collect()
    }

    /// Prune expired data to prevent unbounded growth
    pub fn cleanup(&mut self) {
        let now = Instant::now();
        self.hot_tokens
            .retain(|_, t| now.duration_since(*t).as_secs_f64() < HOT_DURATION_SECS);
        self.swaps.retain(|_, history| {
            while let Some(front) = history.front() {
                if now.duration_since(*front).as_secs_f64() > WINDOW_SECS {
                    history.pop_front();
                } else {
                    break;
                }
            }
            !history.is_empty()
        });
    }
}
