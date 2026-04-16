//! Micro-cap token discovery: polls DexScreener for new Solana tokens,
//! validates safety (no freeze/mint authority), and feeds them into the scanner.

use anyhow::{Context, Result};
use arb_types::Dex;
use serde::Deserialize;
use std::collections::HashSet;
use tracing::{info, warn};

use crate::pool_discovery;
use crate::rugcheck::RugCheckClient;

/// A discovered token that passes safety checks
#[derive(Debug, Clone)]
pub struct DiscoveredToken {
    pub mint: String,
    pub symbol: String,
    pub liquidity_usd: f64,
    pub dex: Dex,
    pub pair_address: String,
    pub safe: bool,
    pub safety_note: String,
}

#[derive(Deserialize)]
struct TokenProfile {
    #[serde(rename = "tokenAddress")]
    token_address: String,
    #[serde(rename = "chainId")]
    chain_id: String,
}

/// Fetch recently profiled Solana tokens from DexScreener
pub async fn discover_new_tokens(
    known_mints: &HashSet<String>,
    min_liquidity: f64,
    rpc_url: Option<&str>,
) -> Result<Vec<DiscoveredToken>> {
    let client = reqwest::Client::new();

    // Fetch latest token profiles
    let profiles: Vec<TokenProfile> = client
        .get("https://api.dexscreener.com/token-profiles/latest/v1")
        .send()
        .await?
        .json()
        .await
        .context("Failed to parse token profiles")?;

    // Filter for Solana tokens not already in our watchlist
    let solana_tokens: Vec<&TokenProfile> = profiles
        .iter()
        .filter(|t| t.chain_id == "solana" && !known_mints.contains(&t.token_address))
        .collect();

    if solana_tokens.is_empty() {
        return Ok(Vec::new());
    }

    info!("Discovery: found {} new Solana tokens to check", solana_tokens.len());

    let mut results = Vec::new();
    let dex_client = crate::dexscreener::DexScreenerClient::new();
    let rugcheck = RugCheckClient::new();

    // Check each token's pairs and safety
    for (i, token) in solana_tokens.iter().enumerate().take(10) {
        // Rate limit: max 10 tokens per discovery cycle
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        match dex_client.get_all_pairs(&token.token_address, min_liquidity).await {
            Ok(pairs) => {
                if pairs.is_empty() {
                    continue;
                }

                // Get the highest-liquidity pair
                let best = pairs.iter()
                    .max_by(|a, b| a.liquidity_usd.partial_cmp(&b.liquidity_usd).unwrap())
                    .unwrap();

                // Get symbol from the first pair
                let symbol = dex_client
                    .get_token_symbol(&token.token_address)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| format!("{}..{}", &token.token_address[..4], &token.token_address[token.token_address.len()-4..]));

                // Safety check: RPC on-chain checks + RugCheck API
                // If RPC fails (rate limited, parse error), fall back to RugCheck only
                let (rpc_safe, rpc_note) = if let Some(rpc) = rpc_url {
                    let result = check_token_safety(rpc, &token.token_address).await;
                    if result.1.starts_with("RPC error") {
                        // RPC failed -- don't block on network errors, let RugCheck decide
                        (true, "RPC unavailable, RugCheck only".to_string())
                    } else {
                        result
                    }
                } else {
                    (true, "no RPC".to_string())
                };

                let (safe, note) = if !rpc_safe {
                    (false, rpc_note)
                } else {
                    // RPC passed, now check RugCheck for deeper analysis
                    match rugcheck.get_report(&token.token_address).await {
                        Ok(Some(report)) => {
                            let score = report.score.unwrap_or(0.0);
                            let top_pct = report.top_holder_pct();
                            let rug_safe = score >= 50.0 && top_pct < 30.0;
                            let note = format!(
                                "{} | rugcheck: {:.0} ({}) top10: {:.1}%",
                                rpc_note, score, report.risk_level(), top_pct
                            );
                            (rug_safe, note)
                        }
                        Ok(None) => {
                            // RugCheck API unavailable, rely on RPC checks only
                            (rpc_safe, format!("{} | rugcheck: unavailable", rpc_note))
                        }
                        Err(e) => {
                            warn!("RugCheck failed for {}: {}", &token.token_address[..8], e);
                            (rpc_safe, format!("{} | rugcheck: error", rpc_note))
                        }
                    }
                };

                info!(
                    "Discovery: {} ({}) | {} | liq: ${:.0} | safe: {} | {}",
                    symbol, &token.token_address[..8], best.dex, best.liquidity_usd, safe, note
                );

                results.push(DiscoveredToken {
                    mint: token.token_address.clone(),
                    symbol,
                    liquidity_usd: best.liquidity_usd,
                    dex: best.dex,
                    pair_address: best.pool_address.clone().unwrap_or_default(),
                    safe,
                    safety_note: note,
                });
            }
            Err(e) => {
                warn!("Discovery: failed to fetch pairs for {}: {}", &token.token_address[..8], e);
            }
        }
    }

    let safe_count = results.iter().filter(|t| t.safe).count();
    info!("Discovery: {} tokens found, {} passed safety checks", results.len(), safe_count);

    Ok(results)
}

