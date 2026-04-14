//! Pure decoding functions for on-chain pool account data.
//! Each function takes raw account bytes and returns a decoded value.
//! Zero I/O — fully unit-testable.

use anyhow::{bail, ensure, Result};

// ── SPL Token Account ──

/// SPL Token account: 165 bytes. Balance (u64) at offset 64.
pub const SPL_TOKEN_ACCOUNT_LEN: usize = 165;
const SPL_TOKEN_AMOUNT_OFFSET: usize = 64;

pub fn decode_spl_token_balance(data: &[u8]) -> Result<u64> {
    ensure!(data.len() >= SPL_TOKEN_ACCOUNT_LEN, "SPL Token account too short: {}", data.len());
    Ok(u64::from_le_bytes(data[SPL_TOKEN_AMOUNT_OFFSET..SPL_TOKEN_AMOUNT_OFFSET + 8].try_into().unwrap()))
}

// ── Raydium AMM V4 ──

/// Raydium pool: 752 bytes. Vault pubkeys at 0x128 and 0x148.
pub const RAYDIUM_POOL_LEN: usize = 752;
const RAYDIUM_COIN_VAULT_OFFSET: usize = 0x128;
const RAYDIUM_PC_VAULT_OFFSET: usize = 0x148;

/// Extract coin_vault and pc_vault pubkeys from a Raydium AMM V4 pool account.
pub fn decode_raydium_pool_vaults(data: &[u8]) -> Result<([u8; 32], [u8; 32])> {
    ensure!(data.len() >= RAYDIUM_POOL_LEN, "Raydium pool too short: {}", data.len());
    let coin: [u8; 32] = data[RAYDIUM_COIN_VAULT_OFFSET..RAYDIUM_COIN_VAULT_OFFSET + 32].try_into().unwrap();
    let pc: [u8; 32] = data[RAYDIUM_PC_VAULT_OFFSET..RAYDIUM_PC_VAULT_OFFSET + 32].try_into().unwrap();
    Ok((coin, pc))
}

/// Compute Raydium price from vault balances.
/// Returns price of base token in quote token units.
pub fn raydium_price_from_vaults(coin_balance: u64, pc_balance: u64, coin_decimals: u8, pc_decimals: u8) -> Result<f64> {
    if coin_balance == 0 {
        bail!("Raydium coin vault is empty");
    }
    let coin_adj = coin_balance as f64 / 10f64.powi(coin_decimals as i32);
    let pc_adj = pc_balance as f64 / 10f64.powi(pc_decimals as i32);
    Ok(pc_adj / coin_adj)
}

// ── Orca Whirlpool ──

/// Whirlpool account layout (Anchor, borsh-packed):
///   8 (discriminator) + 32 (config) + 1 (bump) + 2 (tick_spacing) + 2 (seed) +
///   2 (fee_rate) + 2 (protocol_fee_rate) + 16 (liquidity) = 65, then u128 sqrt_price
/// Validated empirically against live mainnet data.
const WHIRLPOOL_SQRT_PRICE_OFFSET: usize = 65;

/// Decode raw sqrt_price from an Orca Whirlpool account.
/// Returns the raw price (price_a_in_b) WITHOUT decimal adjustment.
/// Caller must handle decimals and inversion based on token ordering.
pub fn decode_whirlpool_sqrt_price(data: &[u8]) -> Result<u128> {
    ensure!(data.len() >= WHIRLPOOL_SQRT_PRICE_OFFSET + 16, "Whirlpool too short: {}", data.len());
    let sp = u128::from_le_bytes(data[WHIRLPOOL_SQRT_PRICE_OFFSET..WHIRLPOOL_SQRT_PRICE_OFFSET + 16].try_into().unwrap());
    if sp == 0 { bail!("Whirlpool sqrt_price is zero"); }
    Ok(sp)
}

