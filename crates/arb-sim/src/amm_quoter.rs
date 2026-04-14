//! Local AMM quoter: fetches on-chain pool state and computes swap outputs
//! using constant-product math. No Jupiter dependency.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Result};
use arb_feed::pool_decoder;
use arb_types::{Dex, KnownPool, LocalQuote, WSOL_MINT};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Cached pool state for fast lookups without RPC calls.
#[derive(Debug, Clone)]
pub struct CachedPoolState {
    pub dex: Dex,
    pub reserve_in: u64,  // SOL/quote side
    pub reserve_out: u64, // token/base side
    pub base_mint: String,
    pub quote_mint: String,
}

/// Pool registry: maps token_mint -> list of known pools.
pub type PoolRegistry = Arc<RwLock<HashMap<String, Vec<KnownPool>>>>;

/// Create an empty pool registry.
pub fn new_pool_registry() -> PoolRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Local AMM quoter that fetches pool state via RPC and computes outputs.
#[derive(Clone)]
pub struct AmmQuoter {
    rpc_url: String,
    client: Client,
}

// ── RPC Response Types ──

#[derive(Debug, Deserialize)]
struct RpcResponse<T> {
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct GetMultipleAccountsResult {
    value: Vec<Option<AccountInfo>>,
}

#[derive(Debug, Deserialize)]
struct AccountInfo {
    data: (String, String), // (base64_data, "base64")
}

impl AmmQuoter {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
            client: Client::new(),
        }
    }

    /// Fetch multiple accounts in a single RPC call. Returns decoded bytes keyed by address.
    async fn get_multiple_accounts(&self, addresses: &[&str]) -> Result<HashMap<String, Vec<u8>>> {
        if addresses.is_empty() {
            return Ok(HashMap::new());
        }

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getMultipleAccounts",
            "params": [
                addresses,
                { "encoding": "base64" }
            ]
        });

        let resp: RpcResponse<GetMultipleAccountsResult> = self
            .client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        let result = resp.result.ok_or_else(|| anyhow::anyhow!("RPC returned null result"))?;
        let mut accounts = HashMap::new();

        for (i, account) in result.value.into_iter().enumerate() {
            if let Some(info) = account {
                use base64::Engine;
                let data = base64::engine::general_purpose::STANDARD.decode(&info.data.0)?;
                accounts.insert(addresses[i].to_string(), data);
            }
        }

        Ok(accounts)
    }

    /// Quote a swap on a specific pool using local AMM math.
    /// Fetches current reserves from chain, computes output amount.
    pub async fn quote_swap(
        &self,
        pool: &KnownPool,
        input_mint: &str,
        amount_in: u64,
    ) -> Result<LocalQuote> {
        match pool.dex {
            Dex::Raydium => self.quote_raydium(pool, input_mint, amount_in).await,
            Dex::PumpSwap => self.quote_pumpswap(pool, input_mint, amount_in).await,
            Dex::PumpFun => self.quote_pumpfun(pool, input_mint, amount_in).await,
            Dex::RaydiumClmm => self.quote_raydium_clmm(pool, input_mint, amount_in).await,
            Dex::Orca => self.quote_orca(pool, input_mint, amount_in).await,
            other => bail!("Local quoting not supported for {:?}", other),
        }
    }

    /// Quote a Raydium V4 swap by fetching pool + vault accounts.
    async fn quote_raydium(
        &self,
        pool: &KnownPool,
        input_mint: &str,
        amount_in: u64,
    ) -> Result<LocalQuote> {
        let (coin_vault, pc_vault) = pool
            .vault_addresses
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Raydium pool missing vault addresses"))?;

        // Fetch both vault balances in one RPC call
        let accounts = self
            .get_multiple_accounts(&[coin_vault.as_str(), pc_vault.as_str()])
            .await?;

        let coin_data = accounts
            .get(coin_vault)
            .ok_or_else(|| anyhow::anyhow!("Failed to fetch coin vault"))?;
        let pc_data = accounts
            .get(pc_vault)
            .ok_or_else(|| anyhow::anyhow!("Failed to fetch pc vault"))?;

        let coin_balance = pool_decoder::decode_spl_token_balance(coin_data)?;
        let pc_balance = pool_decoder::decode_spl_token_balance(pc_data)?;

        // Determine which side is input vs output
        // Raydium: coin = base token, pc = quote (SOL/USDC)
        let (reserve_in, reserve_out) = if input_mint == pool.quote_mint || input_mint == WSOL_MINT {
            // Buying token: SOL/quote in, token out
            (pc_balance, coin_balance)
        } else {
            // Selling token: token in, SOL/quote out
            (coin_balance, pc_balance)
        };

        let output = pool_decoder::raydium_swap_output(reserve_in, reserve_out, amount_in);
        let fee = amount_in as u64 * 25 / 10_000;
        let impact_bps = if reserve_in > 0 {
            (amount_in as f64 / reserve_in as f64) * 10_000.0
        } else {
            0.0
        };

        Ok(LocalQuote {
            pool_address: pool.pool_address.clone(),
            dex: Dex::Raydium,
            input_mint: input_mint.to_string(),
            input_amount: amount_in,
            output_amount: output,
            fee_amount: fee,
            price_impact_bps: impact_bps,
            reserves: (reserve_in, reserve_out),
        })
    }

    /// Quote a Raydium CLMM swap using sqrt_price approximation.
    /// The pool account contains vaults, sqrt_price, liquidity, and mint info.
    async fn quote_raydium_clmm(
        &self,
        pool: &KnownPool,
        input_mint: &str,
        amount_in: u64,
    ) -> Result<LocalQuote> {
        let accounts = self
            .get_multiple_accounts(&[pool.pool_address.as_str()])
            .await?;

        let pool_data = accounts
            .get(&pool.pool_address)
            .ok_or_else(|| anyhow::anyhow!("Failed to fetch CLMM pool account"))?;

        let state = pool_decoder::decode_raydium_clmm(pool_data)?;

        let mint_0_str = bs58::encode(state.mint_0).into_string();
        let is_0_to_1 = input_mint == mint_0_str;

        // Default fee 25 bps (actual varies by amm_config, but we'd need another RPC call)
        let fee_bps = pool.dex.fee_bps();

        let output = pool_decoder::clmm_swap_output(&state, amount_in, fee_bps, is_0_to_1);

        debug!(
            "CLMM LOCAL: pool={} sqrt_price={} liq={} fee={:.1}bps 0_to_1={} input={} output={}",
            &pool.pool_address[..8], state.sqrt_price_x64, state.liquidity, fee_bps, is_0_to_1, amount_in, output
        );

        Ok(LocalQuote {
            pool_address: pool.pool_address.clone(),
            dex: Dex::RaydiumClmm,
            input_mint: input_mint.to_string(),
            input_amount: amount_in,
            output_amount: output,
            fee_amount: (amount_in as f64 * fee_bps / 10_000.0) as u64,
            price_impact_bps: 0.0,
            reserves: (0, 0),
        })
    }

    /// Quote a PumpSwap swap (same mechanics as Raydium, different pool layout).
    async fn quote_pumpswap(
        &self,
        pool: &KnownPool,
        input_mint: &str,
        amount_in: u64,
    ) -> Result<LocalQuote> {
        let (token_vault, sol_vault) = pool
            .vault_addresses
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("PumpSwap pool missing vault addresses"))?;

        let accounts = self
            .get_multiple_accounts(&[token_vault.as_str(), sol_vault.as_str()])
            .await?;

        let token_data = accounts
            .get(token_vault)
            .ok_or_else(|| anyhow::anyhow!("Failed to fetch token vault"))?;
        let sol_data = accounts
            .get(sol_vault)
            .ok_or_else(|| anyhow::anyhow!("Failed to fetch SOL vault"))?;

        let token_balance = pool_decoder::decode_spl_token_balance(token_data)?;
        let sol_balance = pool_decoder::decode_spl_token_balance(sol_data)?;

        let (reserve_in, reserve_out) = if input_mint == WSOL_MINT || input_mint == pool.quote_mint {
            (sol_balance, token_balance)
        } else {
            (token_balance, sol_balance)
        };

        let output = pool_decoder::pumpswap_swap_output(reserve_in, reserve_out, amount_in);
        let fee = amount_in as u64 * 25 / 10_000;
        let impact_bps = if reserve_in > 0 {
            (amount_in as f64 / reserve_in as f64) * 10_000.0
        } else {
            0.0
        };

        Ok(LocalQuote {
            pool_address: pool.pool_address.clone(),
            dex: Dex::PumpSwap,
            input_mint: input_mint.to_string(),
            input_amount: amount_in,
            output_amount: output,
            fee_amount: fee,
            price_impact_bps: impact_bps,
            reserves: (reserve_in, reserve_out),
        })
    }

    /// Quote a PumpFun bonding curve buy/sell.
    async fn quote_pumpfun(
        &self,
        pool: &KnownPool,
        input_mint: &str,
        amount_in: u64,
    ) -> Result<LocalQuote> {
        // Fetch the bonding curve account directly
        let accounts = self
            .get_multiple_accounts(&[pool.pool_address.as_str()])
            .await?;

        let curve_data = accounts
            .get(&pool.pool_address)
            .ok_or_else(|| anyhow::anyhow!("Failed to fetch PumpFun bonding curve"))?;

        let state = pool_decoder::decode_pumpfun_state(curve_data)?;

        if state.complete {
            bail!("PumpFun curve is graduated (complete), use PumpSwap pool instead");
        }

        let (output, fee, reserves) = if input_mint == WSOL_MINT {
            // Buying: SOL in, tokens out
            let out = pool_decoder::pumpfun_buy_output(&state, amount_in);
            let fee = amount_in / 100; // 1% on input
            (out, fee, (state.virtual_sol_reserves, state.virtual_token_reserves))
        } else {
            // Selling: tokens in, SOL out
            let out = pool_decoder::pumpfun_sell_output(&state, amount_in);
            let fee = out / 100; // ~1% on output (already deducted)
            (out, fee, (state.virtual_token_reserves, state.virtual_sol_reserves))
        };

        let impact_bps = if reserves.0 > 0 {
            (amount_in as f64 / reserves.0 as f64) * 10_000.0
        } else {
            0.0
        };

        Ok(LocalQuote {
            pool_address: pool.pool_address.clone(),
            dex: Dex::PumpFun,
            input_mint: input_mint.to_string(),
            input_amount: amount_in,
            output_amount: output,
            fee_amount: fee,
            price_impact_bps: impact_bps,
            reserves,
        })
    }

    /// Quote an Orca Whirlpool swap using sqrt_price approximation.
    /// Fetches the Whirlpool account directly (pool_address = Whirlpool account).
    async fn quote_orca(
        &self,
        pool: &KnownPool,
        input_mint: &str,
        amount_in: u64,
    ) -> Result<LocalQuote> {
        let accounts = self
            .get_multiple_accounts(&[pool.pool_address.as_str()])
            .await?;

        let pool_data = accounts
            .get(&pool.pool_address)
            .ok_or_else(|| anyhow::anyhow!("Failed to fetch Whirlpool account"))?;

        let sqrt_price = pool_decoder::decode_whirlpool_sqrt_price(pool_data)?;
        let liquidity = pool_decoder::decode_whirlpool_liquidity(pool_data)?;
        let fee_bps = pool_decoder::decode_whirlpool_fee_bps(pool_data)?;

        // Determine swap direction based on Whirlpool token ordering
        // Whirlpool mint_a is the "first" token. If input is mint_a, it's a_to_b.
        let mint_a = pool_decoder::decode_whirlpool_mint_a(pool_data)?;
        let mint_a_str = bs58::encode(mint_a).into_string();
        let is_a_to_b = input_mint == mint_a_str;

        let output = pool_decoder::whirlpool_swap_output(
            sqrt_price, liquidity, amount_in, fee_bps, is_a_to_b,
        );

        let fee = (amount_in as f64 * fee_bps / 10_000.0) as u64;

        debug!(
            "ORCA LOCAL: pool={} sqrt_price={} liq={} fee={:.1}bps a_to_b={} input={} output={}",
            &pool.pool_address[..8], sqrt_price, liquidity, fee_bps, is_a_to_b, amount_in, output
        );

        Ok(LocalQuote {
            pool_address: pool.pool_address.clone(),
            dex: Dex::Orca,
            input_mint: input_mint.to_string(),
            input_amount: amount_in,
            output_amount: output,
            fee_amount: fee,
            price_impact_bps: 0.0, // hard to estimate for concentrated liquidity
            reserves: (0, 0),      // CL pools don't have simple reserves
        })
    }

    /// Batch-quote: given a token and all its known pools, fetch all reserves
    /// and compute local prices in as few RPC calls as possible.
    pub async fn quote_all_pools(
        &self,
        pools: &[KnownPool],
        input_mint: &str,
        amount_in: u64,
    ) -> Vec<Result<LocalQuote>> {
        // Collect all accounts we need to fetch
        let mut all_addresses: Vec<&str> = Vec::new();
        for pool in pools {
            match pool.dex {
                Dex::Raydium | Dex::PumpSwap => {
                    if let Some((a, b)) = &pool.vault_addresses {
                        all_addresses.push(a.as_str());
                        all_addresses.push(b.as_str());
                    }
                }
                Dex::PumpFun | Dex::Orca | Dex::RaydiumClmm => {
                    all_addresses.push(pool.pool_address.as_str());
                }
                _ => {}
            }
        }

        // Single batch RPC call for all accounts
        let accounts = match self.get_multiple_accounts(&all_addresses).await {
            Ok(a) => a,
            Err(e) => {
                warn!("Batch account fetch failed: {}", e);
                return pools.iter().map(|_| Err(anyhow::anyhow!("RPC batch failed"))).collect();
            }
        };

        // Now compute quotes using the cached account data
        let mut results = Vec::new();
        for pool in pools {
            let result = match pool.dex {
                Dex::Raydium => {
                    self.quote_raydium_from_cache(pool, input_mint, amount_in, &accounts)
                }
                Dex::PumpSwap => {
                    self.quote_pumpswap_from_cache(pool, input_mint, amount_in, &accounts)
                }
                Dex::PumpFun => {
                    self.quote_pumpfun_from_cache(pool, input_mint, amount_in, &accounts)
                }
                Dex::RaydiumClmm => {
                    self.quote_clmm_from_cache(pool, input_mint, amount_in, &accounts)
                }
                Dex::Orca => {
                    self.quote_orca_from_cache(pool, input_mint, amount_in, &accounts)
                }
                other => Err(anyhow::anyhow!("Unsupported DEX: {:?}", other)),
            };
            results.push(result);
        }
        results
    }

    fn quote_raydium_from_cache(
        &self,
        pool: &KnownPool,
        input_mint: &str,
        amount_in: u64,
        accounts: &HashMap<String, Vec<u8>>,
    ) -> Result<LocalQuote> {
        let (coin_vault, pc_vault) = pool
            .vault_addresses
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Missing vaults"))?;

        let coin_data = accounts.get(coin_vault)
            .ok_or_else(|| anyhow::anyhow!("Coin vault not in cache"))?;
        let pc_data = accounts.get(pc_vault)
            .ok_or_else(|| anyhow::anyhow!("PC vault not in cache"))?;

        let coin_balance = pool_decoder::decode_spl_token_balance(coin_data)?;
        let pc_balance = pool_decoder::decode_spl_token_balance(pc_data)?;

        let (reserve_in, reserve_out) = if input_mint == pool.quote_mint || input_mint == WSOL_MINT {
            (pc_balance, coin_balance)
        } else {
            (coin_balance, pc_balance)
        };

        let output = pool_decoder::raydium_swap_output(reserve_in, reserve_out, amount_in);

        debug!(
            "RAYDIUM LOCAL: pool={} reserves=({}, {}) input={} output={}",
            &pool.pool_address[..8], reserve_in, reserve_out, amount_in, output
        );

        Ok(LocalQuote {
            pool_address: pool.pool_address.clone(),
            dex: Dex::Raydium,
            input_mint: input_mint.to_string(),
            input_amount: amount_in,
            output_amount: output,
            fee_amount: amount_in * 25 / 10_000,
            price_impact_bps: (amount_in as f64 / reserve_in.max(1) as f64) * 10_000.0,
            reserves: (reserve_in, reserve_out),
        })
    }

    fn quote_pumpswap_from_cache(
        &self,
        pool: &KnownPool,
        input_mint: &str,
        amount_in: u64,
        accounts: &HashMap<String, Vec<u8>>,
    ) -> Result<LocalQuote> {
        let (token_vault, sol_vault) = pool
            .vault_addresses
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Missing vaults"))?;

        let token_data = accounts.get(token_vault)
            .ok_or_else(|| anyhow::anyhow!("Token vault not in cache"))?;
        let sol_data = accounts.get(sol_vault)
            .ok_or_else(|| anyhow::anyhow!("SOL vault not in cache"))?;

        let token_balance = pool_decoder::decode_spl_token_balance(token_data)?;
        let sol_balance = pool_decoder::decode_spl_token_balance(sol_data)?;

        let (reserve_in, reserve_out) = if input_mint == WSOL_MINT || input_mint == pool.quote_mint {
            (sol_balance, token_balance)
        } else {
            (token_balance, sol_balance)
        };

        let output = pool_decoder::pumpswap_swap_output(reserve_in, reserve_out, amount_in);

        debug!(
            "PUMPSWAP LOCAL: pool={} reserves=({}, {}) input={} output={}",
            &pool.pool_address[..8], reserve_in, reserve_out, amount_in, output
        );

        Ok(LocalQuote {
            pool_address: pool.pool_address.clone(),
            dex: Dex::PumpSwap,
            input_mint: input_mint.to_string(),
            input_amount: amount_in,
            output_amount: output,
            fee_amount: amount_in * 25 / 10_000,
            price_impact_bps: (amount_in as f64 / reserve_in.max(1) as f64) * 10_000.0,
            reserves: (reserve_in, reserve_out),
        })
    }

    fn quote_pumpfun_from_cache(
        &self,
        pool: &KnownPool,
        input_mint: &str,
        amount_in: u64,
        accounts: &HashMap<String, Vec<u8>>,
    ) -> Result<LocalQuote> {
        let curve_data = accounts.get(&pool.pool_address)
            .ok_or_else(|| anyhow::anyhow!("Bonding curve not in cache"))?;

        let state = pool_decoder::decode_pumpfun_state(curve_data)?;
        if state.complete {
            bail!("Curve graduated");
        }

        let (output, fee, reserves) = if input_mint == WSOL_MINT {
            let out = pool_decoder::pumpfun_buy_output(&state, amount_in);
            (out, amount_in / 100, (state.virtual_sol_reserves, state.virtual_token_reserves))
        } else {
            let out = pool_decoder::pumpfun_sell_output(&state, amount_in);
            (out, out / 100, (state.virtual_token_reserves, state.virtual_sol_reserves))
        };

        debug!(
            "PUMPFUN LOCAL: curve={} virt_reserves=({}, {}) input={} output={}",
            &pool.pool_address[..8], reserves.0, reserves.1, amount_in, output
        );

        Ok(LocalQuote {
            pool_address: pool.pool_address.clone(),
            dex: Dex::PumpFun,
            input_mint: input_mint.to_string(),
            input_amount: amount_in,
            output_amount: output,
            fee_amount: fee,
            price_impact_bps: (amount_in as f64 / reserves.0.max(1) as f64) * 10_000.0,
            reserves,
        })
    }

    fn quote_clmm_from_cache(
        &self,
        pool: &KnownPool,
        input_mint: &str,
        amount_in: u64,
        accounts: &HashMap<String, Vec<u8>>,
    ) -> Result<LocalQuote> {
        let pool_data = accounts.get(&pool.pool_address)
            .ok_or_else(|| anyhow::anyhow!("CLMM pool not in cache"))?;

        let state = pool_decoder::decode_raydium_clmm(pool_data)?;
        let mint_0_str = bs58::encode(state.mint_0).into_string();
        let is_0_to_1 = input_mint == mint_0_str;
        let fee_bps = pool.dex.fee_bps();

        let output = pool_decoder::clmm_swap_output(&state, amount_in, fee_bps, is_0_to_1);

        debug!(
            "CLMM LOCAL (cache): pool={} 0_to_1={} input={} output={}",
            &pool.pool_address[..8], is_0_to_1, amount_in, output
        );

        Ok(LocalQuote {
            pool_address: pool.pool_address.clone(),
            dex: Dex::RaydiumClmm,
            input_mint: input_mint.to_string(),
            input_amount: amount_in,
            output_amount: output,
            fee_amount: (amount_in as f64 * fee_bps / 10_000.0) as u64,
            price_impact_bps: 0.0,
            reserves: (0, 0),
        })
    }

    fn quote_orca_from_cache(
        &self,
        pool: &KnownPool,
        input_mint: &str,
        amount_in: u64,
        accounts: &HashMap<String, Vec<u8>>,
    ) -> Result<LocalQuote> {
        let pool_data = accounts.get(&pool.pool_address)
            .ok_or_else(|| anyhow::anyhow!("Whirlpool not in cache"))?;

        let sqrt_price = pool_decoder::decode_whirlpool_sqrt_price(pool_data)?;
        let liquidity = pool_decoder::decode_whirlpool_liquidity(pool_data)?;
        let fee_bps = pool_decoder::decode_whirlpool_fee_bps(pool_data)?;

        let mint_a = pool_decoder::decode_whirlpool_mint_a(pool_data)?;
        let mint_a_str = bs58::encode(mint_a).into_string();
        let is_a_to_b = input_mint == mint_a_str;

        let output = pool_decoder::whirlpool_swap_output(
            sqrt_price, liquidity, amount_in, fee_bps, is_a_to_b,
        );

        let fee = (amount_in as f64 * fee_bps / 10_000.0) as u64;

        debug!(
            "ORCA LOCAL (cache): pool={} fee={:.1}bps a_to_b={} input={} output={}",
            &pool.pool_address[..8], fee_bps, is_a_to_b, amount_in, output
        );

        Ok(LocalQuote {
            pool_address: pool.pool_address.clone(),
            dex: Dex::Orca,
            input_mint: input_mint.to_string(),
            input_amount: amount_in,
            output_amount: output,
            fee_amount: fee,
            price_impact_bps: 0.0,
            reserves: (0, 0),
        })
    }
}
