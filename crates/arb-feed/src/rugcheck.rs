use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::info;

// ── RugCheck API Client ──

pub struct RugCheckClient {
    client: reqwest::Client,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RugCheckReport {
    pub score: Option<f64>,
    #[serde(default)]
    pub risks: Vec<RiskItem>,
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    #[serde(default)]
    pub top_holders: Vec<HolderInfo>,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct RiskItem {
    pub name: Option<String>,
    pub level: Option<String>,
    pub description: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct HolderInfo {
    pub address: Option<String>,
    pub pct: Option<f64>,
}

impl RugCheckReport {
    pub fn risk_level(&self) -> &'static str {
        match self.score {
            Some(s) if s >= 80.0 => "LOW",
            Some(s) if s >= 50.0 => "MEDIUM",
            Some(_) => "HIGH",
            None => "UNKNOWN",
        }
    }

    pub fn mint_revoked(&self) -> bool {
        self.mint_authority.as_deref() == Some("") || self.mint_authority.is_none()
    }

    pub fn freeze_revoked(&self) -> bool {
        self.freeze_authority.as_deref() == Some("") || self.freeze_authority.is_none()
    }

    pub fn top_holder_pct(&self) -> f64 {
        self.top_holders.iter().take(10).filter_map(|h| h.pct).sum()
    }
}

impl RugCheckClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    pub async fn get_report(&self, mint: &str) -> Result<Option<RugCheckReport>> {
        let url = format!("https://api.rugcheck.xyz/v1/tokens/{mint}/report/summary");

        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let report: RugCheckReport = resp
            .json()
            .await
            .context("Failed to parse RugCheck response")?;

        Ok(Some(report))
    }
}

// ── Token Safety Cache ──

#[derive(Debug, Clone)]
pub struct TokenSafety {
    pub mint: String,
    pub rugcheck_score: Option<f64>,
    pub risk_level: String,
    pub mint_revoked: bool,
    pub freeze_revoked: bool,
    pub top_holder_pct: f64,
    pub safe: bool,
}

impl TokenSafety {
    pub fn from_report(mint: &str, report: &RugCheckReport) -> Self {
        let safe = report.score.unwrap_or(0.0) >= 50.0
            && report.mint_revoked()
            && report.freeze_revoked()
            && report.top_holder_pct() < 30.0;

        Self {
            mint: mint.to_string(),
            rugcheck_score: report.score,
            risk_level: report.risk_level().to_string(),
            mint_revoked: report.mint_revoked(),
            freeze_revoked: report.freeze_revoked(),
            top_holder_pct: report.top_holder_pct(),
            safe,
        }
    }
}

pub struct TokenSafetyCache {
    cache: HashMap<String, (TokenSafety, Instant)>,
    client: RugCheckClient,
    ttl_secs: u64,
}

impl TokenSafetyCache {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            cache: HashMap::new(),
            client: RugCheckClient::new(),
            ttl_secs,
        }
    }

    /// Check if a token is safe. Returns cached result if available,
    /// otherwise fetches from RugCheck API.
    pub async fn check_safety(&mut self, mint: &str) -> Result<TokenSafety> {
        // Check cache first
        if let Some((safety, cached_at)) = self.cache.get(mint) {
            if cached_at.elapsed().as_secs() < self.ttl_secs {
                return Ok(safety.clone());
            }
        }

        // Fetch from RugCheck
        let safety = match self.client.get_report(mint).await? {
            Some(report) => {
                let s = TokenSafety::from_report(mint, &report);
                info!(
                    "RUGCHECK {} | score: {} | risk: {} | mint_revoked: {} | freeze_revoked: {} | top10: {:.1}% | {}",
                    &mint[..8.min(mint.len())],
                    s.rugcheck_score.map_or("?".to_string(), |v| format!("{:.0}", v)),
                    s.risk_level,
                    s.mint_revoked,
                    s.freeze_revoked,
                    s.top_holder_pct,
                    if s.safe { "SAFE" } else { "UNSAFE" },
                );
                s
            }
            None => {
                // API returned non-200; treat as unknown/unsafe
                TokenSafety {
                    mint: mint.to_string(),
                    rugcheck_score: None,
                    risk_level: "UNKNOWN".to_string(),
                    mint_revoked: false,
                    freeze_revoked: false,
                    top_holder_pct: 100.0,
                    safe: false,
                }
            }
        };

        self.cache.insert(mint.to_string(), (safety.clone(), Instant::now()));

        // Prune old entries
        if self.cache.len() > 1000 {
            let ttl = self.ttl_secs;
            self.cache.retain(|_, (_, t)| t.elapsed().as_secs() < ttl);
        }

        Ok(safety)
    }

    pub fn is_cached(&self, mint: &str) -> bool {
        if let Some((_, cached_at)) = self.cache.get(mint) {
            cached_at.elapsed().as_secs() < self.ttl_secs
        } else {
            false
        }
    }
}
