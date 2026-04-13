//! Profitability scanner: periodically checks for profitable round-trips
//! and triangular arbitrage cycles via Jupiter Quote API.

use std::sync::Arc;

use anyhow::Result;
use arb_types::{Dex, SimResult, SwapDirection, WSOL_MINT};
use chrono::Utc;
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

use crate::jupiter_quote::JupiterQuoteClient;

/// Estimated total tx cost for a round-trip (2 swaps)
/// Realistic for mainnet during memecoin activity: 2 * (5000 base + 1,000,000 priority)
const ROUND_TRIP_TX_COST: i64 = 2_010_000;

/// Estimated total tx cost for a triangular arb (3 swaps)
const TRI_TX_COST: i64 = 3_015_000;

/// Execution penalty in bps applied to quote output to model real-world losses.
/// 30 bps slippage (quotes are optimistic) + 20 bps MEV/sandwich cost = 50 bps.
const EXECUTION_PENALTY_BPS: f64 = 50.0;

/// Result of a profitability scan
#[derive(Debug, Clone)]
pub struct ScanResult {
    pub scan_type: ScanType,
    pub token_symbol: String,
    pub token_mint: String,
    pub input_lamports: u64,
    pub output_lamports: u64,
    pub gross_profit_lamports: i64,
    pub net_profit_lamports: i64,
    pub profit_bps: f64,
    pub route_description: String,
    pub profitable: bool,
}

#[derive(Debug, Clone)]
pub enum ScanType {
    RoundTrip,                    // SOL -> TOKEN -> SOL
    Triangular { via: String },   // SOL -> TOKEN -> STABLE -> SOL
}

impl std::fmt::Display for ScanType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanType::RoundTrip => write!(f, "ROUND-TRIP"),
            ScanType::Triangular { via } => write!(f, "TRI-ARB({})", via),
        }
    }
}

#[derive(Clone)]
pub struct ProfitScanner {
    quote_client: Arc<JupiterQuoteClient>,
    pub trade_size_lamports: u64,
    pub slippage_bps: u16,
    pub sol_usd_price: Arc<RwLock<f64>>,
}

impl ProfitScanner {
    pub fn new(sol_usd_price: Arc<RwLock<f64>>) -> Self {
        Self {
            quote_client: Arc::new(JupiterQuoteClient::new()),
            trade_size_lamports: 1_000_000_000, // 1 SOL (realistic for micro-cap pools)
            slippage_bps: 50,
            sol_usd_price,
        }
    }

    /// Run a round-trip scan: SOL -> token -> SOL via Jupiter's best route.
    /// If output > input, Jupiter found a market inefficiency.
    pub async fn scan_round_trip(&self, token_mint: &str, token_symbol: &str) -> Result<ScanResult> {
        // Leg 1: SOL -> TOKEN (unrestricted routing)
        let leg1 = self.quote_client
            .get_quote(WSOL_MINT, token_mint, self.trade_size_lamports, self.slippage_bps, None)
            .await?;

        let leg1_out: u64 = leg1.out_amount.parse()?;
        if leg1_out == 0 {
            anyhow::bail!("Leg 1 returned zero");
        }

        let leg1_route = JupiterQuoteClient::primary_route_label(&leg1)
            .unwrap_or("?").to_string();

        // Leg 2: TOKEN -> SOL (unrestricted routing)
        // Rate limiting is handled globally by JupiterQuoteClient
        let leg2 = self.quote_client
            .get_quote(token_mint, WSOL_MINT, leg1_out, self.slippage_bps, None)
            .await?;

        let leg2_out: u64 = leg2.out_amount.parse()?;

        let leg2_route = JupiterQuoteClient::primary_route_label(&leg2)
            .unwrap_or("?").to_string();

        // Apply execution penalty: real execution always gets less than the quote
        let realistic_output = apply_execution_penalty(leg2_out);

        let input = self.trade_size_lamports as i64;
        let output = realistic_output as i64;
        let gross = output - input;
        let net = gross - ROUND_TRIP_TX_COST;
        let profit_bps = (gross as f64 / input as f64) * 10_000.0;

        Ok(ScanResult {
            scan_type: ScanType::RoundTrip,
            token_symbol: token_symbol.to_string(),
            token_mint: token_mint.to_string(),
            input_lamports: self.trade_size_lamports,
            output_lamports: realistic_output,
            gross_profit_lamports: gross,
            net_profit_lamports: net,
            profit_bps,
            route_description: format!("SOL -[{}]-> {} -[{}]-> SOL", leg1_route, token_symbol, leg2_route),
            profitable: net > 0,
        })
    }

