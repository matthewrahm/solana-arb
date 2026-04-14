use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tracing::{error, info, warn};

use arb_types::{Dex, SwapDirection, SwapSignal, USDC_MINT, USDT_MINT, WSOL_MINT};

// ── Forge Event Types (local mirror, keeps repos independent) ──

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "PascalCase")]
enum ForgeEvent {
    Swap(ForgeSwapEvent),
    #[allow(dead_code)]
    Transfer(serde_json::Value),
}

// Fallback: forge serializes ParsedEvent as { "Swap": { ... } }
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ForgeEventRaw {
    Tagged(ForgeEvent),
    SwapDirect {
        #[serde(rename = "Swap")]
        swap: ForgeSwapEvent,
    },
    #[allow(dead_code)]
    TransferDirect {
        #[serde(rename = "Transfer")]
        transfer: serde_json::Value,
    },
}

#[derive(Debug, Deserialize)]
pub struct ForgeSwapEvent {
    pub signature: String,
    pub slot: u64,
    pub block_time: DateTime<Utc>,
    pub fee_lamports: u64,
    pub fee_payer: String,
    pub platform: ForgePlatform,
    pub signer: String,
    pub token_in: ForgeTokenAmount,
    pub token_out: ForgeTokenAmount,
    pub pool_address: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum ForgePlatform {
    Raydium,
    Jupiter,
    #[serde(rename = "pumpfun")]
    PumpFun,
    #[serde(rename = "pumpswap")]
    PumpSwap,
    Unknown,
}

impl ForgePlatform {
    fn to_dex(self) -> Dex {
        match self {
            ForgePlatform::Raydium => Dex::Raydium,
            ForgePlatform::Jupiter => Dex::Jupiter,
            ForgePlatform::PumpFun => Dex::PumpFun,
            ForgePlatform::PumpSwap => Dex::PumpSwap,
            ForgePlatform::Unknown => Dex::Unknown,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ForgeTokenAmount {
    pub mint: String,
    pub amount: u64,
    pub decimals: u8,
}

// ── Forge Feed Consumer ──

const MIN_SOL_EQUIVALENT: f64 = 1.0; // signal swaps > 1 SOL (micro-cap sensitive)
const DEBOUNCE_SECS: f64 = 3.0; // slightly longer debounce to conserve API budget

fn is_sol_or_stable(mint: &str) -> bool {
    mint == WSOL_MINT || mint == USDC_MINT || mint == USDT_MINT
}

fn classify_swap(swap: &ForgeSwapEvent) -> Option<(String, SwapDirection, f64)> {
    let in_is_sol = is_sol_or_stable(&swap.token_in.mint);
    let out_is_sol = is_sol_or_stable(&swap.token_out.mint);

    match (in_is_sol, out_is_sol) {
        // SOL/stable in, token out = BUY
        (true, false) => {
            let sol_equiv = if swap.token_in.mint == WSOL_MINT {
                swap.token_in.amount as f64 / 10f64.powi(swap.token_in.decimals as i32)
            } else {
                // stablecoin -- rough estimate: assume 1 USDC ~= 0.006 SOL (just use raw amount)
                swap.token_in.amount as f64 / 10f64.powi(swap.token_in.decimals as i32) / 150.0
            };
            Some((swap.token_out.mint.clone(), SwapDirection::Buy, sol_equiv))
        }
        // Token in, SOL/stable out = SELL
        (false, true) => {
            let sol_equiv = if swap.token_out.mint == WSOL_MINT {
                swap.token_out.amount as f64 / 10f64.powi(swap.token_out.decimals as i32)
            } else {
                swap.token_out.amount as f64 / 10f64.powi(swap.token_out.decimals as i32) / 150.0
            };
            Some((swap.token_in.mint.clone(), SwapDirection::Sell, sol_equiv))
        }
        // Token-to-token swap (no clear SOL side), skip
        _ => None,
    }
}

pub async fn run_forge_feed(
    forge_url: &str,
    signal_tx: mpsc::Sender<SwapSignal>,
) -> Result<()> {
    let mut debounce: HashMap<String, Instant> = HashMap::new();

    info!("Connecting to forge WebSocket at {}", forge_url);
    let (ws_stream, _) = connect_async(forge_url).await?;
    info!("Connected to forge stream");

    let (_, mut read) = ws_stream.split();

    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                error!("Forge WebSocket error: {}", e);
                break;
            }
        };

        let text = match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
            tokio_tungstenite::tungstenite::Message::Close(_) => {
                info!("Forge WebSocket closed by server");
                break;
            }
            _ => continue,
        };

        // Parse the forge event
        let swap = match serde_json::from_str::<ForgeEventRaw>(&text) {
            Ok(ForgeEventRaw::Tagged(ForgeEvent::Swap(s))) => s,
            Ok(ForgeEventRaw::SwapDirect { swap: s }) => s,
            Ok(_) => continue, // transfer or other, skip
            Err(_) => continue,
        };

        // Classify: which token, buy or sell, how much SOL?
        let (token_mint, direction, sol_equivalent) = match classify_swap(&swap) {
            Some(c) => c,
            None => continue,
        };

        // Filter by size
        if sol_equivalent < MIN_SOL_EQUIVALENT {
            continue;
        }

        // Debounce: skip if we signaled this token recently
        let now = Instant::now();
        if let Some(last) = debounce.get(&token_mint) {
            if now.duration_since(*last).as_secs_f64() < DEBOUNCE_SECS {
                continue;
            }
        }
        debounce.insert(token_mint.clone(), now);

        // Clean old debounce entries periodically
        if debounce.len() > 500 {
            debounce.retain(|_, v| now.duration_since(*v).as_secs_f64() < 30.0);
        }

        let signal = SwapSignal {
            signature: swap.signature.clone(),
            slot: swap.slot,
            platform: swap.platform.to_dex(),
            signer: swap.signer.clone(),
            token_mint,
            token_symbol: None,
            direction,
            sol_equivalent,
            timestamp: swap.block_time,
        };

        if signal_tx.send(signal).await.is_err() {
            warn!("Signal channel closed");
            break;
        }
    }

    Ok(())
}
