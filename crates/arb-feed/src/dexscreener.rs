use anyhow::{Context, Result};
use arb_types::{Dex, PriceQuote, PriceSource};
use chrono::Utc;
use serde::Deserialize;
use tracing::warn;

pub struct DexScreenerClient {
    client: reqwest::Client,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct DexPair {
    #[serde(default)]
    dex_id: String,
    price_usd: Option<String>,
    liquidity: Option<Liquidity>,
    pair_address: Option<String>,
    base_token: Option<BaseToken>,
    quote_token: Option<QuoteToken>,
}

#[derive(Deserialize, Debug)]
struct Liquidity {
    usd: Option<f64>,
}

#[derive(Deserialize, Debug)]
struct BaseToken {
    symbol: Option<String>,
}

#[derive(Deserialize, Debug)]
struct QuoteToken {
    address: Option<String>,
}

#[derive(Deserialize)]
struct DexResponse {
    pairs: Option<Vec<DexPair>>,
}

impl DexScreenerClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Fetch ALL trading pairs for a token across every DEX.
    /// Returns one PriceQuote per pair with sufficient liquidity.
    pub async fn get_all_pairs(&self, mint: &str, min_liquidity: f64) -> Result<Vec<PriceQuote>> {
        let url = format!("https://api.dexscreener.com/latest/dex/tokens/{mint}");

        let resp: DexResponse = self
            .client
            .get(&url)
            .send()
            .await?
            .json()
            .await
            .context("Failed to parse DexScreener response")?;

        let now = Utc::now();
        let pairs = resp.pairs.unwrap_or_default();

        let quotes: Vec<PriceQuote> = pairs
            .into_iter()
            .filter_map(|pair| {
                let price: f64 = pair.price_usd.as_ref()?.parse().ok()?;
                let liquidity = pair.liquidity.as_ref()?.usd.unwrap_or(0.0);

                if liquidity < min_liquidity {
                    return None;
                }

                let dex = Dex::from_dexscreener_id(&pair.dex_id);
                if dex == Dex::Unknown {
                    warn!("Unknown DEX id from DexScreener: {}", pair.dex_id);
                    return None;
                }

                Some(PriceQuote {
                    dex,
                    base_mint: mint.to_string(),
                    quote_mint: pair.quote_token.as_ref().and_then(|q| q.address.clone()).unwrap_or_default(),
                    price_usd: price,
                    liquidity_usd: liquidity,
                    pool_address: pair.pair_address,
                    source: PriceSource::HttpPoll,
                    timestamp: now,
                })
            })
            .collect();

        Ok(quotes)
    }

    /// Get the symbol for a token from DexScreener
    pub async fn get_token_symbol(&self, mint: &str) -> Result<Option<String>> {
        let url = format!("https://api.dexscreener.com/latest/dex/tokens/{mint}");

        let resp: DexResponse = self
            .client
            .get(&url)
            .send()
            .await?
            .json()
            .await
            .context("Failed to parse DexScreener response")?;

        Ok(resp
            .pairs
            .and_then(|p| p.first()?.base_token.as_ref()?.symbol.clone()))
    }
}
