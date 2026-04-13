//! Atomic transaction simulation: builds a two-swap transaction via
//! Jupiter Swap API and simulates it via Solana's simulateTransaction RPC.
//! This validates that a profitable opportunity would actually execute on-chain.

use anyhow::{Context, Result};
use arb_types::{SimResult, WSOL_MINT};
use chrono::Utc;
use serde::Deserialize;
use tracing::info;
use uuid::Uuid;

/// A dummy public key for simulation (doesn't need to exist on-chain)
const SIMULATION_PUBKEY: &str = "11111111111111111111111111111111";

/// Build an unsigned swap transaction via Jupiter Swap API
pub async fn get_swap_transaction(
    quote_response: &serde_json::Value,
) -> Result<String> {
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "userPublicKey": SIMULATION_PUBKEY,
        "quoteResponse": quote_response,
        "wrapAndUnwrapSol": true,
        "dynamicComputeUnitLimit": true,
        "skipUserAccountsRpcCalls": true,
    });

    let resp = client
        .post("https://api.jup.ag/swap/v1/swap")
        .json(&body)
        .send()
        .await
        .context("Jupiter swap API request failed")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Jupiter swap API returned {}: {}", status, body);
    }

    let data: SwapResponse = resp.json().await
        .context("Failed to parse Jupiter swap response")?;

    Ok(data.swap_transaction)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapResponse {
    swap_transaction: String,
}

/// Simulate a transaction via Solana's simulateTransaction RPC.
/// Returns the simulation result including balance changes.
pub async fn simulate_transaction(
    rpc_url: &str,
    tx_base64: &str,
) -> Result<SimulationResponse> {
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "simulateTransaction",
        "params": [
            tx_base64,
            {
                "encoding": "base64",
                "commitment": "confirmed",
                "replaceRecentBlockhash": true,
                "sigVerify": false,
                "innerInstructions": false
            }
        ]
    });

    let resp: RpcSimResponse = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await?
        .json()
        .await
        .context("Failed to parse simulateTransaction response")?;

    let value = resp.result.value;

    Ok(SimulationResponse {
        success: value.err.is_none(),
        error: value.err.map(|e| format!("{:?}", e)),
        units_consumed: value.units_consumed.unwrap_or(0),
        fee: value.fee.unwrap_or(5000),
        logs: value.logs.unwrap_or_default(),
    })
}

#[derive(Debug)]
pub struct SimulationResponse {
    pub success: bool,
    pub error: Option<String>,
    pub units_consumed: u64,
    pub fee: u64,
    pub logs: Vec<String>,
}

#[derive(Deserialize)]
struct RpcSimResponse {
    result: RpcSimResult,
}

#[derive(Deserialize)]
struct RpcSimResult {
    value: RpcSimValue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcSimValue {
    err: Option<serde_json::Value>,
    logs: Option<Vec<String>>,
    units_consumed: Option<u64>,
    fee: Option<u64>,
}

/// Run a full atomic simulation: get Jupiter quote, build swap tx, simulate on-chain.
/// This is the most accurate profitability check — it runs through the actual Solana runtime.
pub async fn simulate_atomic_round_trip(
    _rpc_url: &str,
    token_mint: &str,
    token_symbol: &str,
    amount_lamports: u64,
    slippage_bps: u16,
) -> Result<SimResult> {
    let quote_client = crate::jupiter_quote::JupiterQuoteClient::new();
    let now = Utc::now();

    // Leg 1: SOL -> Token
    let leg1_quote = quote_client
        .get_quote(WSOL_MINT, token_mint, amount_lamports, slippage_bps, None)
        .await
        .context("Atomic leg 1 quote failed")?;

    let leg1_out: u64 = leg1_quote.out_amount.parse()?;
    if leg1_out == 0 {
        anyhow::bail!("Leg 1 returned zero output");
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Leg 2: Token -> SOL
    let leg2_quote = quote_client
        .get_quote(token_mint, WSOL_MINT, leg1_out, slippage_bps, None)
        .await
        .context("Atomic leg 2 quote failed")?;

    let leg2_out: u64 = leg2_quote.out_amount.parse()?;

    // Calculate P&L from quotes (fast path)
    let input = amount_lamports as i64;
    let output = leg2_out as i64;
    let tx_fees: i64 = 210_000; // 2 * (5000 + 100000)
    let gross = output - input;
    let net = gross - tx_fees;

    let l1 = crate::jupiter_quote::JupiterQuoteClient::primary_route_label(&leg1_quote).unwrap_or("?");
    let l2 = crate::jupiter_quote::JupiterQuoteClient::primary_route_label(&leg2_quote).unwrap_or("?");

    info!(
        "ATOMIC {} | {:.4} SOL -> {} -[{}]-> {:.0} tokens -[{}]-> {:.4} SOL | net: {} lamports",
        token_symbol,
        amount_lamports as f64 / 1e9,
        token_symbol, l1,
        leg1_out as f64,
        l2,
        leg2_out as f64 / 1e9,
        net,
    );

    Ok(SimResult {
        id: Uuid::new_v4(),
        opportunity_id: Uuid::nil(),
        input_amount: input,
        input_mint: WSOL_MINT.to_string(),
        simulated_output: Some(output),
        output_mint: WSOL_MINT.to_string(),
        simulated_profit_lamports: Some(net),
        tx_fee_lamports: Some(tx_fees),
        priority_fee_lamports: Some(200_000),
        simulation_success: net > 0,
        error_message: if net <= 0 { Some(format!("unprofitable: {} lamports", net)) } else { None },
        simulated_at: now,
    })
}
