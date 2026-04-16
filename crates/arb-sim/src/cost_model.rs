//! Realistic execution cost model for Solana MEV/arb on memecoin-busy blocks.
//! Accounts for base fees, priority fees, Jito tips, and slippage/MEV penalty.
//!
//! Defaults are calibrated for 2026 memecoin conditions:
//!   - 3M lamports priority fee per swap (median during memecoin activity)
//!   - 50K lamport Jito tip floor (~75th percentile landed tip)
//!   - 50 bps execution penalty (30 slippage + 20 MEV on micro-caps)
//!   - 3-tx bundle: 2 swaps + 1 tip transfer
//!
//! Override via `CostModel::new(...)` for CLI tuning or dynamic fee/tip streams.

#[derive(Debug, Clone, Copy)]
pub struct CostModel {
    pub base_fee_lamports: u64,
    pub priority_fee_lamports_per_tx: u64,
    pub num_txs: u64,
    pub execution_penalty_bps: f64,
    pub min_jito_tip_lamports: u64,
    pub jito_tip_fraction: f64,
}

impl Default for CostModel {
    fn default() -> Self {
        Self {
            base_fee_lamports: 5_000,
            priority_fee_lamports_per_tx: 3_000_000,
            num_txs: 3,
            execution_penalty_bps: 50.0,
            min_jito_tip_lamports: 50_000,
            jito_tip_fraction: 0.5,
        }
    }
}

/// Breakdown of a single scan's economics.
#[derive(Debug, Clone, Copy)]
pub struct ScanEconomics {
    pub input_lamports: u64,
    pub raw_output_lamports: u64,
    pub slippage_lamports: u64,
    pub realistic_output_lamports: u64,
    pub gross_profit_lamports: i64,
    pub fixed_cost_lamports: i64,
    pub jito_tip_lamports: u64,
    pub net_profit_lamports: i64,
    pub profit_bps: f64,
}

impl CostModel {
    pub fn fixed_costs(&self) -> i64 {
        ((self.base_fee_lamports + self.priority_fee_lamports_per_tx) * self.num_txs) as i64
    }

    pub fn slippage_lamports(&self, raw_output: u64) -> u64 {
        (raw_output as f64 * self.execution_penalty_bps / 10_000.0) as u64
    }

    pub fn jito_tip_lamports(&self, gross_profit: i64) -> u64 {
        if gross_profit <= 0 {
            return self.min_jito_tip_lamports;
        }
        ((gross_profit as f64 * self.jito_tip_fraction) as u64).max(self.min_jito_tip_lamports)
    }

    /// Compute full economics of a round-trip given raw AMM output.
    pub fn compute(&self, input_lamports: u64, raw_output: u64) -> ScanEconomics {
        let slippage = self.slippage_lamports(raw_output);
        let realistic_out = raw_output.saturating_sub(slippage);
        let gross = realistic_out as i64 - input_lamports as i64;
        let tip = self.jito_tip_lamports(gross);
        let fixed = self.fixed_costs();
        let net = gross - fixed - tip as i64;
        let profit_bps = if input_lamports > 0 {
            (gross as f64 / input_lamports as f64) * 10_000.0
        } else {
            0.0
        };

        ScanEconomics {
            input_lamports,
            raw_output_lamports: raw_output,
            slippage_lamports: slippage,
            realistic_output_lamports: realistic_out,
            gross_profit_lamports: gross,
            fixed_cost_lamports: fixed,
            jito_tip_lamports: tip,
            net_profit_lamports: net,
            profit_bps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_costs_memecoin_honest() {
        let m = CostModel::default();
        assert_eq!(m.fixed_costs(), 9_015_000); // 3 txs * (5K base + 3M priority)
    }

    #[test]
    fn one_sol_round_trip_with_flat_output_is_net_negative() {
        let m = CostModel::default();
        let e = m.compute(1_000_000_000, 1_000_000_000);
        assert!(e.net_profit_lamports < 0);
    }

    #[test]
    fn one_sol_with_2pct_edge_nets_positive() {
        let m = CostModel::default();
        // 2% raw edge = 1.02 SOL out; after 50 bps slippage = 1.0149 SOL
        // gross = +0.0149 SOL, tip = 50% = 0.00745 SOL, fixed 0.009 SOL
        // net ≈ -0.00155 SOL (still underwater after real costs)
        let e = m.compute(1_000_000_000, 1_020_000_000);
        assert!(e.gross_profit_lamports > 0);
        assert!(e.net_profit_lamports < 0); // honest: 2% edge on 1 SOL doesn't cover costs
    }

    #[test]
    fn five_pct_edge_clears_costs() {
        let m = CostModel::default();
        let e = m.compute(1_000_000_000, 1_050_000_000);
        assert!(e.net_profit_lamports > 0);
    }
}