/// Convert sqrt_price to a price with decimal adjustment.
pub fn sqrt_price_to_price(sqrt_price: u128, decimals_a: u8, decimals_b: u8) -> f64 {
    let sqrt_f = sqrt_price as f64 / (1u128 << 64) as f64;
    let raw_price = sqrt_f * sqrt_f;
    let decimal_adj = 10f64.powi(decimals_a as i32 - decimals_b as i32);
    raw_price * decimal_adj
}

/// Decode price from an Orca Whirlpool account.
/// price_a_in_b = (sqrt_price / 2^64)^2, adjusted for decimals.
pub fn decode_whirlpool_price(data: &[u8], decimals_a: u8, decimals_b: u8) -> Result<f64> {
    let sp = decode_whirlpool_sqrt_price(data)?;
    Ok(sqrt_price_to_price(sp, decimals_a, decimals_b))
}

/// Extract mint_a pubkey (32 bytes) from Whirlpool account.
/// mint_a is at offset 65 + 16 (sqrt_price) + 4 (tick) + 8 + 8 = 101
/// Actually: after sqrt_price(u128@65), tick_current_index(i32@81),
/// protocol_fee_owed_a(u64@85), protocol_fee_owed_b(u64@93), then mint_a(Pubkey@101)
const WHIRLPOOL_MINT_A_OFFSET: usize = 101;

pub fn decode_whirlpool_mint_a(data: &[u8]) -> Result<[u8; 32]> {
    ensure!(data.len() >= WHIRLPOOL_MINT_A_OFFSET + 32, "Whirlpool too short for mint_a");
    Ok(data[WHIRLPOOL_MINT_A_OFFSET..WHIRLPOOL_MINT_A_OFFSET + 32].try_into().unwrap())
}

// ── Meteora DAMM v2 ──

/// Meteora pool: sqrt_price_x64 (u128) at offset 0x108.
const METEORA_SQRT_PRICE_OFFSET: usize = 0x108;

/// Decode price from a Meteora DAMM v2 pool account.
/// Same formula as Orca: price = (sqrt_price / 2^64)^2.
pub fn decode_meteora_price(data: &[u8], decimals_a: u8, decimals_b: u8) -> Result<f64> {
    ensure!(data.len() >= METEORA_SQRT_PRICE_OFFSET + 16, "Meteora pool too short: {}", data.len());
    let sqrt_price = u128::from_le_bytes(data[METEORA_SQRT_PRICE_OFFSET..METEORA_SQRT_PRICE_OFFSET + 16].try_into().unwrap());
    if sqrt_price == 0 {
        bail!("Meteora sqrt_price is zero");
    }
    let sqrt_f = sqrt_price as f64 / (1u128 << 64) as f64;
    let raw_price = sqrt_f * sqrt_f;
    let decimal_adj = 10f64.powi(decimals_a as i32 - decimals_b as i32);
    Ok(raw_price * decimal_adj)
}

// ── PumpFun Bonding Curve ──

/// PumpFun: 8-byte discriminator, then sequential u64 fields.
const PUMPFUN_VIRTUAL_TOKEN_OFFSET: usize = 8;
const PUMPFUN_VIRTUAL_SOL_OFFSET: usize = 16;
const PUMPFUN_REAL_TOKEN_OFFSET: usize = 24;
const PUMPFUN_REAL_SOL_OFFSET: usize = 32;
const PUMPFUN_COMPLETE_OFFSET: usize = 48;
const PUMPFUN_MIN_LEN: usize = 49;

#[derive(Debug, Clone)]
pub struct PumpFunState {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub complete: bool,
}

