//! Jupiter V6 Quote API client for getting swap quotes.
//! Uses a global rate limiter to prevent 429s across all concurrent systems.

use std::sync::Arc;

use anyhow::{Context, Result};
use arb_types::Dex;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio::time::{Duration, Instant};

/// Global rate limiter: max 1 Jupiter request per 600ms (~1.6 req/s).
/// Shared across ALL JupiterQuoteClient instances via static.
static JUPITER_RATE_LIMIT_MS: u64 = 600;

static GLOBAL_SEMAPHORE: std::sync::LazyLock<Arc<Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(Semaphore::new(1)));

static GLOBAL_LAST_REQUEST: std::sync::LazyLock<Arc<tokio::sync::Mutex<Option<Instant>>>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Mutex::new(None)));

pub struct JupiterQuoteClient {
    client: reqwest::Client,
    base_url: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    pub input_mint: String,
    pub in_amount: String,
    pub output_mint: String,
    pub out_amount: String,
    pub other_amount_threshold: String,
    pub swap_mode: String,
    pub slippage_bps: u16,
    pub price_impact_pct: String,
    pub route_plan: Vec<RouteLeg>,
    pub context_slot: Option<u64>,
    pub time_taken: Option<f64>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RouteLeg {
    pub swap_info: SwapInfo,
    pub percent: u8,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SwapInfo {
    pub amm_key: String,
    pub label: String,
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: String,
    pub out_amount: String,
}

impl JupiterQuoteClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: "https://api.jup.ag/swap/v1/quote".to_string(),
        }
    }

    /// Enforce global rate limit: wait until at least JUPITER_RATE_LIMIT_MS since last request.
    /// This is shared across ALL JupiterQuoteClient instances process-wide.
    async fn rate_limit() {
        let _permit = GLOBAL_SEMAPHORE.acquire().await.unwrap();
        let mut last = GLOBAL_LAST_REQUEST.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            let min_gap = Duration::from_millis(JUPITER_RATE_LIMIT_MS);
            if elapsed < min_gap {
                tokio::time::sleep(min_gap - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    /// Get a swap quote from Jupiter, optionally restricted to a specific DEX.
    /// Automatically rate-limited to prevent 429s.
    pub async fn get_quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
        restrict_dex: Option<Dex>,
    ) -> Result<QuoteResponse> {
        // Enforce global rate limit before making the request
        Self::rate_limit().await;

        let mut url = format!(
            "{}?inputMint={}&outputMint={}&amount={}&slippageBps={}",
            self.base_url, input_mint, output_mint, amount, slippage_bps
        );

        // Restrict routing to a specific DEX if requested
        if let Some(dex) = restrict_dex {
            if let Some(jup_name) = dex_to_jupiter_name(dex) {
                url.push_str(&format!("&dexes={}", jup_name));
            }
        }

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Jupiter quote request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Jupiter quote API returned {}: {}", status, body);
        }

        resp.json()
            .await
            .context("Failed to parse Jupiter quote response")
    }

    /// Get the route label (DEX name) from the first leg of a quote.
    pub fn primary_route_label(quote: &QuoteResponse) -> Option<&str> {
        quote.route_plan.first().map(|leg| leg.swap_info.label.as_str())
    }
}

/// Map our Dex enum to Jupiter's dexes parameter values.
/// Jupiter uses specific label names for DEX routing.
fn dex_to_jupiter_name(dex: Dex) -> Option<&'static str> {
    match dex {
        Dex::Raydium => Some("Raydium,Raydium CLMM"),
        Dex::Orca => Some("Whirlpool"),
        Dex::Meteora => Some("Meteora,Meteora DLMM"),
        Dex::PumpFun => None, // PumpFun not directly routable via Jupiter
        _ => None,
    }
}