/// Fetch trending/boosted Solana tokens from DexScreener that have 2+ pools on different DEXs.
/// These are the tokens where cross-venue arb is actually possible.
pub async fn discover_trending_multi_pool(
    known_mints: &HashSet<String>,
    min_liquidity: f64,
) -> Result<Vec<DiscoveredToken>> {
    let client = reqwest::Client::new();
    let dex_client = crate::dexscreener::DexScreenerClient::new();

    // Fetch boosted tokens (tokens that are actively being promoted/traded)
    let profiles: Vec<TokenProfile> = client
        .get("https://api.dexscreener.com/token-boosts/latest/v1")
        .send()
        .await?
        .json()
        .await
        .context("Failed to parse boosted token profiles")?;

    let solana_tokens: Vec<&TokenProfile> = profiles
        .iter()
        .filter(|t| t.chain_id == "solana" && !known_mints.contains(&t.token_address))
        .collect();

    if solana_tokens.is_empty() {
        return Ok(Vec::new());
    }

    info!("Trending discovery: checking {} boosted Solana tokens for multi-pool", solana_tokens.len());

    let mut results = Vec::new();

    for (i, token) in solana_tokens.iter().enumerate().take(15) {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }

        match dex_client.get_all_pairs(&token.token_address, min_liquidity).await {
            Ok(pairs) => {
                if pairs.len() < 2 {
                    continue; // need 2+ pools for cross-venue arb
                }

                // Check if pools are on DIFFERENT DEXs
                let dexes: HashSet<String> = pairs.iter().map(|p| p.dex.to_string()).collect();
                if dexes.len() < 2 {
                    continue; // all pools on same DEX, no cross-venue opportunity
                }

                let best = pairs.iter()
                    .max_by(|a, b| a.liquidity_usd.partial_cmp(&b.liquidity_usd).unwrap())
                    .unwrap();

                let symbol = dex_client
                    .get_token_symbol(&token.token_address)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| format!("{}..{}", &token.token_address[..4], &token.token_address[token.token_address.len()-4..]));

                info!(
                    "MULTI-POOL: {} ({}) | {} pools on {} DEXs | liq: ${:.0} | DEXs: {:?}",
                    symbol, &token.token_address[..8], pairs.len(), dexes.len(),
                    best.liquidity_usd, dexes
                );

                results.push(DiscoveredToken {
                    mint: token.token_address.clone(),
                    symbol,
                    liquidity_usd: best.liquidity_usd,
                    dex: best.dex,
                    pair_address: best.pool_address.clone().unwrap_or_default(),
                    safe: true, // boosted tokens are already established
                    safety_note: format!("{} pools across {} DEXs", pairs.len(), dexes.len()),
                });
            }
            Err(e) => {
                warn!("Trending discovery: failed for {}: {}", &token.token_address[..8], e);
            }
        }
    }

    info!("Trending discovery: {} multi-pool tokens found", results.len());
    Ok(results)
}

/// Check token safety by reading the mint account.
/// Returns (is_safe, description).
async fn check_token_safety(rpc_url: &str, mint: &str) -> (bool, String) {
    match pool_discovery::fetch_account_data_pub(rpc_url, mint).await {
        Ok(data) => {
            if data.len() < 82 {
                return (false, "mint account too short".to_string());
            }

            // SPL Mint layout:
            // 0-3:   mint_authority_option (u32, 4 bytes) - 0 = None, 1 = Some
            // 4-35:  mint_authority (32 bytes, only valid if option = 1)
            // 36-43: supply (u64)
            // 44:    decimals (u8)
            // 45:    is_initialized (bool)
            // 46-49: freeze_authority_option (u32)
            // 50-81: freeze_authority (32 bytes)

            let mint_authority_present = u32::from_le_bytes(data[0..4].try_into().unwrap()) == 1;
            let freeze_authority_present = u32::from_le_bytes(data[46..50].try_into().unwrap()) == 1;

            let mut warnings = Vec::new();
            if mint_authority_present {
                warnings.push("has mint authority (can inflate)");
            }
            if freeze_authority_present {
                warnings.push("has freeze authority (can freeze)");
            }

            if warnings.is_empty() {
                (true, "authorities revoked".to_string())
            } else {
                (false, warnings.join(", "))
            }
        }
        Err(e) => {
            (false, format!("RPC error: {}", e))
        }
    }
}