    /// Run a triangular arb scan: SOL -> TOKEN -> STABLE -> SOL
    pub async fn scan_triangular(
        &self,
        token_mint: &str,
        token_symbol: &str,
        stable_mint: &str,
        stable_symbol: &str,
    ) -> Result<ScanResult> {
        // Leg 1: SOL -> TOKEN
        let leg1 = self.quote_client
            .get_quote(WSOL_MINT, token_mint, self.trade_size_lamports, self.slippage_bps, None)
            .await?;
        let leg1_out: u64 = leg1.out_amount.parse()?;
        if leg1_out == 0 { anyhow::bail!("Leg 1 zero"); }

        // Leg 2: TOKEN -> STABLE (rate limiting handled by quote client)
        let leg2 = self.quote_client
            .get_quote(token_mint, stable_mint, leg1_out, self.slippage_bps, None)
            .await?;
        let leg2_out: u64 = leg2.out_amount.parse()?;
        if leg2_out == 0 { anyhow::bail!("Leg 2 zero"); }

        // Leg 3: STABLE -> SOL
        let leg3 = self.quote_client
            .get_quote(stable_mint, WSOL_MINT, leg2_out, self.slippage_bps, None)
            .await?;
        let leg3_out: u64 = leg3.out_amount.parse()?;

        // Apply execution penalty to final output
        let realistic_output = apply_execution_penalty(leg3_out);

        let input = self.trade_size_lamports as i64;
        let output = realistic_output as i64;
        let gross = output - input;
        let net = gross - TRI_TX_COST;
        let profit_bps = (gross as f64 / input as f64) * 10_000.0;

        let l1 = JupiterQuoteClient::primary_route_label(&leg1).unwrap_or("?");
        let l2 = JupiterQuoteClient::primary_route_label(&leg2).unwrap_or("?");
        let l3 = JupiterQuoteClient::primary_route_label(&leg3).unwrap_or("?");

        Ok(ScanResult {
            scan_type: ScanType::Triangular { via: stable_symbol.to_string() },
            token_symbol: token_symbol.to_string(),
            token_mint: token_mint.to_string(),
            input_lamports: self.trade_size_lamports,
            output_lamports: leg3_out,
            gross_profit_lamports: gross,
            net_profit_lamports: net,
            profit_bps,
            route_description: format!(
                "SOL -[{}]-> {} -[{}]-> {} -[{}]-> SOL",
                l1, token_symbol, l2, stable_symbol, l3
            ),
            profitable: net > 0,
        })
    }

    /// Run a full scan cycle across all tokens and arb types.
    /// Returns all results (profitable and unprofitable for logging).
    pub async fn run_full_scan(
        &self,
        tokens: &[(&str, &str)],      // (mint, symbol)
        stablecoins: &[(&str, &str)],  // (mint, symbol)
    ) -> Vec<ScanResult> {
        let mut results = Vec::new();

        for (mint, symbol) in tokens {
            // Round-trip scan
            match self.scan_round_trip(mint, symbol).await {
                Ok(r) => results.push(r),
                Err(e) => warn!("Round-trip scan failed for {}: {}", symbol, e),
            }

            // Triangular scans via each stablecoin
            // Rate limiting is handled globally by JupiterQuoteClient
            for (stable_mint, stable_symbol) in stablecoins {
                match self.scan_triangular(mint, symbol, stable_mint, stable_symbol).await {
                    Ok(r) => results.push(r),
                    Err(e) => warn!("Tri-arb scan failed for {} via {}: {}", symbol, stable_symbol, e),
                }
            }
        }

        results
    }
}

