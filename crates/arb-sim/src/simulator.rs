//! Two-leg arbitrage simulation using Jupiter Quote API.
//! For each detected opportunity, simulates buy + sell legs and calculates P&L.

use std::sync::Arc;

use anyhow::{Context, Result};
use arb_types::{ArbOpportunity, SimResult, WSOL_MINT};
use chrono::Utc;
use tokio::sync::{RwLock, Semaphore};
use tracing::{info, warn};
use uuid::Uuid;

use crate::jupiter_quote::JupiterQuoteClient;

/// Estimated base fee per transaction (lamports)
const BASE_TX_FEE: i64 = 5_000;
/// Estimated priority fee per transaction (lamports)
const PRIORITY_FEE: i64 = 100_000;

#[derive(Clone)]
pub struct Simulator {
    quote_client: Arc<JupiterQuoteClient>,
    /// Trade size in SOL lamports (default: 1 SOL = 1_000_000_000)
    pub trade_size_lamports: u64,
    /// Slippage tolerance in bps
    pub slippage_bps: u16,
    /// Shared SOL/USD price
    pub sol_usd_price: Arc<RwLock<f64>>,
    /// Rate limiter: max concurrent simulations
    semaphore: Arc<Semaphore>,
}

impl Simulator {
    pub fn new(sol_usd_price: Arc<RwLock<f64>>) -> Self {
        Self {
            quote_client: Arc::new(JupiterQuoteClient::new()),
            trade_size_lamports: 1_000_000_000, // 1 SOL
            slippage_bps: 100,                   // 1%
            sol_usd_price,
            semaphore: Arc::new(Semaphore::new(1)), // 1 concurrent simulation
        }
    }

    /// Simulate a two-leg arbitrage opportunity.
    /// Leg 1: Buy base_mint with SOL
    /// Leg 2: Sell base_mint for SOL
    /// Returns SimResult with P&L.
    /// Rate-limited to 1 concurrent simulation via semaphore.
    pub async fn simulate(&self, opp: &ArbOpportunity) -> Result<SimResult> {
        let _permit = self.semaphore.acquire().await?;
        let now = Utc::now();

        // Leg 1: Buy base_mint with SOL
        let leg1 = match self
            .quote_client
            .get_quote(
                WSOL_MINT,
                &opp.base_mint,
                self.trade_size_lamports,
                self.slippage_bps,
            )
            .await
        {
            Ok(q) => q,
            Err(e) => {
                return Ok(self.failed_result(opp, &format!("Leg 1 quote: {}", e)));
            }
        };

        let leg1_out: u64 = leg1
            .out_amount
            .parse()
            .context("Failed to parse leg 1 out_amount")?;

        if leg1_out == 0 {
            return Ok(self.failed_result(opp, "Leg 1 returned zero output"));
        }

        // Brief pause to avoid Jupiter rate limits
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Leg 2: Sell base_mint for SOL (using leg 1 output as input)
        let leg2 = match self
            .quote_client
            .get_quote(
                &opp.base_mint,
                WSOL_MINT,
                leg1_out,
                self.slippage_bps,
            )
            .await
        {
            Ok(q) => q,
            Err(e) => {
                return Ok(self.failed_result(opp, &format!("Leg 2 quote: {}", e)));
            }
        };

        let leg2_out: u64 = leg2
            .out_amount
            .parse()
            .context("Failed to parse leg 2 out_amount")?;

        // P&L calculation
        let input_lamports = self.trade_size_lamports as i64;
        let output_lamports = leg2_out as i64;
        let tx_fees = BASE_TX_FEE * 2;
        let priority_fees = PRIORITY_FEE * 2;
        let gross_profit = output_lamports - input_lamports;
        let net_profit = gross_profit - tx_fees - priority_fees;

        let leg1_label = JupiterQuoteClient::primary_route_label(&leg1)
            .unwrap_or("unknown");
        let leg2_label = JupiterQuoteClient::primary_route_label(&leg2)
            .unwrap_or("unknown");

        let price_impact: f64 = leg1
            .price_impact_pct
            .parse()
            .unwrap_or(0.0)
            + leg2
                .price_impact_pct
                .parse::<f64>()
                .unwrap_or(0.0);

        info!(
            "SIM legs: buy via {} ({} hops), sell via {} ({} hops), impact: {:.4}%",
            leg1_label,
            leg1.route_plan.len(),
            leg2_label,
            leg2.route_plan.len(),
            price_impact
        );

        Ok(SimResult {
            id: Uuid::new_v4(),
            opportunity_id: opp.id,
            input_amount: input_lamports,
            input_mint: WSOL_MINT.to_string(),
            simulated_output: Some(output_lamports),
            output_mint: WSOL_MINT.to_string(),
            simulated_profit_lamports: Some(net_profit),
            tx_fee_lamports: Some(tx_fees),
            priority_fee_lamports: Some(priority_fees),
            simulation_success: true,
            error_message: None,
            simulated_at: now,
        })
    }

    fn failed_result(&self, opp: &ArbOpportunity, error: &str) -> SimResult {
        warn!("Simulation failed for {}: {}", opp.token_symbol, error);
        SimResult {
            id: Uuid::new_v4(),
            opportunity_id: opp.id,
            input_amount: self.trade_size_lamports as i64,
            input_mint: WSOL_MINT.to_string(),
            simulated_output: None,
            output_mint: WSOL_MINT.to_string(),
            simulated_profit_lamports: None,
            tx_fee_lamports: None,
            priority_fee_lamports: None,
            simulation_success: false,
            error_message: Some(error.to_string()),
            simulated_at: Utc::now(),
        }
    }
}
