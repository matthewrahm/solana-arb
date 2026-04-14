//! Local cross-venue scanner: detects arbitrage using direct AMM math.
//! No Jupiter dependency. Fetches pool reserves via RPC, computes outputs locally.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use arb_feed::pool_decoder;
use arb_types::{CrossVenueResult, Dex, KnownPool, SwapDirection, WSOL_MINT};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::amm_quoter::AmmQuoter;

/// Estimated total tx cost for a 2-leg arb (2 swaps via Jito bundle).
/// Base fee (5K) + priority fee (1M) per swap, plus Jito tip estimate.
const TX_COST_LAMPORTS: i64 = 2_500_000;

/// Execution penalty in bps to model real-world losses (slippage, latency).
const EXECUTION_PENALTY_BPS: f64 = 30.0;

/// Pool registry: maps token_mint -> list of known pools with vault info.
pub type PoolRegistry = Arc<RwLock<HashMap<String, Vec<KnownPool>>>>;

/// Create an empty pool registry.
pub fn new_pool_registry() -> PoolRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

#[derive(Clone)]
pub struct LocalScanner {
    quoter: AmmQuoter,
    pool_registry: PoolRegistry,
    client: Client,
    rpc_url: String,
    pub sol_usd_price: Arc<RwLock<f64>>,
}

// ── DexScreener types for pool lookup ──

#[derive(Debug, Deserialize)]
struct DexScreenerPairsResponse {
    pairs: Option<Vec<DexScreenerPair>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DexScreenerPair {
    pair_address: String,
    dex_id: String,
    base_token: DexScreenerToken,
    quote_token: DexScreenerToken,
    liquidity: Option<DexScreenerLiquidity>,
}

#[derive(Debug, Deserialize)]
struct DexScreenerToken {
    address: String,
}

#[derive(Debug, Deserialize)]
struct DexScreenerLiquidity {
    usd: Option<f64>,
}

// ── RPC response types ──

#[derive(Debug, Deserialize)]
struct RpcResponse<T> {
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct GetMultipleAccountsResult {
    value: Vec<Option<AccountInfo>>,
}

#[derive(Debug, Deserialize)]
struct AccountInfo {
    data: (String, String),
}

impl LocalScanner {
    pub fn new(
        rpc_url: &str,
        sol_usd_price: Arc<RwLock<f64>>,
    ) -> Self {
        Self {
            quoter: AmmQuoter::new(rpc_url),
            pool_registry: new_pool_registry(),
            client: Client::new(),
            rpc_url: rpc_url.to_string(),
            sol_usd_price,
        }
    }

    /// Get the pool registry (for external population from discovery).
    pub fn registry(&self) -> &PoolRegistry {
        &self.pool_registry
    }