pub fn decode_pumpfun_state(data: &[u8]) -> Result<PumpFunState> {
    ensure!(data.len() >= PUMPFUN_MIN_LEN, "PumpFun too short: {} (need {})", data.len(), PUMPFUN_MIN_LEN);
    let r = |off: usize| u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
    Ok(PumpFunState {
        virtual_token_reserves: r(PUMPFUN_VIRTUAL_TOKEN_OFFSET),
        virtual_sol_reserves: r(PUMPFUN_VIRTUAL_SOL_OFFSET),
        real_token_reserves: r(PUMPFUN_REAL_TOKEN_OFFSET),
        real_sol_reserves: r(PUMPFUN_REAL_SOL_OFFSET),
        complete: data[PUMPFUN_COMPLETE_OFFSET] != 0,
    })
}

/// Price of token in SOL. PumpFun tokens: 6 decimals, SOL: 9 decimals.
pub fn pumpfun_price_in_sol(state: &PumpFunState) -> Result<f64> {
    if state.virtual_token_reserves == 0 {
        bail!("PumpFun virtual_token_reserves is zero");
    }
    let sol = state.virtual_sol_reserves as f64 / 1e9;
    let tokens = state.virtual_token_reserves as f64 / 1e6;
    Ok(sol / tokens)
}

// ── Swap Output Computation ──
// All functions use integer math to avoid floating-point rounding issues.
// Constant-product formula: output = (input * (10000 - fee_bps) * reserve_out) / (reserve_in * 10000 + input * (10000 - fee_bps))

/// Raydium V4 swap output (25 bps fee).
/// Given reserves and input amount, returns the output amount after fees.
pub fn raydium_swap_output(reserve_in: u64, reserve_out: u64, amount_in: u64) -> u64 {
    constant_product_swap(reserve_in, reserve_out, amount_in, 25)
}

/// PumpSwap swap output (25 bps fee). Same constant-product as Raydium.
pub fn pumpswap_swap_output(reserve_in: u64, reserve_out: u64, amount_in: u64) -> u64 {
    constant_product_swap(reserve_in, reserve_out, amount_in, 25)
}

/// PumpFun bonding curve buy: SOL in, tokens out (1% fee on SOL input).
/// Uses virtual reserves for the constant-product calculation.
pub fn pumpfun_buy_output(state: &PumpFunState, sol_in_lamports: u64) -> u64 {
    if state.virtual_sol_reserves == 0 || state.virtual_token_reserves == 0 {
        return 0;
    }
    // PumpFun charges 1% fee on the input SOL
    let fee = sol_in_lamports / 100;
    let net_sol = sol_in_lamports.saturating_sub(fee);
    // Constant-product against virtual reserves (no additional AMM fee)
    constant_product_swap(
        state.virtual_sol_reserves,
        state.virtual_token_reserves,
        net_sol,
        0, // fee already deducted
    )
}

/// PumpFun bonding curve sell: tokens in, SOL out (1% fee on SOL output).
/// Uses virtual reserves for the constant-product calculation.
pub fn pumpfun_sell_output(state: &PumpFunState, tokens_in: u64) -> u64 {
    if state.virtual_sol_reserves == 0 || state.virtual_token_reserves == 0 {
        return 0;
    }
    // Constant-product against virtual reserves
    let gross_sol = constant_product_swap(
        state.virtual_token_reserves,
        state.virtual_sol_reserves,
        tokens_in,
        0,
    );
    // PumpFun charges 1% fee on the SOL output
    let fee = gross_sol / 100;
    gross_sol.saturating_sub(fee)
}

/// Generic constant-product AMM swap: x * y = k
/// fee_bps is deducted from the input before computing output.
fn constant_product_swap(reserve_in: u64, reserve_out: u64, amount_in: u64, fee_bps: u64) -> u64 {
    if reserve_in == 0 || reserve_out == 0 || amount_in == 0 {
        return 0;
    }
    // Use u128 to prevent overflow on large reserves
    let amount_in = amount_in as u128;
    let reserve_in = reserve_in as u128;
    let reserve_out = reserve_out as u128;
    let fee_factor = 10_000u128 - fee_bps as u128;

    let numerator = amount_in * fee_factor * reserve_out;
    let denominator = reserve_in * 10_000u128 + amount_in * fee_factor;

    if denominator == 0 {
        return 0;
    }

    (numerator / denominator) as u64
}

