//! Jito Block Engine client for atomic bundle submission.
//! Bundles are groups of transactions that execute all-or-nothing.
//! Tips incentivize validators to include the bundle.

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_sdk::{
    instruction::AccountMeta,
    pubkey::Pubkey,
    transaction::Transaction,
};
use tracing::{debug, info, warn};

/// Jito Block Engine REST API endpoints.
const JITO_MAINNET_URL: &str = "https://mainnet.block-engine.jito.wtf";

/// 8 hardcoded Jito tip accounts. Tip goes to one of these in the last tx of the bundle.
const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4bVqkfRtQ7NmXwkiK3AB4Yf",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSLnS6yB6pAVbTAnUKR",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

/// Result of a bundle submission.
#[derive(Debug, Clone)]
pub struct BundleResult {
    pub bundle_id: String,
    pub status: BundleStatus,
    pub tip_lamports: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BundleStatus {
    Submitted,
    Confirmed,
    Failed(String),
    Simulated,
}

/// Jito bundle submission client.
#[derive(Clone)]
pub struct JitoBundler {
    client: Client,
    block_engine_url: String,
}

#[derive(Serialize)]
struct SendBundleRequest {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: Vec<Vec<String>>,
}

#[derive(Deserialize)]
struct SendBundleResponse {
    result: Option<String>,
    error: Option<JitoError>,
}

#[derive(Deserialize)]
struct JitoError {
    message: String,
}

impl JitoBundler {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            block_engine_url: JITO_MAINNET_URL.to_string(),
        }
    }

    /// Select a random Jito tip account.
    pub fn random_tip_account() -> Pubkey {
        use std::time::{SystemTime, UNIX_EPOCH};
        let idx = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as usize
            % JITO_TIP_ACCOUNTS.len();
        JITO_TIP_ACCOUNTS[idx].parse().unwrap()
    }

    /// Calculate tip amount based on expected profit.
    /// Tip = max(min_tip, profit * tip_fraction).
    pub fn calculate_tip(expected_profit_lamports: i64) -> u64 {
        const MIN_TIP: u64 = 10_000; // 0.00001 SOL minimum
        const TIP_FRACTION: f64 = 0.5; // give 50% of profit as tip

        if expected_profit_lamports <= 0 {
            return MIN_TIP;
        }

        let tip = (expected_profit_lamports as f64 * TIP_FRACTION) as u64;
        tip.max(MIN_TIP)
    }

    /// Build a SOL transfer instruction (tip) from payer to a Jito tip account.
    pub fn build_tip_instruction(from: &Pubkey, tip_lamports: u64) -> solana_sdk::instruction::Instruction {
        let tip_account = Self::random_tip_account();
        // System program transfer: instruction index 2, data = u32(2) + u64(lamports)
        let system_program_id: Pubkey = "11111111111111111111111111111111".parse().unwrap();
        let mut data = Vec::with_capacity(12);
        data.extend_from_slice(&2u32.to_le_bytes()); // Transfer instruction index
        data.extend_from_slice(&tip_lamports.to_le_bytes());

        solana_sdk::instruction::Instruction {
            program_id: system_program_id,
            accounts: vec![
                AccountMeta::new(*from, true),          // from (signer, writable)
                AccountMeta::new(tip_account, false),    // to (writable)
            ],
            data,
        }
    }

    /// Submit a bundle of transactions to the Jito Block Engine.
    /// Transactions must be signed and base58-encoded.
    pub async fn submit_bundle(&self, signed_txs: &[Transaction]) -> Result<BundleResult> {
        let encoded: Vec<String> = signed_txs
            .iter()
            .map(|tx| bs58::encode(bincode::serialize(tx).unwrap()).into_string())
            .collect();

        let request = SendBundleRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "sendBundle",
            params: vec![encoded],
        };

        let url = format!("{}/api/v1/bundles", self.block_engine_url);
        debug!("Submitting bundle with {} txs to {}", signed_txs.len(), url);

        let resp: SendBundleResponse = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await?
            .json()
            .await?;

        if let Some(err) = resp.error {
            warn!("Jito bundle error: {}", err.message);
            return Ok(BundleResult {
                bundle_id: String::new(),
                status: BundleStatus::Failed(err.message),
                tip_lamports: 0,
            });
        }

        let bundle_id = resp.result.unwrap_or_default();
        info!("Bundle submitted: {}", bundle_id);

        Ok(BundleResult {
            bundle_id,
            status: BundleStatus::Submitted,
            tip_lamports: 0,
        })
    }

    /// Simulate a transaction via RPC (does not submit to network).
    pub async fn simulate_transaction(
        rpc_url: &str,
        tx: &Transaction,
    ) -> Result<SimulationResult> {
        let client = Client::new();
        let encoded = bs58::encode(bincode::serialize(tx)?).into_string();

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "simulateTransaction",
            "params": [
                encoded,
                {
                    "encoding": "base58",
                    "commitment": "processed",
                    "replaceRecentBlockhash": true
                }
            ]
        });

        let resp: serde_json::Value = client
            .post(rpc_url)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        let result = resp.get("result").ok_or_else(|| anyhow::anyhow!("No result in simulation response"))?;
        let err = result.get("value").and_then(|v| v.get("err"));

        if let Some(err) = err {
            if !err.is_null() {
                return Ok(SimulationResult {
                    success: false,
                    error: Some(format!("{}", err)),
                    units_consumed: result
                        .get("value")
                        .and_then(|v| v.get("unitsConsumed"))
                        .and_then(|u| u.as_u64()),
                    logs: extract_logs(result),
                });
            }
        }

        Ok(SimulationResult {
            success: true,
            error: None,
            units_consumed: result
                .get("value")
                .and_then(|v| v.get("unitsConsumed"))
                .and_then(|u| u.as_u64()),
            logs: extract_logs(result),
        })
    }
}

/// Result of a transaction simulation.
#[derive(Debug, Clone)]
pub struct SimulationResult {
    pub success: bool,
    pub error: Option<String>,
    pub units_consumed: Option<u64>,
    pub logs: Vec<String>,
}

fn extract_logs(result: &serde_json::Value) -> Vec<String> {
    result
        .get("value")
        .and_then(|v| v.get("logs"))
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}