impl ProfitScanner {
    /// Swap-triggered scan: reacts to an on-chain swap signal by checking
    /// cross-venue pricing with DEX-restricted routing on BOTH legs.
    ///
    /// Both legs must be DEX-restricted to find real cross-venue arbitrage.
    /// If either leg is unrestricted, Jupiter optimizes the spread away.
    /// Tries each alternative DEX as the counter-venue.
    pub async fn scan_triggered(
        &self,
        token_mint: &str,
        token_symbol: &str,
        trigger_dex: Dex,
        trigger_direction: SwapDirection,
    ) -> Result<ScanResult> {
        let alternative_dexes = [Dex::Raydium, Dex::Orca, Dex::Meteora];

        // Determine which DEX to buy on and which to try selling on
        let (buy_dex, sell_dexes): (Dex, Vec<Dex>) = match trigger_direction {
            // Someone BOUGHT on trigger_dex -> price UP there
            // Buy from alternatives (still cheap), sell on trigger_dex (now expensive)
            SwapDirection::Buy => {
                let alts: Vec<Dex> = alternative_dexes.iter()
                    .copied()
                    .filter(|d| *d != trigger_dex)
                    .collect();
                // We'll iterate: buy on each alt, sell on trigger
                // Return best result
                (trigger_dex, alts) // sell_dex = trigger, buy from alts
            }
            // Someone SOLD on trigger_dex -> price DOWN there
            // Buy on trigger_dex (now cheap), sell on alternatives (still expensive)
            SwapDirection::Sell => {
                let alts: Vec<Dex> = alternative_dexes.iter()
                    .copied()
                    .filter(|d| *d != trigger_dex)
                    .collect();
                (trigger_dex, alts) // buy_dex = trigger, sell on alts
            }
        };

        let mut best_result: Option<ScanResult> = None;

        for alt_dex in &sell_dexes {
            let (leg1_restrict, leg2_restrict) = match trigger_direction {
                SwapDirection::Buy => (Some(*alt_dex), Some(trigger_dex)),
                SwapDirection::Sell => (Some(trigger_dex), Some(*alt_dex)),
            };

            // Leg 1: SOL -> TOKEN (restricted to buy venue)
            let leg1 = match self.quote_client
                .get_quote(WSOL_MINT, token_mint, self.trade_size_lamports, self.slippage_bps, leg1_restrict)
                .await
            {
                Ok(q) => q,
                Err(_) => continue, // this DEX doesn't have a pool for this token
            };

            let leg1_out: u64 = match leg1.out_amount.parse() {
                Ok(v) if v > 0 => v,
                _ => continue,
            };

            let leg1_route = JupiterQuoteClient::primary_route_label(&leg1)
                .unwrap_or("?").to_string();

            // Leg 2: TOKEN -> SOL (restricted to sell venue)
            let leg2 = match self.quote_client
                .get_quote(token_mint, WSOL_MINT, leg1_out, self.slippage_bps, leg2_restrict)
                .await
            {
                Ok(q) => q,
                Err(_) => continue,
            };

            let leg2_out: u64 = match leg2.out_amount.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };

            let leg2_route = JupiterQuoteClient::primary_route_label(&leg2)
                .unwrap_or("?").to_string();

            let realistic_output = apply_execution_penalty(leg2_out);

            let input = self.trade_size_lamports as i64;
            let output = realistic_output as i64;
            let gross = output - input;
            let net = gross - ROUND_TRIP_TX_COST;
            let profit_bps = (gross as f64 / input as f64) * 10_000.0;

            let result = ScanResult {
                scan_type: ScanType::RoundTrip,
                token_symbol: token_symbol.to_string(),
                token_mint: token_mint.to_string(),
                input_lamports: self.trade_size_lamports,
                output_lamports: realistic_output,
                gross_profit_lamports: gross,
                net_profit_lamports: net,
                profit_bps,
                route_description: format!(
                    "TRIGGERED({} {}) SOL -[{}]-> {} -[{}]-> SOL",
                    trigger_dex, trigger_direction,
                    leg1_route, token_symbol, leg2_route
                ),
                profitable: net > 0,
            };

            if result.profitable {
                info!(
                    "TRIGGERED *** PROFIT *** {} via {} | +{:.6} SOL | {:.1} bps | {}",
                    token_symbol, alt_dex,
                    net as f64 / 1e9,
                    profit_bps,
                    result.route_description,
                );
                return Ok(result); // Found profit, return immediately
            }

            // Track best (least negative) result
            match &best_result {
                None => best_result = Some(result),
                Some(prev) if net > prev.net_profit_lamports => best_result = Some(result),
                _ => {}
            }
        }