    /// Scan cross-venue arbitrage triggered by a forge swap signal.
    /// Returns the best result (or all results if needed).
    pub async fn scan_triggered(
        &self,
        token_mint: &str,
        token_symbol: &str,
        trigger_dex: Dex,
        trigger_direction: SwapDirection,
        signal_sol: f64,
    ) -> Result<CrossVenueResult> {
        // Dynamic trade size: 20% of triggering swap, clamped 0.1-2 SOL
        let trade_sol = (signal_sol * 0.2).clamp(0.1, 2.0);
        let trade_lamports = (trade_sol * 1e9) as u64;

        // Get known pools for this token
        let pools = self.get_or_discover_pools(token_mint).await?;

        // Check we have pools on at least 2 different DEXs
        let dex_set: Vec<Dex> = pools.iter().map(|p| p.dex).collect::<std::collections::HashSet<_>>().into_iter().collect();
        if dex_set.len() < 2 {
            anyhow::bail!(
                "Need pools on 2+ different DEXs, found {} for {}",
                dex_set.len(),
                token_symbol
            );
        }

        info!(
            "LOCAL SCAN: {} pools for {} (trigger: {} {} {:.1} SOL)",
            pools.len(),
            token_symbol,
            trigger_dex,
            trigger_direction,
            signal_sol
        );

        // Batch-fetch all pool reserves in one RPC call
        let quotes = self.quoter.quote_all_pools(&pools, WSOL_MINT, trade_lamports).await;

        // Build map of pool_address -> (buy_quote, sell_quote)
        // buy_quote = SOL -> TOKEN, sell_quote = TOKEN -> SOL
        let mut buy_quotes: Vec<(usize, u64)> = Vec::new(); // (pool_idx, tokens_received)
        for (i, result) in quotes.iter().enumerate() {
            match result {
                Ok(q) if q.output_amount > 0 => {
                    buy_quotes.push((i, q.output_amount));
                }
                Ok(_) => debug!("Pool {} returned zero output", &pools[i].pool_address[..8]),
                Err(e) => debug!("Pool {} quote failed: {}", &pools[i].pool_address[..8], e),
            }
        }

        // For each buy venue, compute sell on every other venue
        let mut best_result: Option<CrossVenueResult> = None;

        for &(buy_idx, tokens_received) in &buy_quotes {
            let buy_pool = &pools[buy_idx];

            // Get sell quotes: TOKEN -> SOL on other pools (DIFFERENT DEX only)
            let sell_pools: Vec<&KnownPool> = pools
                .iter()
                .enumerate()
                .filter(|(i, p)| *i != buy_idx && p.dex != buy_pool.dex)
                .map(|(_, p)| p)
                .collect();

            for sell_pool in sell_pools {
                let sell_result = self
                    .quoter
                    .quote_swap(sell_pool, token_mint, tokens_received)
                    .await;

                let sol_out = match sell_result {
                    Ok(q) if q.output_amount > 0 => q.output_amount,
                    _ => continue,
                };

                // Sanity check: output can't be >5x input for a SOL round-trip
                if sol_out > trade_lamports * 5 {
                    warn!(
                        "Sanity check failed: {} sell on {} returned {} from {} input (>5x), skipping",
                        token_symbol, sell_pool.dex, sol_out, trade_lamports
                    );
                    continue;
                }

                // Apply execution penalty
                let penalty = (sol_out as f64 * EXECUTION_PENALTY_BPS / 10_000.0) as u64;
                let realistic_out = sol_out.saturating_sub(penalty);

                let input = trade_lamports as i64;
                let output = realistic_out as i64;
                let gross = output - input;
                let net = gross - TX_COST_LAMPORTS;
                let profit_bps = if input > 0 {
                    (gross as f64 / input as f64) * 10_000.0
                } else {
                    0.0
                };

                let result = CrossVenueResult {
                    token_mint: token_mint.to_string(),
                    token_symbol: token_symbol.to_string(),
                    buy_pool: buy_pool.clone(),
                    sell_pool: sell_pool.clone(),
                    input_lamports: trade_lamports,
                    tokens_received,
                    output_lamports: realistic_out,
                    gross_profit_lamports: gross,
                    net_profit_lamports: net,
                    profit_bps,
                    profitable: net > 0,
                };

                if result.profitable {
                    let sol_usd = *self.sol_usd_price.read().await;
                    let net_sol = net as f64 / 1e9;
                    info!(
                        "LOCAL *** PROFIT *** {} | buy {} sell {} | +{:.6} SOL (${:.4}) | {:.1} bps",
                        token_symbol,
                        buy_pool.dex,
                        sell_pool.dex,
                        net_sol,
                        net_sol * sol_usd,
                        profit_bps,
                    );
                    return Ok(result);
                }

                // Track best (least negative)
                match &best_result {
                    None => best_result = Some(result),
                    Some(prev) if net > prev.net_profit_lamports => {
                        best_result = Some(result);
                    }
                    _ => {}
                }
            }
        }

        best_result.ok_or_else(|| {
            anyhow::anyhow!("No valid cross-venue routes for {}", token_symbol)
        })
    }

