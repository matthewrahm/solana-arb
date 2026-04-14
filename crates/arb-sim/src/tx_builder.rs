//! Transaction builder: constructs Solana swap instructions for direct AMM interaction.
//! Each builder takes pool state + trade params and returns Instructions.

use anyhow::{bail, Result};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::str::FromStr;
use tracing::debug;

use arb_types::RAYDIUM_AMM_V4;

// ── Program IDs ──

const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

/// PumpSwap AMM program ID (graduated PumpFun tokens).
#[allow(dead_code)]
const PUMPSWAP_PROGRAM: &str = "PSwapMdSai8tjrEXcxFeQth87xC4rRsa4VA5mhGhXkP";

/// Raydium AMM V4 authority PDA seed.
const RAYDIUM_AUTHORITY_SEED: &[u8] = &[
    97, 109, 109, 32, 97, 117, 116, 104, 111, 114, 105, 116, 121, // "amm authority"
];

// ── Raydium V4 Pool Account Layout ──
// These offsets are for decoding additional fields beyond the vaults (already decoded in pool_decoder).

#[allow(dead_code)]
const RAYDIUM_STATUS_OFFSET: usize = 0x0; // skip discriminator region
const RAYDIUM_NONCE_OFFSET: usize = 0x04;
const RAYDIUM_OPEN_ORDERS_OFFSET: usize = 0x28;
const RAYDIUM_TARGET_ORDERS_OFFSET: usize = 0x48;
const RAYDIUM_COIN_VAULT_OFFSET: usize = 0x128;
const RAYDIUM_PC_VAULT_OFFSET: usize = 0x148;
const RAYDIUM_MARKET_PROGRAM_OFFSET: usize = 0x188;
const RAYDIUM_MARKET_OFFSET: usize = 0x1A8;

/// Decoded Raydium V4 pool state needed for building swap instructions.
#[derive(Debug, Clone)]
pub struct RaydiumPoolState {
    pub pool_address: Pubkey,
    pub nonce: u8,
    pub open_orders: Pubkey,
    pub target_orders: Pubkey,
    pub coin_vault: Pubkey,
    pub pc_vault: Pubkey,
    pub market_program: Pubkey,
    pub market: Pubkey,
    pub authority: Pubkey,
}

/// Decode the full Raydium V4 pool state needed for swap instructions.
pub fn decode_raydium_pool_state(pool_address: &str, data: &[u8]) -> Result<RaydiumPoolState> {
    if data.len() < 752 {
        bail!("Raydium pool too short: {} (need 752)", data.len());
    }

    let pool_pubkey = Pubkey::from_str(pool_address)?;
    let nonce = data[RAYDIUM_NONCE_OFFSET];

    let read_pubkey = |offset: usize| -> Pubkey {
        let bytes: [u8; 32] = data[offset..offset + 32].try_into().unwrap();
        Pubkey::new_from_array(bytes)
    };

    let open_orders = read_pubkey(RAYDIUM_OPEN_ORDERS_OFFSET);
    let target_orders = read_pubkey(RAYDIUM_TARGET_ORDERS_OFFSET);
    let coin_vault = read_pubkey(RAYDIUM_COIN_VAULT_OFFSET);
    let pc_vault = read_pubkey(RAYDIUM_PC_VAULT_OFFSET);
    let market_program = read_pubkey(RAYDIUM_MARKET_PROGRAM_OFFSET);
    let market = read_pubkey(RAYDIUM_MARKET_OFFSET);

    // Derive the AMM authority PDA
    let amm_program = Pubkey::from_str(RAYDIUM_AMM_V4)?;
    let authority = Pubkey::find_program_address(
        &[RAYDIUM_AUTHORITY_SEED, &[nonce]],
        &amm_program,
    ).0;

    Ok(RaydiumPoolState {
        pool_address: pool_pubkey,
        nonce,
        open_orders,
        target_orders,
        coin_vault,
        pc_vault,
        market_program,
        market,
        authority,
    })
}

/// Build a Raydium V4 swap_base_in instruction.
/// Instruction data: [9] (opcode) + amount_in(u64 LE) + minimum_amount_out(u64 LE)
pub fn build_raydium_swap(
    pool_state: &RaydiumPoolState,
    user_source_token: &Pubkey,
    user_dest_token: &Pubkey,
    user_owner: &Pubkey,
    amount_in: u64,
    minimum_out: u64,
) -> Result<Instruction> {
    let program_id = Pubkey::from_str(RAYDIUM_AMM_V4)?;
    let token_program = Pubkey::from_str(TOKEN_PROGRAM)?;

    // Instruction data: opcode 9 (swap_base_in) + amount_in + min_out
    let mut data = Vec::with_capacity(17);
    data.push(9u8); // swap_base_in opcode
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_out.to_le_bytes());

    let accounts = vec![
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new(pool_state.pool_address, false),
        AccountMeta::new_readonly(pool_state.authority, false),
        AccountMeta::new(pool_state.open_orders, false),
        AccountMeta::new(pool_state.target_orders, false),
        AccountMeta::new(pool_state.coin_vault, false),
        AccountMeta::new(pool_state.pc_vault, false),
        AccountMeta::new_readonly(pool_state.market_program, false),
        AccountMeta::new(pool_state.market, false),
        // Serum/OpenBook market accounts (bids, asks, event_queue, coin_vault, pc_vault, vault_signer)
        // These need to be fetched from the market account -- for now we'll resolve them at execution time
        AccountMeta::new(*user_source_token, false),
        AccountMeta::new(*user_dest_token, false),
        AccountMeta::new_readonly(*user_owner, true), // signer
    ];

    debug!(
        "Built Raydium swap: pool={} amount_in={} min_out={} accounts={}",
        &pool_state.pool_address.to_string()[..8],
        amount_in,
        minimum_out,
        accounts.len()
    );

    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}

/// Build a SOL transfer instruction for Jito tips.
pub fn build_tip_instruction(
    from: &Pubkey,
    tip_account: &Pubkey,
    tip_lamports: u64,
) -> Instruction {
    let system_program_id: Pubkey = "11111111111111111111111111111111".parse().unwrap();
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&2u32.to_le_bytes()); // Transfer instruction index
    data.extend_from_slice(&tip_lamports.to_le_bytes());

    Instruction {
        program_id: system_program_id,
        accounts: vec![
            AccountMeta::new(*from, true),
            AccountMeta::new(*tip_account, false),
        ],
        data,
    }
}

/// Derive the Associated Token Account (ATA) address for a wallet + mint.
pub fn derive_ata(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    let token_program = Pubkey::from_str(TOKEN_PROGRAM).unwrap();
    let ata_program = Pubkey::from_str(ASSOCIATED_TOKEN_PROGRAM).unwrap();
    Pubkey::find_program_address(
        &[
            wallet.as_ref(),
            token_program.as_ref(),
            mint.as_ref(),
        ],
        &ata_program,
    ).0
}

/// Compute minimum output with slippage tolerance.
/// slippage_bps: e.g., 100 = 1% slippage tolerance.
pub fn apply_slippage(expected_output: u64, slippage_bps: u64) -> u64 {
    let deduction = expected_output * slippage_bps / 10_000;
    expected_output.saturating_sub(deduction)
}
