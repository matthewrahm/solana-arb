use arb_types::{Dex, SwapDirection, SwapSignal};
use tracing::{debug, info};

/// Determines if a swap signal warrants an immediate cross-venue scan.
/// Returns scan parameters if actionable.
pub struct SwapAnalyzer {
    /// Minimum SOL value to consider a swap significant enough to create a spread
    min_sol_threshold: f64,
}

/// A scan request triggered by an on-chain swap signal
#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub token_mint: String,
    pub token_symbol: Option<String>,
    /// The DEX where the triggering swap happened
    pub trigger_dex: Dex,
    /// The direction of the triggering swap (Buy = price went up on trigger_dex)
    pub trigger_direction: SwapDirection,
    /// SOL size of the triggering swap
    pub sol_equivalent: f64,
    /// Suggested venue to buy from (cheaper after the trigger)
    pub buy_dex: Option<Dex>,
    /// Suggested venue to sell on (more expensive after the trigger)
    pub sell_dex: Option<Dex>,
}

impl SwapAnalyzer {
    pub fn new(min_sol_threshold: f64) -> Self {
        Self { min_sol_threshold }
    }

    pub fn analyze(&self, signal: &SwapSignal) -> Option<ScanRequest> {
        let sig_short = &signal.signature[..8.min(signal.signature.len())];

        // Skip signals below threshold
        if signal.sol_equivalent < self.min_sol_threshold {
            debug!("SKIP {} | {:.1} SOL < {:.1} threshold on {} [{}]",
                sig_short, signal.sol_equivalent, self.min_sol_threshold,
                signal.platform, &signal.token_mint[..8.min(signal.token_mint.len())]);
            return None;
        }

        // Skip Jupiter-routed swaps -- Jupiter already optimized the route,
        // so there's no residual spread to capture
        if signal.platform == Dex::Jupiter {
            debug!("SKIP {} | Jupiter-routed {:.1} SOL [{}]",
                sig_short, signal.sol_equivalent, &signal.token_mint[..8.min(signal.token_mint.len())]);
            return None;
        }

        // All non-Jupiter signals above threshold are actionable:
        // PumpFun, PumpSwap, Raydium, Orca -- let the scanner determine
        // if multi-pool cross-venue arb is possible

        // Determine buy/sell venue strategy based on trigger direction
        let (buy_dex, sell_dex) = match signal.direction {
            // Someone BOUGHT token on trigger_dex -> price went UP there
            // Strategy: buy on OTHER venues (still cheap), sell on trigger_dex (now expensive)
            SwapDirection::Buy => (None, Some(signal.platform)),
            // Someone SOLD token on trigger_dex -> price went DOWN there
            // Strategy: buy on trigger_dex (now cheap), sell on OTHER venues (still expensive)
            SwapDirection::Sell => (Some(signal.platform), None),
        };

        info!(
            "SIGNAL ACTIONABLE: {} {:.1} SOL on {} [{}] -- {}",
            signal.direction,
            signal.sol_equivalent,
            signal.platform,
            &signal.token_mint[..8.min(signal.token_mint.len())],
            match signal.direction {
                SwapDirection::Buy => "price UP on trigger, buy elsewhere",
                SwapDirection::Sell => "price DOWN on trigger, buy there",
            }
        );

        Some(ScanRequest {
            token_mint: signal.token_mint.clone(),
            token_symbol: signal.token_symbol.clone(),
            trigger_dex: signal.platform,
            trigger_direction: signal.direction,
            sol_equivalent: signal.sol_equivalent,
            buy_dex,
            sell_dex,
        })
    }
}