    /// Periodic cross-venue scan for a token (not triggered by a signal).
    /// Uses fixed 1 SOL trade size. Returns best result across all venue pairs.
    pub async fn scan_token(
        &self,
        token_mint: &str,
        token_symbol: &str,
    ) -> Result<CrossVenueResult> {
        let trade_lamports: u64 = 1_000_000_000; // 1 SOL

        let pools = self.get_or_discover_pools(token_mint).await?;

        // Log pool breakdown by DEX for debugging
        let dex_counts: HashMap<Dex, usize> = pools.iter().fold(HashMap::new(), |mut m, p| {
            *m.entry(p.dex).or_insert(0) += 1;
            m
        });
        let unique_dexes = dex_counts.len();
        info!(
            "SCAN {}: {} pools across {} DEXs: {}",
            token_symbol,
            pools.len(),
            unique_dexes,
            dex_counts.iter().map(|(d, c)| format!("{}x{}", c, d)).collect::<Vec<_>>().join(", ")
        );

        if unique_dexes < 2 {
            anyhow::bail!(
                "Need pools on 2+ different DEXs, found {} DEX(es) for {}",
                unique_dexes,
                token_symbol
            );
        }

        // Quote each pool individually (dedup keeps this to 2-3 pools max)
        let mut buy_quotes: Vec<(usize, u64)> = Vec::new();
        for (i, pool) in pools.iter().enumerate() {
            match self.quoter.quote_swap(pool, WSOL_MINT, trade_lamports).await {
                Ok(q) if q.output_amount > 0 => {
                    info!(
                        "  BUY quote: {} pool {} -> {} tokens",
                        pool.dex, &pool.pool_address[..8], q.output_amount
                    );
                    buy_quotes.push((i, q.output_amount));
                }
                Ok(_q) => {
                    info!("  BUY quote: {} pool {} -> 0 tokens (empty)", pool.dex, &pool.pool_address[..8]);
                }
                Err(e) => {
                    info!("  BUY quote: {} pool {} -> FAILED: {}", pool.dex, &pool.pool_address[..8], e);
                }
            }
        }

        let mut best_result: Option<CrossVenueResult> = None;

        for &(buy_idx, tokens_received) in &buy_quotes {
            let buy_pool = &pools[buy_idx];

            for (sell_idx, sell_pool) in pools.iter().enumerate() {
                // Skip same pool AND same DEX (cross-venue only)
                if sell_idx == buy_idx || sell_pool.dex == buy_pool.dex {
                    continue;
                }

                let sol_out = match self
                    .quoter
                    .quote_swap(sell_pool, token_mint, tokens_received)
                    .await
                {
                    Ok(q) if q.output_amount > 0 => q.output_amount,
                    _ => continue,
                };

                // Sanity check: output can't be >5x input for a SOL round-trip
                if sol_out > trade_lamports * 5 {
                    warn!(
                        "Sanity check failed: {} sell on {} returned {} from {} input (>5x), skipping",
                        token_symbol, sell_pool.dex, sol_out, trade_lamports
                    );
                    continue;
                }

                let penalty = (sol_out as f64 * EXECUTION_PENALTY_BPS / 10_000.0) as u64;
                let realistic_out = sol_out.saturating_sub(penalty);

                let input = trade_lamports as i64;
                let output = realistic_out as i64;
                let gross = output - input;
                let net = gross - TX_COST_LAMPORTS;
                let profit_bps = if input > 0 {
                    (gross as f64 / input as f64) * 10_000.0
                } else {
                    0.0
                };

                let result = CrossVenueResult {
                    token_mint: token_mint.to_string(),
                    token_symbol: token_symbol.to_string(),
                    buy_pool: buy_pool.clone(),
                    sell_pool: sell_pool.clone(),
                    input_lamports: trade_lamports,
                    tokens_received,
                    output_lamports: realistic_out,
                    gross_profit_lamports: gross,
                    net_profit_lamports: net,
                    profit_bps,
                    profitable: net > 0,
                };

                match &best_result {
                    None => best_result = Some(result),
                    Some(prev) if net > prev.net_profit_lamports => {
                        best_result = Some(result);
                    }
                    _ => {}
                }
            }
        }

        best_result.ok_or_else(|| {
            anyhow::anyhow!("No valid cross-venue routes for {}", token_symbol)
        })
    }

    /// Keep at most 1 pool per DEX to minimize RPC calls.
    /// DexScreener returns pools sorted by liquidity, so the first per DEX is the best.
    fn dedup_by_dex(pools: &[KnownPool]) -> Vec<KnownPool> {
        let mut seen = HashMap::new();
        let mut result = Vec::new();
        for pool in pools {
            if !seen.contains_key(&pool.dex) {
                seen.insert(pool.dex, true);
                result.push(pool.clone());
            }
        }
        result
    }

    /// Register pools for a token in the registry (called from discovery).
    pub async fn register_pools(&self, token_mint: &str, pools: Vec<KnownPool>) {
        if !pools.is_empty() {
            let mut reg = self.pool_registry.write().await;
            reg.insert(token_mint.to_string(), pools);
        }
    }

    /// Get pools from registry, or discover them via DexScreener + RPC.
    /// Returns at most 1 pool per DEX (best liquidity) to keep RPC batch small.
    async fn get_or_discover_pools(&self, token_mint: &str) -> Result<Vec<KnownPool>> {
        // Check registry first
        {
            let reg = self.pool_registry.read().await;
            if let Some(pools) = reg.get(token_mint) {
                if !pools.is_empty() {
                    return Ok(Self::dedup_by_dex(pools));
                }
            }
        }

        // Discover pools via DexScreener
        let pools = self.discover_pools_for_token(token_mint).await?;
        let pools = Self::dedup_by_dex(&pools);

        // Cache in registry
        if !pools.is_empty() {
            let mut reg = self.pool_registry.write().await;
            reg.insert(token_mint.to_string(), pools.clone());
        }

        Ok(pools)
    }

