//! Phase B2: stale-reserve observation mode.
//!
//! For every above-threshold forge signal on PumpSwap/Raydium, measure whether
//! the post-trigger pool implied price deviates meaningfully from the broader
//! market's fair-price reference.
//!
//! - `pool_implied_price_usd`: price at the specific pool that just saw the trigger swap
//! - `fair_price_usd`: liquidity-weighted aggregate across all other DexScreener pairs for the token
//! - `delta_bps`: (pool - fair) / fair × 10000. Positive means pool > fair (over-bought).
//!
//! This costs 1 DexScreener HTTP call per observation. Zero Helius credits.
//! Results persist to `stale_reserve_observations` for later analysis.

use anyhow::{Context, Result};
use arb_types::{Dex, SwapDirection};
use reqwest::Client;
use serde::Deserialize;
use std::time::Instant;
use tracing::{debug, info};

const DEXSCREENER_TOKENS_URL: &str = "https://api.dexscreener.com/latest/dex/tokens";

#[derive(Debug, Clone)]
pub struct ObservationInput<'a> {
    pub signature: Option<&'a str>,
    pub token_mint: &'a str,
    pub token_symbol: Option<&'a str>,
    pub trigger_dex: Dex,
    pub trigger_pool_address: Option<&'a str>,
    pub trigger_direction: SwapDirection,
    pub trigger_sol_equivalent: f64,
}

#[derive(Debug, Clone)]
pub struct Observation {
    pub pool_address: String,
    pub pool_implied_price_usd: f64,
    pub fair_price_usd: f64,
    pub delta_bps: f64,
    pub pool_liquidity_usd: f64,
    pub observation_latency_ms: i64,
    pub other_pairs_count: usize,
}

#[derive(Clone)]
pub struct StaleReserveObserver {
    client: Client,
}

impl Default for StaleReserveObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl StaleReserveObserver {
    pub fn new() -> Self {
        Self { client: Client::new() }
    }

    /// Fetch DexScreener data, match the trigger pool, compute delta vs liquidity-weighted fair.
    pub async fn observe(&self, input: &ObservationInput<'_>) -> Result<Observation> {
        let started = Instant::now();
        let url = format!("{}/{}", DEXSCREENER_TOKENS_URL, input.token_mint);
        let resp: DexScreenerResponse = self
            .client
            .get(&url)
            .send()
            .await
            .context("dexscreener request failed")?
            .json()
            .await
            .context("dexscreener json decode failed")?;

        let pairs = resp.pairs.unwrap_or_default();
        if pairs.is_empty() {
            anyhow::bail!("no dexscreener pairs for {}", input.token_mint);
        }

        // Pick the trigger pool: prefer exact pool-address match, fall back to DEX-name match.
        let trigger = pick_trigger_pair(&pairs, input.trigger_dex, input.trigger_pool_address)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no dexscreener pair matches trigger dex {} for {}",
                    input.trigger_dex,
                    input.token_mint
                )
            })?;

        let pool_implied = parse_f64(trigger.price_usd.as_deref())
            .ok_or_else(|| anyhow::anyhow!("trigger pair missing priceUsd"))?;
        let pool_liquidity = trigger.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0);

        // Fair price = liquidity-weighted mean across OTHER pairs.
        // If no other pairs exist (single-pool token), there's no reference — record a 0 delta.
        let others: Vec<&DexScreenerPair> = pairs
            .iter()
            .filter(|p| p.pair_address != trigger.pair_address)
            .collect();

        let (fair_price, delta_bps) = if others.is_empty() {
            (pool_implied, 0.0)
        } else {
            let (weighted_sum, weight_total) = others.iter().fold((0.0, 0.0), |acc, p| {
                let price = parse_f64(p.price_usd.as_deref()).unwrap_or(0.0);
                let weight = p.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0);
                if price > 0.0 && weight > 0.0 {
                    (acc.0 + price * weight, acc.1 + weight)
                } else {
                    acc
                }
            });
            if weight_total <= 0.0 {
                (pool_implied, 0.0)
            } else {
                let fair = weighted_sum / weight_total;
                let delta = if fair > 0.0 {
                    ((pool_implied - fair) / fair) * 10_000.0
                } else {
                    0.0
                };
                (fair, delta)
            }
        };

        let obs = Observation {
            pool_address: trigger.pair_address.clone(),
            pool_implied_price_usd: pool_implied,
            fair_price_usd: fair_price,
            delta_bps,
            pool_liquidity_usd: pool_liquidity,
            observation_latency_ms: started.elapsed().as_millis() as i64,
            other_pairs_count: others.len(),
        };

        let abs_bps = delta_bps.abs();
        if abs_bps >= 50.0 {
            info!(
                "OBSERVE {} {} {} | pool ${:.10} vs fair ${:.10} | delta {:+.1} bps | liq ${:.0} | {} other pairs | {}ms",
                input.token_symbol.unwrap_or("?"),
                input.trigger_dex,
                input.trigger_direction,
                pool_implied,
                fair_price,
                delta_bps,
                pool_liquidity,
                others.len(),
                obs.observation_latency_ms,
            );
        } else {
            debug!(
                "observe {} | delta {:+.1} bps | {} other pairs",
                input.token_symbol.unwrap_or("?"),
                delta_bps,
                others.len()
            );
        }
        Ok(obs)
    }

}

