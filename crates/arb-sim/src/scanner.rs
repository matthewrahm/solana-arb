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
const ROUND_TRIP_TX_COST: i64 = 210_000; // 2 * (5000 base + 100000 priority)

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
            trade_size_lamports: 10_000_000_000, // 10 SOL
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

        let input = self.trade_size_lamports as i64;
        let output = leg2_out as i64;
        let gross = output - input;
        let net = gross - ROUND_TRIP_TX_COST;
        let profit_bps = (gross as f64 / input as f64) * 10_000.0;

        Ok(ScanResult {
            scan_type: ScanType::RoundTrip,
            token_symbol: token_symbol.to_string(),
            token_mint: token_mint.to_string(),
            input_lamports: self.trade_size_lamports,
            output_lamports: leg2_out,
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

        // Cost is higher for 3-leg: 3 transactions
        let tri_tx_cost: i64 = 315_000; // 3 * (5000 + 100000)

        let input = self.trade_size_lamports as i64;
        let output = leg3_out as i64;
        let gross = output - input;
        let net = gross - tri_tx_cost;
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
    /// cross-venue pricing with DEX-directed routing.
    ///
    /// Instead of unrestricted Jupiter routing (which optimizes away the spread),
    /// this restricts each leg to specific venues based on the trigger signal.
    pub async fn scan_triggered(
        &self,
        token_mint: &str,
        token_symbol: &str,
        trigger_dex: Dex,
        trigger_direction: SwapDirection,
    ) -> Result<ScanResult> {
        // Strategy based on trigger direction:
        // BUY on trigger_dex -> price UP there -> buy from others, sell on trigger
        // SELL on trigger_dex -> price DOWN there -> buy on trigger, sell on others
        let (buy_restrict, sell_restrict) = match trigger_direction {
            SwapDirection::Buy => (None, Some(trigger_dex)),
            SwapDirection::Sell => (Some(trigger_dex), None),
        };

        // Leg 1: SOL -> TOKEN (buy from cheaper venue)
        let leg1 = self.quote_client
            .get_quote(WSOL_MINT, token_mint, self.trade_size_lamports, self.slippage_bps, buy_restrict)
            .await;

        // Fallback to unrestricted if DEX-restricted quote fails
        let leg1 = match leg1 {
            Ok(q) => q,
            Err(_) if buy_restrict.is_some() => {
                self.quote_client
                    .get_quote(WSOL_MINT, token_mint, self.trade_size_lamports, self.slippage_bps, None)
                    .await?
            }
            Err(e) => return Err(e),
        };

        let leg1_out: u64 = leg1.out_amount.parse()?;
        if leg1_out == 0 {
            anyhow::bail!("Triggered leg 1 returned zero");
        }

        let leg1_route = JupiterQuoteClient::primary_route_label(&leg1)
            .unwrap_or("?").to_string();

        // Leg 2: TOKEN -> SOL (sell on more expensive venue)
        // Rate limiting handled globally by JupiterQuoteClient
        let leg2 = self.quote_client
            .get_quote(token_mint, WSOL_MINT, leg1_out, self.slippage_bps, sell_restrict)
            .await;

        let leg2 = match leg2 {
            Ok(q) => q,
            Err(_) if sell_restrict.is_some() => {
                self.quote_client
                    .get_quote(token_mint, WSOL_MINT, leg1_out, self.slippage_bps, None)
                    .await?
            }
            Err(e) => return Err(e),
        };

        let leg2_out: u64 = leg2.out_amount.parse()?;

        let leg2_route = JupiterQuoteClient::primary_route_label(&leg2)
            .unwrap_or("?").to_string();

        let input = self.trade_size_lamports as i64;
        let output = leg2_out as i64;
        let gross = output - input;
        let net = gross - ROUND_TRIP_TX_COST;
        let profit_bps = (gross as f64 / input as f64) * 10_000.0;

        let tag = match trigger_direction {
            SwapDirection::Buy => format!("{} price UP", trigger_dex),
            SwapDirection::Sell => format!("{} price DOWN", trigger_dex),
        };

        let result = ScanResult {
            scan_type: ScanType::RoundTrip,
            token_symbol: token_symbol.to_string(),
            token_mint: token_mint.to_string(),
            input_lamports: self.trade_size_lamports,
            output_lamports: leg2_out,
            gross_profit_lamports: gross,
            net_profit_lamports: net,
            profit_bps,
            route_description: format!(
                "TRIGGERED({}) SOL -[{}]-> {} -[{}]-> SOL",
                tag, leg1_route, token_symbol, leg2_route
            ),
            profitable: net > 0,
        };

        if result.profitable {
            info!(
                "TRIGGERED *** PROFIT *** {} | +{:.6} SOL | {:.1} bps | {}",
                token_symbol,
                net as f64 / 1e9,
                profit_bps,
                result.route_description,
            );
        }

        Ok(result)
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
            tx_fee_lamports: Some(210_000),
            priority_fee_lamports: Some(200_000),
            simulation_success: true,
            error_message: None,
            simulated_at: Utc::now(),
        }
    }
}
