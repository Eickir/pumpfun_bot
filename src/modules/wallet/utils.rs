use crate::modules::wallet::constants::*;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;
use crate::modules::wallet::instructions::{Buy, Sell};
use solana_program::instruction::Instruction;
use spl_associated_token_account::get_associated_token_address;
use solana_program::instruction::AccountMeta;

#[inline(always)]
pub fn buy_instructions_arguments(
    virtual_sol_reserves: u64,
    virtual_token_reserves: u64,
    sol_to_invest: f64,
    slippage: f64,
) -> (u64, u64) {
    // Convert SOL to lamports (rounded efficiently)
    let sol_lamports = sol_to_invest.mul_add(LAMPORTS_PER_SOL as f64, 0.5) as u64;

    // Convert slippage percentage to basis points (1% = 100 basis points)
    let slippage_basis_points = slippage.mul_add(10000.0, 0.5) as u64;

    // Compute invariant product of reserves in u128 for precision
    let invariant: u128 = (virtual_sol_reserves as u128) * (virtual_token_reserves as u128);

    // Precompute new SOL reserves to avoid redundant calculations
    let new_virtual_sol_reserves: u128 = (virtual_sol_reserves as u128) + (sol_lamports as u128);

    // Compute new token reserves
    let new_token_reserves = (invariant / new_virtual_sol_reserves).wrapping_add(1);

    // Calculate the amount of tokens received (avoiding underflow)
    let token_amount = (virtual_token_reserves as u128).saturating_sub(new_token_reserves) as u64;

    // Calculate max SOL cost with slippage (efficient multiplication)
    let max_sol_cost = sol_lamports.saturating_add((sol_lamports * slippage_basis_points) >> 14); // Divide by 10000 using bit shift

    (token_amount, max_sol_cost)
}

#[inline(always)]
pub fn buy_instructions(
    payer: &Pubkey,
    mint: &Pubkey,
    bonding_curve: &Pubkey,
    creator: &Pubkey,
    args: Buy,
) -> Instruction {
    let associated_bonding_curve = get_associated_token_address(&bonding_curve, &mint);
    let associated_user = get_associated_token_address(&payer, &mint);

    // Calcul de la PDA creator_vault avec le creator (utilisateur)
    let (creator_vault, _) = Pubkey::find_program_address(
        &[b"creator-vault", creator.as_ref()],
        &PROGRAM_ID,
    );

    Instruction::new_with_bytes(
        PROGRAM_ID,
        &args.data(),
        vec![
            AccountMeta::new_readonly(GLOBAL, false),
            AccountMeta::new(FEE_RECIPIENT, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*bonding_curve, false),
            AccountMeta::new(associated_bonding_curve, false),
            AccountMeta::new(associated_user, false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new(creator_vault, false), // PDA calculée pour le creator_vault
            AccountMeta::new_readonly(EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
        ],
    )
}

#[inline(always)]
pub fn sell_instructions(
    payer: &Pubkey,
    mint: &Pubkey,
    bonding_curve: &Pubkey,
    creator: &Pubkey,
    args: Sell,
) -> Instruction {
    let associated_bonding_curve = get_associated_token_address(&bonding_curve, &mint);
    let associated_user = get_associated_token_address(&payer, &mint);

    let (creator_vault, _) = Pubkey::find_program_address(
        &[b"creator-vault", creator.as_ref()],
        &PROGRAM_ID,
    );

    Instruction::new_with_bytes(
        PROGRAM_ID,
        &args.data(),
        vec![
            AccountMeta::new_readonly(GLOBAL, false),
            AccountMeta::new(FEE_RECIPIENT, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*bonding_curve, false),
            AccountMeta::new(associated_bonding_curve, false),
            AccountMeta::new(associated_user, false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new(creator_vault, false), // PDA calculée pour le creator_vault
            AccountMeta::new_readonly(EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
        ]
    )
}