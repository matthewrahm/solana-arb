use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

pub struct JupiterClient {
    client: reqwest::Client,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PriceData {
    usd_price: Option<f64>,
}

impl JupiterClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Get USD price for a single token mint
    pub async fn get_price(&self, mint: &str) -> Result<f64> {
        let prices = self.get_prices(&[mint]).await?;
        Ok(*prices.get(mint).unwrap_or(&0.0))
    }

    /// Batch fetch USD prices for multiple token mints
    pub async fn get_prices(&self, mints: &[&str]) -> Result<HashMap<String, f64>> {
        if mints.is_empty() {
            return Ok(HashMap::new());
        }

        let mut all_prices = HashMap::new();

        for chunk in mints.chunks(100) {
            let ids = chunk.join(",");
            let url = format!("https://api.jup.ag/price/v3?ids={ids}");

            let resp: HashMap<String, PriceData> = self
                .client
                .get(&url)
                .send()
                .await?
                .json()
                .await
                .context("Failed to parse Jupiter price response")?;

            for (mint, data) in resp {
                if let Some(price) = data.usd_price {
                    all_prices.insert(mint, price);
                }
            }
        }

        Ok(all_prices)
    }
}
