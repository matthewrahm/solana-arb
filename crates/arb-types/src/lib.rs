use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Token Constants ──

pub const BONK_MINT: &str = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";
pub const WIF_MINT: &str = "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm";
pub const POPCAT_MINT: &str = "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr";
pub const MEW_MINT: &str = "MEW1gQWJ3nEXg2qgERiKu7FAFj79PHvQVREQUzScPP5";
pub const FARTCOIN_MINT: &str = "9BB6NFEcjBCtnNLFko2FqVQBq8HHM13kCyYcdQbgpump";
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const USDT_MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

/// Default watchlist of volatile Solana memecoins with good volume
pub fn default_watch_mints() -> Vec<(&'static str, &'static str)> {
    vec![
        (BONK_MINT, "BONK"),
        (WIF_MINT, "WIF"),
        (POPCAT_MINT, "POPCAT"),
        (MEW_MINT, "MEW"),
        (FARTCOIN_MINT, "FARTCOIN"),
    ]
}

/// Stablecoins used as intermediaries in triangular arb
pub fn stablecoin_mints() -> Vec<&'static str> {
    vec![USDC_MINT, USDT_MINT]
}

// ── DEX Program IDs ──

pub const RAYDIUM_AMM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
pub const JUPITER_V6: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
pub const PUMPFUN: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const ORCA_WHIRLPOOL: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
pub const METEORA_DAMM_V2: &str = "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG";

// ── Default Fees (bps) ──

/// Typical swap fee per leg in basis points
pub const DEFAULT_SWAP_FEE_BPS: f64 = 25.0;

// ── Enums ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dex {
    Raydium,
    Orca,
    Jupiter,
    Meteora,
    PumpFun,
    PumpSwap,
    Unknown,
}

impl Dex {
    pub fn as_str(&self) -> &'static str {
        match self {
            Dex::Raydium => "raydium",
            Dex::Orca => "orca",
            Dex::Jupiter => "jupiter",
            Dex::Meteora => "meteora",
            Dex::PumpFun => "pumpfun",
            Dex::PumpSwap => "pumpswap",
            Dex::Unknown => "unknown",
        }
    }

    /// Parse from DexScreener's `dexId` field
    pub fn from_dexscreener_id(id: &str) -> Self {
        match id {
            "raydium" => Dex::Raydium,
            "orca" => Dex::Orca,
            "jupiter" => Dex::Jupiter,
            "meteora" => Dex::Meteora,
            "pumpfun" => Dex::PumpFun,
            "pumpswap" => Dex::PumpSwap,
            _ => Dex::Unknown,
        }
    }

    /// Estimated swap fee for this DEX in basis points
    pub fn fee_bps(&self) -> f64 {
        match self {
            Dex::Raydium => 25.0,
            Dex::Orca => 25.0,
            Dex::Meteora => 30.0,
            Dex::PumpFun => 100.0,  // 1% fee
            Dex::PumpSwap => 25.0,  // 0.25% fee (standard AMM)
            _ => DEFAULT_SWAP_FEE_BPS,
        }
    }
}

impl std::fmt::Display for Dex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceSource {
    HttpPoll,
    WebSocket,
    ForgeStream,
}

// ── Forge Integration Types ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwapDirection {
    Buy,
    Sell,
}

