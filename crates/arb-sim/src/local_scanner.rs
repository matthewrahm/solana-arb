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
use crate::cost_model::CostModel;

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
    pub cost_model: CostModel,
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
        Self::with_cost_model(rpc_url, sol_usd_price, CostModel::default())
    }

    pub fn with_cost_model(
        rpc_url: &str,
        sol_usd_price: Arc<RwLock<f64>>,
        cost_model: CostModel,
    ) -> Self {
        Self {
            quoter: AmmQuoter::new(rpc_url),
            pool_registry: new_pool_registry(),
            client: Client::new(),
            rpc_url: rpc_url.to_string(),
            sol_usd_price,
            cost_model,
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

                let econ = self.cost_model.compute(trade_lamports, sol_out);

                let result = CrossVenueResult {
                    token_mint: token_mint.to_string(),
                    token_symbol: token_symbol.to_string(),
                    buy_pool: buy_pool.clone(),
                    sell_pool: sell_pool.clone(),
                    input_lamports: trade_lamports,
                    tokens_received,
                    output_lamports: econ.realistic_output_lamports,
                    gross_profit_lamports: econ.gross_profit_lamports,
                    net_profit_lamports: econ.net_profit_lamports,
                    profit_bps: econ.profit_bps,
                    profitable: econ.net_profit_lamports > 0,
                    fixed_cost_lamports: econ.fixed_cost_lamports,
                    jito_tip_lamports: econ.jito_tip_lamports,
                    slippage_lamports: econ.slippage_lamports,
                };

                if result.profitable {
                    let sol_usd = *self.sol_usd_price.read().await;
                    let net_sol = econ.net_profit_lamports as f64 / 1e9;
                    info!(
                        "LOCAL *** PROFIT *** {} | buy {} sell {} | +{:.6} SOL (${:.4}) | {:.1} bps | tip {} lamports",
                        token_symbol,
                        buy_pool.dex,
                        sell_pool.dex,
                        net_sol,
                        net_sol * sol_usd,
                        econ.profit_bps,
                        econ.jito_tip_lamports,
                    );
                    return Ok(result);
                }

                // Track best (least negative)
                match &best_result {
                    None => best_result = Some(result),
                    Some(prev) if econ.net_profit_lamports > prev.net_profit_lamports => {
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

                let econ = self.cost_model.compute(trade_lamports, sol_out);

                let result = CrossVenueResult {
                    token_mint: token_mint.to_string(),
                    token_symbol: token_symbol.to_string(),
                    buy_pool: buy_pool.clone(),
                    sell_pool: sell_pool.clone(),
                    input_lamports: trade_lamports,
                    tokens_received,
                    output_lamports: econ.realistic_output_lamports,
                    gross_profit_lamports: econ.gross_profit_lamports,
                    net_profit_lamports: econ.net_profit_lamports,
                    profit_bps: econ.profit_bps,
                    profitable: econ.net_profit_lamports > 0,
                    fixed_cost_lamports: econ.fixed_cost_lamports,
                    jito_tip_lamports: econ.jito_tip_lamports,
                    slippage_lamports: econ.slippage_lamports,
                };

                match &best_result {
                    None => best_result = Some(result),
                    Some(prev) if econ.net_profit_lamports > prev.net_profit_lamports => {
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

    /// Same-pool back-run: a large swap just moved the pool's price.
    /// We trade the OPPOSITE direction on the SAME pool to capture mean-reversion,
    /// sized as a fraction of the trigger so we don't cause further slippage.
    ///
    /// Edge hypothesis: a 2+ SOL trade on a thin PumpSwap pool pushes price by 20-50 bps.
    /// For the ~100-500ms before Jupiter + other bots close the window, the post-trade
    /// price is stale relative to the broader market. A small counter-trade + reverse
    /// captures the gap minus costs.
    ///
    /// This is still a round-trip (SOL -> TOKEN -> SOL or TOKEN -> SOL -> TOKEN),
    /// but buy_pool == sell_pool. The profit comes from time-decay of the imbalance,
    /// not cross-venue price diff. Only meaningful if caller can execute within ~1 slot.
    pub async fn scan_same_pool_backrun(
        &self,
        token_mint: &str,
        token_symbol: &str,
        trigger_dex: Dex,
        trigger_direction: SwapDirection,
        signal_sol: f64,
    ) -> Result<CrossVenueResult> {
        // 10% of trigger SOL, clamped 0.05-1 SOL. Smaller than cross-venue because
        // we're trading into the same pool the trigger just impacted.
        let trade_sol = (signal_sol * 0.1).clamp(0.05, 1.0);
        let trade_lamports = (trade_sol * 1e9) as u64;

        let pools = self.get_or_discover_pools(token_mint).await?;

        // Pick first pool on the triggering DEX — pools are already liquidity-sorted
        // via DexScreener discovery + dedup_by_dex keeps one-per-DEX.
        let pool = pools
            .iter()
            .find(|p| p.dex == trigger_dex)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No {} pool found for {} (have {} pools on other DEXs)",
                    trigger_dex, token_symbol, pools.len()
                )
            })?;

        // Leg 1 direction = opposite of trigger.
        //   Trigger BUY (SOL -> TOKEN pushed price up): we SELL first (TOKEN -> SOL), then BUY back (SOL -> TOKEN)
        //   Trigger SELL (TOKEN -> SOL pushed price down): we BUY first (SOL -> TOKEN), then SELL back (TOKEN -> SOL)
        //
        // Both cases are a round-trip on the same pool. We start with SOL either way
        // (we don't hold the token), so actually we can only trade:
        //   BUY trigger: start SOL, swap SOL->TOKEN (into elevated-price side — bad), wait for revert, swap TOKEN->SOL
        //   SELL trigger: start SOL, swap SOL->TOKEN (into depressed-price side — good), wait for revert, swap TOKEN->SOL
        //
        // So ONLY SELL triggers are exploitable starting from SOL. On BUY triggers the
        // mean-reversion exit direction is opposite (we'd need inventory of the token).
        // For now we skip BUY triggers in same-pool mode.
        if trigger_direction == SwapDirection::Buy {
            anyhow::bail!(
                "Same-pool back-run on BUY trigger requires token inventory — skipping {} for now",
                token_symbol
            );
        }

        info!(
            "SAME-POOL BACK-RUN: {} on {} pool {} (trigger {} {:.1} SOL, our trade {:.3} SOL)",
            token_symbol,
            pool.dex,
            &pool.pool_address[..8.min(pool.pool_address.len())],
            trigger_direction,
            signal_sol,
            trade_sol,
        );

        // Leg 1: SOL -> TOKEN on the same pool (buy into the depressed side)
        let leg1 = self.quoter.quote_swap(pool, WSOL_MINT, trade_lamports).await?;
        if leg1.output_amount == 0 {
            anyhow::bail!("Leg 1 quote returned 0 tokens for {}", token_symbol);
        }
        let tokens_received = leg1.output_amount;

        // Leg 2: TOKEN -> SOL on the SAME pool.
        // Note: without modeling the trigger swap's actual impact on reserves, this quote
        // will show a small loss (AMM fee on both legs with no price move). The "edge"
        // is the revert of the trigger's impact, which we don't model in the quoter yet.
        // For Phase B we report raw round-trip economics and flag when the post-trigger
        // mismatch would be favorable.
        let leg2 = self.quoter.quote_swap(pool, token_mint, tokens_received).await?;
        let sol_out = leg2.output_amount;

        // Sanity
        if sol_out > trade_lamports * 5 {
            warn!(
                "Sanity check failed: same-pool sell returned {} from {} input (>5x), skipping",
                sol_out, trade_lamports
            );
            anyhow::bail!("Same-pool sanity fail for {}", token_symbol);
        }

        let econ = self.cost_model.compute(trade_lamports, sol_out);

        let result = CrossVenueResult {
            token_mint: token_mint.to_string(),
            token_symbol: token_symbol.to_string(),
            buy_pool: pool.clone(),
            sell_pool: pool.clone(),
            input_lamports: trade_lamports,
            tokens_received,
            output_lamports: econ.realistic_output_lamports,
            gross_profit_lamports: econ.gross_profit_lamports,
            net_profit_lamports: econ.net_profit_lamports,
            profit_bps: econ.profit_bps,
            profitable: econ.net_profit_lamports > 0,
            fixed_cost_lamports: econ.fixed_cost_lamports,
            jito_tip_lamports: econ.jito_tip_lamports,
            slippage_lamports: econ.slippage_lamports,
        };

        if result.profitable {
            let sol_usd = *self.sol_usd_price.read().await;
            let net_sol = econ.net_profit_lamports as f64 / 1e9;
            info!(
                "SAME-POOL *** PROFIT *** {} | {} pool | +{:.6} SOL (${:.4}) | {:.1} bps | tip {}",
                token_symbol,
                pool.dex,
                net_sol,
                net_sol * sol_usd,
                econ.profit_bps,
                econ.jito_tip_lamports,
            );
        }

        Ok(result)
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
            if !matches!(dex, Dex::Raydium | Dex::RaydiumClmm | Dex::PumpSwap | Dex::PumpFun | Dex::Orca) {
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

        // Resolve vault addresses for Raydium pools (V4 and CLMM)
        self.resolve_raydium_vaults(&mut pools).await;

        // Resolve vault addresses for PumpSwap pools
        self.resolve_pumpswap_vaults(&mut pools).await;

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
                if data.len() == 752 {
                    // Raydium V4 (constant product)
                    match pool_decoder::decode_raydium_pool_vaults(data) {
                        Ok((coin_vault, pc_vault)) => {
                            pool.vault_addresses = Some((
                                bs58::encode(coin_vault).into_string(),
                                bs58::encode(pc_vault).into_string(),
                            ));
                            debug!(
                                "Resolved Raydium V4 vaults for pool {}",
                                &pool.pool_address[..8]
                            );
                        }
                        Err(e) => {
                            debug!("Failed to decode Raydium V4 pool {}: {}", &pool.pool_address[..8], e);
                        }
                    }
                } else if data.len() >= 1544 {
                    // Raydium CLMM (concentrated liquidity)
                    match pool_decoder::decode_raydium_clmm(data) {
                        Ok(state) => {
                            pool.dex = Dex::RaydiumClmm;
                            pool.vault_addresses = Some((
                                bs58::encode(state.vault_0).into_string(),
                                bs58::encode(state.vault_1).into_string(),
                            ));
                            // Store decimals in the base/quote mint fields for later use
                            pool.base_mint = bs58::encode(state.mint_0).into_string();
                            pool.quote_mint = bs58::encode(state.mint_1).into_string();
                            info!(
                                "Resolved Raydium CLMM pool {} (dec {}/{})",
                                &pool.pool_address[..8], state.decimals_0, state.decimals_1
                            );
                        }
                        Err(e) => {
                            debug!("Failed to decode Raydium CLMM pool {}: {}", &pool.pool_address[..8], e);
                        }
                    }
                } else {
                    debug!(
                        "Raydium pool {} is {} bytes (unknown layout), skipping",
                        &pool.pool_address[..8], data.len()
                    );
                }
            }
        }

        // Remove Raydium/RaydiumClmm pools where we couldn't resolve vaults
        pools.retain(|p| !matches!(p.dex, Dex::Raydium | Dex::RaydiumClmm) || p.vault_addresses.is_some());
    }

    /// Fetch account data for multiple addresses via RPC.
    /// For PumpSwap pools, fetch the pool account and decode vault pubkeys.
    async fn resolve_pumpswap_vaults(&self, pools: &mut Vec<KnownPool>) {
        let pumpswap_pools: Vec<usize> = pools
            .iter()
            .enumerate()
            .filter(|(_, p)| p.dex == Dex::PumpSwap && p.vault_addresses.is_none())
            .map(|(i, _)| i)
            .collect();

        if pumpswap_pools.is_empty() {
            return;
        }

        let addresses: Vec<&str> = pumpswap_pools
            .iter()
            .map(|&i| pools[i].pool_address.as_str())
            .collect();

        let accounts = match self.fetch_accounts(&addresses).await {
            Ok(a) => a,
            Err(e) => {
                warn!("Failed to fetch PumpSwap pool accounts: {}", e);
                return;
            }
        };

        for &idx in &pumpswap_pools {
            let pool = &mut pools[idx];
            if let Some(data) = accounts.get(&pool.pool_address) {
                match pool_decoder::decode_pumpswap_pool(data) {
                    Ok(state) => {
                        pool.vault_addresses = Some((
                            bs58::encode(state.base_vault).into_string(),
                            bs58::encode(state.quote_vault).into_string(),
                        ));
                        pool.base_mint = bs58::encode(state.base_mint).into_string();
                        pool.quote_mint = bs58::encode(state.quote_mint).into_string();
                        debug!(
                            "Resolved PumpSwap vaults for pool {}",
                            &pool.pool_address[..8]
                        );
                    }
                    Err(e) => {
                        debug!("Failed to decode PumpSwap pool {}: {}", &pool.pool_address[..8], e);
                    }
                }
            }
        }

        // Remove PumpSwap pools where we couldn't resolve vaults
        pools.retain(|p| p.dex != Dex::PumpSwap || p.vault_addresses.is_some());
    }

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