        best_result.ok_or_else(|| anyhow::anyhow!(
            "No DEX routes available for {} on alternatives to {}",
            token_symbol, buy_dex
        ))
    }

    /// Graduation sniper: repeatedly scan a just-graduated token for 60 seconds.
    /// The idea: right after PumpFun migration, the new pool may be mispriced
    /// relative to Jupiter's optimal routing for 10-30 seconds.
    pub async fn snipe_graduation(&self, token_mint: &str, token_symbol: &str) -> Vec<ScanResult> {
        let mut results = Vec::new();
        let start = std::time::Instant::now();
        let max_duration = std::time::Duration::from_secs(60);
        let check_interval = std::time::Duration::from_secs(5);
        let mut attempt = 0;

        tracing::info!("SNIPER: starting graduation scan for {} ({})", token_symbol, &token_mint[..8]);

        // Wait a few seconds for the new pool to initialize
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        while start.elapsed() < max_duration {
            attempt += 1;

            match self.scan_round_trip(token_mint, token_symbol).await {
                Ok(r) => {
                    let net_sol = r.net_profit_lamports as f64 / 1e9;
                    let sol_usd = *self.sol_usd_price.read().await;

                    if r.profitable {
                        tracing::info!(
                            "SNIPER *** PROFIT *** {} attempt #{} | +{:.6} SOL (${:.4}) | {}",
                            token_symbol, attempt, net_sol, net_sol * sol_usd, r.route_description
                        );
                    } else {
                        tracing::info!(
                            "SNIPER {} attempt #{} | {:.6} SOL (${:.4}) | {:.1} bps | {}",
                            token_symbol, attempt, net_sol, net_sol * sol_usd, r.profit_bps, r.route_description
                        );
                    }

                    results.push(r);
                }
                Err(e) => {
                    tracing::warn!("SNIPER {} attempt #{} failed: {}", token_symbol, attempt, e);
                }
            }

            tokio::time::sleep(check_interval).await;
        }

        let profitable = results.iter().filter(|r| r.profitable).count();
        tracing::info!(
            "SNIPER: {} scan complete. {} attempts, {} profitable",
            token_symbol, results.len(), profitable
        );

        results
    }
}

impl ScanResult {
    /// Convert to SimResult for database storage
    pub fn to_sim_result(&self) -> SimResult {
        SimResult {
            id: Uuid::new_v4(),
            opportunity_id: Uuid::nil(), // no linked opportunity
            input_amount: self.input_lamports as i64,
            input_mint: WSOL_MINT.to_string(),
            simulated_output: Some(self.output_lamports as i64),
            output_mint: WSOL_MINT.to_string(),
            simulated_profit_lamports: Some(self.net_profit_lamports),
            tx_fee_lamports: Some(ROUND_TRIP_TX_COST),
            priority_fee_lamports: Some(2_000_000),
            simulation_success: true,
            error_message: None,
            simulated_at: Utc::now(),
        }
    }
}

/// Apply execution penalty to a Jupiter quote output to model real-world losses.
/// Jupiter quotes are optimistic -- real execution always gets less due to
/// slippage (30 bps) and MEV/sandwich attacks (20 bps).
fn apply_execution_penalty(quote_output: u64) -> u64 {
    let penalty_fraction = EXECUTION_PENALTY_BPS / 10_000.0;
    let deduction = (quote_output as f64 * penalty_fraction) as u64;
    quote_output.saturating_sub(deduction)
}