impl std::fmt::Display for SwapDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwapDirection::Buy => write!(f, "BUY"),
            SwapDirection::Sell => write!(f, "SELL"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapSignal {
    pub signature: String,
    pub slot: u64,
    pub platform: Dex,
    pub signer: String,
    pub token_mint: String,
    pub token_symbol: Option<String>,
    pub direction: SwapDirection,
    pub sol_equivalent: f64,
    pub timestamp: DateTime<Utc>,
}

impl Dex {
    /// Map from forge's platform string to our Dex enum
    pub fn from_forge_platform(platform: &str) -> Self {
        match platform {
            "raydium" => Dex::Raydium,
            "jupiter" => Dex::Jupiter,
            "pumpfun" => Dex::PumpFun,
            "pumpswap" => Dex::PumpSwap,
            _ => Dex::Unknown,
        }
    }
}

// ── Core Data Types ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceQuote {
    pub dex: Dex,
    pub base_mint: String,
    pub quote_mint: String,
    pub price_usd: f64,
    pub liquidity_usd: f64,
    pub pool_address: Option<String>,
    pub source: PriceSource,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbOpportunity {
    pub id: Uuid,
    pub base_mint: String,
    pub token_symbol: String,
    pub buy_dex: Dex,
    pub buy_price: f64,
    pub buy_pool: Option<String>,
    pub sell_dex: Dex,
    pub sell_price: f64,
    pub sell_pool: Option<String>,
    pub gross_spread_bps: f64,
    pub estimated_fees_bps: f64,
    pub net_spread_bps: f64,
    pub estimated_profit_usd: f64,
    pub detected_at: DateTime<Utc>,
    pub detection_latency_ms: u64,
}

// ── WebSocket Pool Types ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraduationEvent {
    pub bonding_curve_address: String,
    pub base_mint: String,
    pub token_symbol: String,
    pub graduated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct RaydiumVaults {
    pub coin_vault: String,
    pub pc_vault: String,
}

// ── Simulation Result ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimResult {
    pub id: Uuid,
    pub opportunity_id: Uuid,
    pub input_amount: i64,
    pub input_mint: String,
    pub simulated_output: Option<i64>,
    pub output_mint: String,
    pub simulated_profit_lamports: Option<i64>,
    pub tx_fee_lamports: Option<i64>,
    pub priority_fee_lamports: Option<i64>,
    pub simulation_success: bool,
    pub error_message: Option<String>,
    pub simulated_at: DateTime<Utc>,
}

// ── Pool Registry Types ──

/// A known AMM pool for a token pair, used for local price computation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownPool {
    pub pool_address: String,
    pub dex: Dex,
    pub base_mint: String,
    pub quote_mint: String,
    /// Raydium: (coin_vault, pc_vault). PumpSwap: (token_vault, sol_vault).
    /// PumpFun: pool_address IS the bonding curve, no separate vaults needed.
    pub vault_addresses: Option<(String, String)>,
}

/// Result of a local AMM quote (no Jupiter involved).
#[derive(Debug, Clone)]
pub struct LocalQuote {
    pub pool_address: String,
    pub dex: Dex,
    pub input_mint: String,
    pub input_amount: u64,
    pub output_amount: u64,
    pub fee_amount: u64,
    pub price_impact_bps: f64,
    pub reserves: (u64, u64),
}

/// Result of a cross-venue arbitrage scan using local AMM math.
#[derive(Debug, Clone)]
pub struct CrossVenueResult {
    pub token_mint: String,
    pub token_symbol: String,
    pub buy_pool: KnownPool,
    pub sell_pool: KnownPool,
    pub input_lamports: u64,
    pub tokens_received: u64,
    pub output_lamports: u64,
    pub gross_profit_lamports: i64,
    pub net_profit_lamports: i64,
    pub profit_bps: f64,
    pub profitable: bool,
}

// ── Execution Types ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Paper,
    Simulate,
    Live,
}

impl std::fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutionMode::Paper => write!(f, "paper"),
            ExecutionMode::Simulate => write!(f, "simulate"),
            ExecutionMode::Live => write!(f, "live"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    CrossVenueArb,
    GraduationSnipe,
    BackRun,
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Strategy::CrossVenueArb => write!(f, "cross_venue_arb"),
            Strategy::GraduationSnipe => write!(f, "graduation_snipe"),
            Strategy::BackRun => write!(f, "back_run"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub id: Uuid,
    pub strategy: Strategy,
    pub mode: ExecutionMode,
    pub token_mint: String,
    pub token_symbol: String,
    pub buy_dex: Option<String>,
    pub sell_dex: Option<String>,
    pub input_lamports: i64,
    pub expected_output_lamports: Option<i64>,
    pub actual_output_lamports: Option<i64>,
    pub expected_profit_lamports: Option<i64>,
    pub actual_profit_lamports: Option<i64>,
    pub tip_lamports: Option<i64>,
    pub tx_signature: Option<String>,
    pub bundle_id: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
    pub simulation_units: Option<i64>,
    pub executed_at: DateTime<Utc>,
}

// ── Configuration ──

#[derive(Debug, Clone)]
pub struct Config {
    pub helius_api_key: String,
    pub database_url: String,
    pub api_port: u16,
    pub poll_interval_secs: u64,
    /// Minimum net spread in bps to flag an opportunity
    pub min_spread_bps: f64,
    /// Tokens to monitor (mint addresses)
    pub watch_mints: Vec<String>,
}
