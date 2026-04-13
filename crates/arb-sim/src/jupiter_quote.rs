//! Jupiter V6 Quote API client for getting swap quotes.

use anyhow::{Context, Result};
use serde::Deserialize;

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

    /// Get a swap quote from Jupiter.
    /// `amount` is in raw token units (e.g., lamports for SOL).
    pub async fn get_quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<QuoteResponse> {
        let url = format!(
            "{}?inputMint={}&outputMint={}&amount={}&slippageBps={}",
            self.base_url, input_mint, output_mint, amount, slippage_bps
        );

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
