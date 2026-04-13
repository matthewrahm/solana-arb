//! Pool discovery: collects pool addresses from DexScreener and resolves
//! Raydium vault accounts + token decimals via RPC.

use anyhow::{Context, Result};
use arb_types::{Dex, RaydiumVaults, USDC_MINT};
use base64::Engine;
use serde::Deserialize;
use tracing::{info, warn};

use crate::pool_decoder;

/// A pool discovered from DexScreener that we can subscribe to
#[derive(Debug, Clone)]
pub struct DiscoveredPool {
    pub pool_address: String,
    pub dex: Dex,
    pub base_mint: String,
    pub quote_mint: String,
    pub base_decimals: u8,
    pub quote_decimals: u8,
}

/// Collect subscribable pools from DexScreener for the given tokens.
pub async fn discover_pools(watch_mints: &[String], min_liquidity: f64) -> Result<Vec<DiscoveredPool>> {
    let client = crate::dexscreener::DexScreenerClient::new();
    let mut pools = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for mint in watch_mints {
        match client.get_all_pairs(mint, min_liquidity).await {
            Ok(quotes) => {
                for q in &quotes {
                    if let Some(ref addr) = q.pool_address {
                        // Only subscribe to DEXes we can decode on-chain
                        match q.dex {
                            Dex::Raydium | Dex::Orca | Dex::Meteora | Dex::PumpFun => {
                                if seen.insert(addr.clone()) {
                                    pools.push(DiscoveredPool {
                                        pool_address: addr.clone(),
                                        dex: q.dex,
                                        base_mint: q.base_mint.clone(),
                                        quote_mint: q.quote_mint.clone(),
                                        base_decimals: 0, // resolved later
                                        quote_decimals: 0,
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Err(e) => warn!("Pool discovery failed for {}: {}", &mint[..8], e),
        }
    }

    info!("Discovered {} subscribable pools", pools.len());
    for p in &pools {
        info!("  {} on {} ({})", &p.pool_address[..8], p.dex, &p.base_mint[..8]);
    }

    Ok(pools)
}

/// Fetch Raydium pool account data and extract vault pubkeys.
pub async fn resolve_raydium_vaults(rpc_url: &str, pool_address: &str) -> Result<RaydiumVaults> {
    let data = fetch_account_data(rpc_url, pool_address).await?;
    let (coin_bytes, pc_bytes) = pool_decoder::decode_raydium_pool_vaults(&data)?;
    let coin_vault = bs58::encode(coin_bytes).into_string();
    let pc_vault = bs58::encode(pc_bytes).into_string();

    info!("Raydium {} vaults: coin={}, pc={}", &pool_address[..8], &coin_vault[..8], &pc_vault[..8]);

    Ok(RaydiumVaults { coin_vault, pc_vault })
}

/// Read token decimals from a mint account.
pub async fn resolve_token_decimals(rpc_url: &str, mint: &str) -> Result<u8> {
    let data = fetch_account_data(rpc_url, mint).await?;
    pool_decoder::decode_mint_decimals(&data)
}

/// Check if a quote mint is a stablecoin (price already in USD terms).
pub fn is_usd_quoted(quote_mint: &str) -> bool {
    quote_mint == USDC_MINT
}

/// Fetch raw account data via getAccountInfo RPC (public for pool_monitor use).
pub async fn fetch_account_data_pub(rpc_url: &str, address: &str) -> Result<Vec<u8>> {
    fetch_account_data(rpc_url, address).await
}

/// Fetch raw account data via getAccountInfo RPC.
async fn fetch_account_data(rpc_url: &str, address: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getAccountInfo",
        "params": [address, { "encoding": "base64" }]
    });

    let resp: RpcResponse = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await?
        .json()
        .await
        .context("Failed to parse getAccountInfo response")?;

    let data_b64 = resp
        .result
        .value
        .ok_or_else(|| anyhow::anyhow!("Account {} not found", address))?
        .data
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No data in account {}", address))?;

    base64::engine::general_purpose::STANDARD
        .decode(&data_b64)
        .context("base64 decode failed")
}

#[derive(Deserialize)]
struct RpcResponse {
    result: RpcResult,
}

#[derive(Deserialize)]
struct RpcResult {
    value: Option<AccountValue>,
}

#[derive(Deserialize)]
struct AccountValue {
    data: Vec<String>,
}