// ── SPL Mint Account ──

/// SPL Mint account: decimals (u8) at offset 44.
pub fn decode_mint_decimals(data: &[u8]) -> Result<u8> {
    ensure!(data.len() >= 45, "Mint account too short: {}", data.len());
    Ok(data[44])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spl_token_balance() {
        let mut data = vec![0u8; 165];
        let amount: u64 = 1_000_000_000;
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        assert_eq!(decode_spl_token_balance(&data).unwrap(), 1_000_000_000);
    }

    #[test]
    fn test_spl_token_too_short() {
        let data = vec![0u8; 100];
        assert!(decode_spl_token_balance(&data).is_err());
    }

    #[test]
    fn test_raydium_price() {
        // 1,000,000 tokens (5 decimals) and 1,000 SOL (9 decimals)
        let coin: u64 = 1_000_000 * 100_000;
        let pc: u64 = 1_000 * 1_000_000_000;
        let price = raydium_price_from_vaults(coin, pc, 5, 9).unwrap();
        assert!((price - 0.001).abs() < 1e-9);
    }

    #[test]
    fn test_raydium_empty_vault() {
        assert!(raydium_price_from_vaults(0, 1000, 5, 9).is_err());
    }

    #[test]
    fn test_whirlpool_price_unity() {
        let sqrt_price: u128 = 1u128 << 64;
        let mut data = vec![0u8; 256];
        data[65..81].copy_from_slice(&sqrt_price.to_le_bytes());
        let price = decode_whirlpool_price(&data, 6, 6).unwrap();
        assert!((price - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_whirlpool_price_with_decimals() {
        let sqrt_price: u128 = 1u128 << 64;
        let mut data = vec![0u8; 256];
        data[65..81].copy_from_slice(&sqrt_price.to_le_bytes());
        let price = decode_whirlpool_price(&data, 5, 9).unwrap();
        assert!((price - 0.0001).abs() < 1e-12);
    }

    #[test]
    fn test_meteora_price() {
        let sqrt_price: u128 = 1u128 << 64;
        let mut data = vec![0u8; 300];
        data[0x108..0x118].copy_from_slice(&sqrt_price.to_le_bytes());
        let price = decode_meteora_price(&data, 6, 6).unwrap();
        assert!((price - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_pumpfun_state() {
        let mut data = vec![0u8; 64];
        let vt: u64 = 1_000_000_000_000;
        let vs: u64 = 30_000_000_000;
        data[8..16].copy_from_slice(&vt.to_le_bytes());
        data[16..24].copy_from_slice(&vs.to_le_bytes());
        data[48] = 0;

        let state = decode_pumpfun_state(&data).unwrap();
        assert_eq!(state.virtual_token_reserves, vt);
        assert_eq!(state.virtual_sol_reserves, vs);
        assert!(!state.complete);
    }

    #[test]
    fn test_pumpfun_graduation() {
        let mut data = vec![0u8; 64];
        data[48] = 1;
        let state = decode_pumpfun_state(&data).unwrap();
        assert!(state.complete);
    }

    #[test]
    fn test_pumpfun_price() {
        let state = PumpFunState {
            virtual_token_reserves: 1_000_000 * 1_000_000, // 1M tokens (6 dec)
            virtual_sol_reserves: 30 * 1_000_000_000,       // 30 SOL (9 dec)
            real_token_reserves: 0,
            real_sol_reserves: 0,
            complete: false,
        };
        let price = pumpfun_price_in_sol(&state).unwrap();
        // 30 SOL / 1M tokens = 0.00003 SOL/token
        assert!((price - 0.00003).abs() < 1e-10);
    }

    #[test]
    fn test_mint_decimals() {
        let mut data = vec![0u8; 82];
        data[44] = 9; // SOL decimals
        assert_eq!(decode_mint_decimals(&data).unwrap(), 9);
    }

    // ── Swap Output Tests ──

    #[test]
    fn test_constant_product_basic() {
        // Pool: 1000 SOL, 1_000_000 tokens. Swap 1 SOL in.
        // Expected: ~999 tokens out (minus 25bps fee + price impact)
        let out = raydium_swap_output(
            1_000 * 1_000_000_000,   // 1000 SOL reserve_in
            1_000_000 * 1_000_000,   // 1M tokens reserve_out (6 dec)
            1_000_000_000,           // 1 SOL in
        );
        // Without fee: 1M * 1 / (1000 + 1) = ~999 tokens = 999_000_999
        // With 25bps fee on input: slightly less
        assert!(out > 990_000_000, "got {} tokens", out);
        assert!(out < 1_000_000_000, "got {} tokens", out);
    }

    #[test]
    fn test_constant_product_zero_input() {
        assert_eq!(raydium_swap_output(1000, 1000, 0), 0);
    }

    #[test]
    fn test_constant_product_zero_reserves() {
        assert_eq!(raydium_swap_output(0, 1000, 100), 0);
        assert_eq!(raydium_swap_output(1000, 0, 100), 0);
    }

    #[test]
    fn test_pumpswap_same_as_raydium() {
        // Same fee structure, same formula
        let r = raydium_swap_output(1000, 2000, 100);
        let p = pumpswap_swap_output(1000, 2000, 100);
        assert_eq!(r, p);
    }

    #[test]
    fn test_pumpfun_buy() {
        // Virtual reserves: 30 SOL, 1B tokens (typical initial PumpFun state)
        let state = PumpFunState {
            virtual_token_reserves: 1_000_000_000 * 1_000_000, // 1B tokens (6 dec)
            virtual_sol_reserves: 30 * 1_000_000_000,          // 30 SOL
            real_token_reserves: 0,
            real_sol_reserves: 0,
            complete: false,
        };
        // Buy with 1 SOL
        let tokens_out = pumpfun_buy_output(&state, 1_000_000_000);
        // After 1% fee: 0.99 SOL effective input
        // Output ~= 1B * 0.99 / (30 + 0.99) ~= 31.9M tokens
        assert!(tokens_out > 30_000_000 * 1_000_000, "got {}", tokens_out);
        assert!(tokens_out < 35_000_000 * 1_000_000, "got {}", tokens_out);
    }

    #[test]
    fn test_pumpfun_sell() {
        let state = PumpFunState {
            virtual_token_reserves: 1_000_000_000 * 1_000_000,
            virtual_sol_reserves: 30 * 1_000_000_000,
            real_token_reserves: 0,
            real_sol_reserves: 0,
            complete: false,
        };
        // Sell 32M tokens (roughly what 1 SOL buy would give)
        let sol_out = pumpfun_sell_output(&state, 32_000_000 * 1_000_000);
        // Should get roughly ~0.94 SOL back (some slippage + 1% fee on output)
        assert!(sol_out > 800_000_000, "got {} lamports", sol_out);
        assert!(sol_out < 1_000_000_000, "got {} lamports", sol_out);
    }

    #[test]
    fn test_pumpfun_zero_reserves() {
        let state = PumpFunState {
            virtual_token_reserves: 0,
            virtual_sol_reserves: 0,
            real_token_reserves: 0,
            real_sol_reserves: 0,
            complete: false,
        };
        assert_eq!(pumpfun_buy_output(&state, 1_000_000_000), 0);
        assert_eq!(pumpfun_sell_output(&state, 1_000_000), 0);
    }

    #[test]
    fn test_large_swap_no_overflow() {
        // Large reserves: 100K SOL, 100B tokens
        let out = raydium_swap_output(
            100_000 * 1_000_000_000,
            100_000_000_000 * 1_000_000,
            10_000 * 1_000_000_000, // 10K SOL swap
        );
        assert!(out > 0, "should not overflow");
    }
}