fn pick_trigger_pair<'a>(
    pairs: &'a [DexScreenerPair],
    trigger_dex: Dex,
    trigger_pool_address: Option<&str>,
) -> Option<&'a DexScreenerPair> {
    if let Some(addr) = trigger_pool_address {
        if let Some(exact) = pairs.iter().find(|p| p.pair_address == addr) {
            return Some(exact);
        }
    }
    let dex_name = dex_to_dexscreener_id(trigger_dex);
    // Among pairs on the target DEX, pick highest-liquidity pool.
    pairs
        .iter()
        .filter(|p| p.dex_id.eq_ignore_ascii_case(dex_name))
        .max_by(|a, b| {
            let al = a.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0);
            let bl = b.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0);
            al.partial_cmp(&bl).unwrap_or(std::cmp::Ordering::Equal)
        })
}

fn dex_to_dexscreener_id(dex: Dex) -> &'static str {
    match dex {
        Dex::Raydium => "raydium",
        Dex::RaydiumClmm => "raydium",
        Dex::Orca => "orca",
        Dex::Meteora => "meteora",
        Dex::PumpFun => "pumpfun",
        Dex::PumpSwap => "pumpswap",
        Dex::Jupiter => "jupiter",
        Dex::Unknown => "",
    }
}

fn parse_f64(s: Option<&str>) -> Option<f64> {
    s.and_then(|v| v.parse::<f64>().ok())
}

#[derive(Debug, Deserialize)]
struct DexScreenerResponse {
    pairs: Option<Vec<DexScreenerPair>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DexScreenerPair {
    pair_address: String,
    dex_id: String,
    #[serde(default)]
    price_usd: Option<String>,
    liquidity: Option<DexScreenerLiquidity>,
}

#[derive(Debug, Deserialize)]
struct DexScreenerLiquidity {
    usd: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dex_ids_cover_known_variants() {
        assert_eq!(dex_to_dexscreener_id(Dex::Raydium), "raydium");
        assert_eq!(dex_to_dexscreener_id(Dex::PumpSwap), "pumpswap");
        assert_eq!(dex_to_dexscreener_id(Dex::PumpFun), "pumpfun");
    }

    #[test]
    fn parse_f64_strips_none_on_bad() {
        assert_eq!(parse_f64(Some("1.23")), Some(1.23));
        assert_eq!(parse_f64(Some("foo")), None);
        assert_eq!(parse_f64(None), None);
    }
}