    /// Query DexScreener for all pools for a token, then resolve vault addresses via RPC.
    pub async fn discover_pools_for_token(&self, token_mint: &str) -> Result<Vec<KnownPool>> {
        let url = format!(
            "https://api.dexscreener.com/latest/dex/tokens/{}",
            token_mint
        );

        let resp: DexScreenerPairsResponse = self
            .client
            .get(&url)
            .send()
            .await?
            .json()
            .await?;

        let pairs = resp.pairs.unwrap_or_default();
        let mut pools = Vec::new();

        for pair in pairs {
            let dex = Dex::from_dexscreener_id(&pair.dex_id);

            // Only support DEXes we can locally quote
            if !matches!(dex, Dex::Raydium | Dex::PumpSwap | Dex::PumpFun | Dex::Orca) {
                continue;
            }

            // Skip very low liquidity pools
            let liquidity = pair
                .liquidity
                .and_then(|l| l.usd)
                .unwrap_or(0.0);
            if liquidity < 1000.0 {
                continue;
            }

            let base_mint = pair.base_token.address;
            let quote_mint = pair.quote_token.address;

            let known_pool = KnownPool {
                pool_address: pair.pair_address.clone(),
                dex,
                base_mint: base_mint.clone(),
                quote_mint: quote_mint.clone(),
                vault_addresses: None, // resolved below
            };

            pools.push(known_pool);
        }

        // Resolve vault addresses for Raydium pools
        self.resolve_raydium_vaults(&mut pools).await;

        debug!(
            "Discovered {} quotable pools for {}",
            pools.len(),
            &token_mint[..8]
        );

        Ok(pools)
    }

    /// For Raydium pools, fetch the pool account and decode vault pubkeys.
    async fn resolve_raydium_vaults(&self, pools: &mut Vec<KnownPool>) {
        let raydium_pools: Vec<usize> = pools
            .iter()
            .enumerate()
            .filter(|(_, p)| p.dex == Dex::Raydium && p.vault_addresses.is_none())
            .map(|(i, _)| i)
            .collect();

        if raydium_pools.is_empty() {
            return;
        }

        // Batch-fetch pool accounts
        let addresses: Vec<&str> = raydium_pools
            .iter()
            .map(|&i| pools[i].pool_address.as_str())
            .collect();

        let accounts = match self.fetch_accounts(&addresses).await {
            Ok(a) => a,
            Err(e) => {
                warn!("Failed to fetch Raydium pool accounts: {}", e);
                return;
            }
        };

        for &idx in &raydium_pools {
            let pool = &mut pools[idx];
            if let Some(data) = accounts.get(&pool.pool_address) {
                // Raydium V4 pools are 752 bytes. Shorter accounts are likely
                // Raydium CLMM (concentrated liquidity) which has a different
                // layout we don't support yet. Skip silently.
                if data.len() < 752 {
                    debug!(
                        "Raydium pool {} is {} bytes (likely CLMM, need V4 >= 752), skipping",
                        &pool.pool_address[..8], data.len()
                    );
                    continue;
                }
                match pool_decoder::decode_raydium_pool_vaults(data) {
                    Ok((coin_vault, pc_vault)) => {
                        pool.vault_addresses = Some((
                            bs58::encode(coin_vault).into_string(),
                            bs58::encode(pc_vault).into_string(),
                        ));
                        debug!(
                            "Resolved Raydium vaults for pool {}",
                            &pool.pool_address[..8]
                        );
                    }
                    Err(e) => {
                        debug!(
                            "Failed to decode Raydium pool {}: {}",
                            &pool.pool_address[..8],
                            e
                        );
                    }
                }
            }
        }

        // Remove Raydium pools where we couldn't resolve vaults
        pools.retain(|p| p.dex != Dex::Raydium || p.vault_addresses.is_some());
    }

    /// Fetch account data for multiple addresses via RPC.
    async fn fetch_accounts(&self, addresses: &[&str]) -> Result<HashMap<String, Vec<u8>>> {
        if addresses.is_empty() {
            return Ok(HashMap::new());
        }

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getMultipleAccounts",
            "params": [addresses, { "encoding": "base64" }]
        });

        let resp: RpcResponse<GetMultipleAccountsResult> = self
            .client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        let result = resp
            .result
            .ok_or_else(|| anyhow::anyhow!("RPC null result"))?;

        let mut out = HashMap::new();
        for (i, account) in result.value.into_iter().enumerate() {
            if let Some(info) = account {
                use base64::Engine;
                let data =
                    base64::engine::general_purpose::STANDARD.decode(&info.data.0)?;
                out.insert(addresses[i].to_string(), data);
            }
        }
        Ok(out)
    }
}
