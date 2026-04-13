//! WebSocket pool monitor: subscribes to on-chain account changes
//! via accountSubscribe and emits PriceQuotes through the shared channel.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use arb_types::{Dex, GraduationEvent, PriceQuote, PriceSource};
use base64::Engine;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::pool_decoder;
use crate::pool_discovery::{self, DiscoveredPool};

// ── Account Role Tracking ──

#[derive(Debug, Clone)]
enum AccountRole {
    RaydiumCoinVault {
        pool_address: String,
    },
    RaydiumPcVault {
        pool_address: String,
    },
    OrcaWhirlpool {
        pool_address: String,
        base_mint: String,
        decimals_a: u8,
        decimals_b: u8,
        base_is_a: bool,
        quote_mint: String,
    },
    PumpFunCurve {
        pool_address: String,
        base_mint: String,
    },
}

struct RaydiumVaultState {
    coin_balance: Option<u64>,
    pc_balance: Option<u64>,
    coin_decimals: u8,
    pc_decimals: u8,
    base_mint: String,
    pool_address: String,
    quote_mint: String,
}

pub struct PoolMonitor {
    ws_url: String,
    rpc_url: String,
    quote_tx: mpsc::Sender<PriceQuote>,
    graduation_tx: mpsc::Sender<GraduationEvent>,
    sol_usd_price: Arc<RwLock<f64>>,
}

impl PoolMonitor {
    pub fn new(
        helius_api_key: &str,
        quote_tx: mpsc::Sender<PriceQuote>,
        graduation_tx: mpsc::Sender<GraduationEvent>,
        sol_usd_price: Arc<RwLock<f64>>,
    ) -> Self {
        Self {
            ws_url: format!("wss://mainnet.helius-rpc.com/?api-key={helius_api_key}"),
            rpc_url: format!("https://mainnet.helius-rpc.com/?api-key={helius_api_key}"),
            quote_tx,
            graduation_tx,
            sol_usd_price,
        }
    }

