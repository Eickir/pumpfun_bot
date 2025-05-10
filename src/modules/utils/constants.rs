use once_cell::sync::Lazy;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashSet;
use std::str::FromStr;

pub static ALLOWED_PROGRAM_IDS: Lazy<HashSet<Pubkey>> = Lazy::new(|| {
    [
        "ComputeBudget111111111111111111111111111111",
        "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",  // Pumpfun
        "11111111111111111111111111111111",            // Native loader
        "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL", // Associated Token
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", // SPL Token
    ]
    .iter()
    .filter_map(|s| Pubkey::from_str(s).ok())
    .collect()
});

/// Ton wallet pour filtrer les trades liés à toi-même
pub static MY_BOT_WALLET_PUBKEY: Lazy<Pubkey> = Lazy::new(|| {
    Pubkey::new_from_array([0u8; 32]) // Remplace ça par ton vrai pubkey
});