    /// Main entry: discover pools, resolve accounts, connect WebSocket, process updates.
    pub async fn run(&self, watch_mints: Vec<String>, min_liquidity: f64) -> Result<()> {
        // 1. Discover pools
        let discovered = pool_discovery::discover_pools(&watch_mints, min_liquidity).await?;
        if discovered.is_empty() {
            warn!("No pools discovered, WebSocket monitor has nothing to subscribe to");
            return Ok(());
        }

        // 2. Build subscription map
        let (account_roles, mut raydium_states) = self.build_subscriptions(&discovered).await;

        if account_roles.is_empty() {
            warn!("No accounts resolved for subscription");
            return Ok(());
        }

        let subscribe_accounts: Vec<String> = account_roles.keys().cloned().collect();
        info!("WebSocket: subscribing to {} accounts", subscribe_accounts.len());

        // 3. PumpFun graduation tracking
        let mut pumpfun_was_complete: HashMap<String, bool> = HashMap::new();
        for pool in &discovered {
            if pool.dex == Dex::PumpFun {
                pumpfun_was_complete.insert(pool.pool_address.clone(), false);
            }
        }

        // 4. Connect with reconnect loop
        loop {
            match self.run_session(&subscribe_accounts, &account_roles, &mut raydium_states, &mut pumpfun_was_complete).await {
                Ok(()) => break,
                Err(e) => {
                    error!("WebSocket error: {}. Reconnecting in 3s...", e);
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            }
        }

        Ok(())
    }

    async fn build_subscriptions(&self, discovered: &[DiscoveredPool]) -> (HashMap<String, AccountRole>, HashMap<String, RaydiumVaultState>) {
        let mut roles: HashMap<String, AccountRole> = HashMap::new();
        let mut raydium_states: HashMap<String, RaydiumVaultState> = HashMap::new();

        for pool in discovered {
            // Resolve decimals
            let base_dec = pool_discovery::resolve_token_decimals(&self.rpc_url, &pool.base_mint)
                .await.unwrap_or(6);
            let quote_dec = if !pool.quote_mint.is_empty() {
                pool_discovery::resolve_token_decimals(&self.rpc_url, &pool.quote_mint)
                    .await.unwrap_or(9)
            } else {
                9 // default to SOL decimals
            };

            match pool.dex {
                Dex::Raydium => {
                    match pool_discovery::resolve_raydium_vaults(&self.rpc_url, &pool.pool_address).await {
                        Ok(vaults) => {
                            roles.insert(vaults.coin_vault.clone(), AccountRole::RaydiumCoinVault {
                                pool_address: pool.pool_address.clone(),
                            });
                            roles.insert(vaults.pc_vault.clone(), AccountRole::RaydiumPcVault {
                                pool_address: pool.pool_address.clone(),
                            });
                            raydium_states.insert(pool.pool_address.clone(), RaydiumVaultState {
                                coin_balance: None,
                                pc_balance: None,
                                coin_decimals: base_dec,
                                pc_decimals: quote_dec,
                                base_mint: pool.base_mint.clone(),
                                pool_address: pool.pool_address.clone(),
                                quote_mint: pool.quote_mint.clone(),
                            });
                        }
                        Err(e) => warn!("Raydium vault resolve failed for {}: {}", &pool.pool_address[..8], e),
                    }
                }
                Dex::Orca => {
                    // Read the pool account to determine mint_a (on-chain token ordering)
                    match pool_discovery::fetch_account_data_pub(&self.rpc_url, &pool.pool_address).await {
                        Ok(data) => {
                            match pool_decoder::decode_whirlpool_mint_a(&data) {
                                Ok(mint_a_bytes) => {
                                    let mint_a_b58 = bs58::encode(&mint_a_bytes).into_string();
                                    let base_is_a = mint_a_b58 == pool.base_mint;
                                    let (dec_a, dec_b) = if base_is_a {
                                        (base_dec, quote_dec)
                                    } else {
                                        (quote_dec, base_dec)
                                    };
                                    info!("Orca {} mint_a={}, base_is_a={}", &pool.pool_address[..8], &mint_a_b58[..8], base_is_a);
                                    roles.insert(pool.pool_address.clone(), AccountRole::OrcaWhirlpool {
                                        pool_address: pool.pool_address.clone(),
                                        base_mint: pool.base_mint.clone(),
                                        decimals_a: dec_a,
                                        decimals_b: dec_b,
                                        base_is_a,
                                        quote_mint: pool.quote_mint.clone(),
                                    });
                                }
                                Err(e) => warn!("Orca mint_a decode failed for {}: {}", &pool.pool_address[..8], e),
                            }
                        }
                        Err(e) => warn!("Orca account fetch failed for {}: {}", &pool.pool_address[..8], e),
                    }
                }
                Dex::Meteora => {
                    // Meteora pool addresses from DexScreener don't map to on-chain accounts.
                    // Meteora pools are monitored via HTTP polling only.
                }
                Dex::PumpFun => {
                    roles.insert(pool.pool_address.clone(), AccountRole::PumpFunCurve {
                        pool_address: pool.pool_address.clone(),
                        base_mint: pool.base_mint.clone(),
                    });
                }
                _ => {}
            }
        }

        (roles, raydium_states)
    }

    async fn run_session(
        &self,
        accounts: &[String],
        roles: &HashMap<String, AccountRole>,
        raydium_states: &mut HashMap<String, RaydiumVaultState>,
        pumpfun_was_complete: &mut HashMap<String, bool>,
    ) -> Result<()> {
        let (ws_stream, _) = connect_async(&self.ws_url)
            .await
            .context("WebSocket connect failed")?;

        let (mut write, mut read) = ws_stream.split();
        info!("WebSocket connected");

        // Track subscription IDs
        let mut req_to_account: HashMap<u64, String> = HashMap::new();
        let mut sub_to_account: HashMap<u64, String> = HashMap::new();

        // Subscribe to all accounts
        for (i, account) in accounts.iter().enumerate() {
            let req_id = (i + 1) as u64;
            let msg = serde_json::json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "accountSubscribe",
                "params": [account, {"encoding": "base64", "commitment": "confirmed"}]
            });
            req_to_account.insert(req_id, account.clone());
            write.send(Message::Text(msg.to_string())).await?;
        }

        info!("Sent {} accountSubscribe requests", accounts.len());

        // Process messages
        while let Some(msg) = read.next().await {
            let msg = msg.context("WebSocket read error")?;
            match msg {
                Message::Text(text) => {
                    self.handle_message(
                        &text,
                        roles,
                        raydium_states,
                        pumpfun_was_complete,
                        &mut sub_to_account,
                        &req_to_account,
                    ).await;
                }
                Message::Ping(data) => { write.send(Message::Pong(data)).await.ok(); }
                Message::Close(_) => {
                    warn!("WebSocket closed by server");
                    return Ok(());
                }
                _ => {}
            }
        }

        Ok(())
    }

    async fn handle_message(
        &self,
        text: &str,
        roles: &HashMap<String, AccountRole>,
        raydium_states: &mut HashMap<String, RaydiumVaultState>,
        pumpfun_was_complete: &mut HashMap<String, bool>,
        sub_to_account: &mut HashMap<u64, String>,
        req_to_account: &HashMap<u64, String>,
    ) {
        let msg: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };

        // Handle subscription confirmation: {"id": N, "result": sub_id}
        if let (Some(req_id), Some(sub_id)) = (
            msg.get("id").and_then(|v| v.as_u64()),
            msg.get("result").and_then(|v| v.as_u64()),
        ) {
            if let Some(account) = req_to_account.get(&req_id) {
                sub_to_account.insert(sub_id, account.clone());
            }
            return;
        }

        // Handle account notification
        if msg.get("method").and_then(|v| v.as_str()) != Some("accountNotification") {
            return;
        }

        let params = match msg.get("params") {
            Some(p) => p,
            None => return,
        };

        let sub_id = match params.get("subscription").and_then(|v| v.as_u64()) {
            Some(id) => id,
            None => return,
        };

        let account_pk = match sub_to_account.get(&sub_id) {
            Some(pk) => pk.clone(),
            None => return,
        };

        // Decode base64 account data
        let data_b64 = match params.pointer("/result/value/data/0").and_then(|v| v.as_str()) {
            Some(d) => d,
            None => return,
        };

        let data = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
            Ok(d) => d,
            Err(_) => return,
        };

        let role = match roles.get(&account_pk) {
            Some(r) => r.clone(),
            None => return,
        };

        let now = Utc::now();
        let sol_usd = *self.sol_usd_price.read().await;

        match role {
            AccountRole::RaydiumCoinVault { pool_address, .. } => {
                if let Ok(balance) = pool_decoder::decode_spl_token_balance(&data) {
                    if let Some(state) = raydium_states.get_mut(&pool_address) {
                        state.coin_balance = Some(balance);
                        self.try_emit_raydium(state, sol_usd, now).await;
                    }
                }
            }
            AccountRole::RaydiumPcVault { pool_address, .. } => {
                if let Ok(balance) = pool_decoder::decode_spl_token_balance(&data) {
                    if let Some(state) = raydium_states.get_mut(&pool_address) {
                        state.pc_balance = Some(balance);
                        self.try_emit_raydium(state, sol_usd, now).await;
                    }
                }
            }
            AccountRole::OrcaWhirlpool { pool_address, base_mint, decimals_a, decimals_b, base_is_a, quote_mint } => {
                if let Ok(mut price) = pool_decoder::decode_whirlpool_price(&data, decimals_a, decimals_b) {
                    if !base_is_a && price > 0.0 {
                        price = 1.0 / price;
                    }
                    let price_usd = if pool_discovery::is_usd_quoted(&quote_mint) { price } else { price * sol_usd };
                    self.emit_quote(Dex::Orca, &base_mint, &pool_address, price_usd, now).await;
                }
            }
            AccountRole::PumpFunCurve { pool_address, base_mint } => {
                if let Ok(state) = pool_decoder::decode_pumpfun_state(&data) {
                    // Graduation detection
                    if let Some(was_complete) = pumpfun_was_complete.get_mut(&pool_address) {
                        if state.complete && !*was_complete {
                            info!("GRADUATION: PumpFun curve {} graduated!", &pool_address[..8]);
                            let event = GraduationEvent {
                                bonding_curve_address: pool_address.clone(),
                                base_mint: base_mint.clone(),
                                token_symbol: String::new(),
                                graduated_at: now,
                            };
                            self.graduation_tx.send(event).await.ok();
                        }
                        *was_complete = state.complete;
                    }

                    // Emit price if not graduated
                    if !state.complete {
                        if let Ok(price_sol) = pool_decoder::pumpfun_price_in_sol(&state) {
                            let price_usd = price_sol * sol_usd;
                            self.emit_quote(Dex::PumpFun, &base_mint, &pool_address, price_usd, now).await;
                        }
                    }
                }
            }
        }
    }

    async fn try_emit_raydium(&self, state: &RaydiumVaultState, sol_usd: f64, now: chrono::DateTime<Utc>) {
        if let (Some(coin), Some(pc)) = (state.coin_balance, state.pc_balance) {
            if let Ok(price) = pool_decoder::raydium_price_from_vaults(coin, pc, state.coin_decimals, state.pc_decimals) {
                let price_usd = if pool_discovery::is_usd_quoted(&state.quote_mint) { price } else { price * sol_usd };
                self.emit_quote(Dex::Raydium, &state.base_mint, &state.pool_address, price_usd, now).await;
            }
        }
    }

    async fn emit_quote(&self, dex: Dex, base_mint: &str, pool_address: &str, price_usd: f64, timestamp: chrono::DateTime<Utc>) {
        let quote = PriceQuote {
            dex,
            base_mint: base_mint.to_string(),
            quote_mint: String::new(),
            price_usd,
            liquidity_usd: 0.0,
            pool_address: Some(pool_address.to_string()),
            source: PriceSource::WebSocket,
            timestamp,
        };
        self.quote_tx.send(quote).await.ok();
    }
}
